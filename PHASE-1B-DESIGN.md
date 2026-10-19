# Phase 1b — versioned (MVCC) storage engine

**Status:** proposal, for review. No code written.
**Roadmap scope:** `ROADMAP.md` Phase 1b. Its definition of done: *a long read continues on
its version while a concurrent write commits a new one; readers never see a torn state;
retired versions are reclaimed; churn is measured and bounded.*

---

## 1. What exists today

| Thing | Where | Relevant fact |
|---|---|---|
| Trie | `src/storage/sparse_store.rs` | `SparseStore { root: Node, dim_count }`, `Node { children: HashMap<u32, Node>, value: Option<CellValue> }` |
| Mutability | `SparseStore::write(&mut self, …)` | walks the path and mutates nodes in place |
| Cube | `src/cube/cube.rs` | owns `store: SparseStore` and `query_cache: HashMap<Vec<u32>, f64>` |
| Reads | `query_slice(&self)` | already snapshot-friendly |
| Reads that mutate | `query_consolidated(&mut self)` | takes `&mut` **only** to populate the cache |
| Cache invalidation | 4 × `self.query_cache.clear()` | global, cleared on every write |
| Server | `src/bin/serve.rs` | one `Arc<Mutex<Catalog>>` for the whole process: readers and writers serialise |
| Persistence | `Catalog::save_to_disk(&self)` | bincode of a whole-catalog snapshot, magic + `FORMAT_VERSION = 1` |
| Dimension ids | `src/dimension/dimension.rs` | `next_id` only ever increments; deleted ids are never reused |

So the engine already has the *durability* snapshot (1a). What it lacks is a **readable
version that survives a concurrent write** — the server's single `Mutex` is the honest
statement of that.

## 2. What a "version" is here

The Infor model versions **pages**. The natural unit here is a **trie node**, because the
trie is already a persistent structure in spirit: an unchanged subtree is shared.

```
Version = { id: u64, root: Arc<Node> }
Node    = { children: HashMap<u32, Arc<Node>>, value: Option<CellValue> }
```

A write copies **only the nodes on the path** to the coordinate, sharing every untouched
subtree with the previous version. Cost is O(depth), not O(cells).

Because the sharing unit is an `Arc`, **reclamation is the refcount**: a version is
retired when `current` moves past it and no reader still holds its `Arc`. No mark-and-sweep
pass, no version table to prune. "Retired versions are reclaimed" becomes a property of
the type system rather than a GC task — provided nothing keeps a snapshot alive longer
than it should.

## 3. The two options the roadmap asks us to choose

**A — version model *as* the storage engine (recommended).** `Arc<Node>` children, writes
return a new version. Matches the Infor model most faithfully and makes sharing intrinsic.

**B — a versioned layer above the mutable store.** Keep `SparseStore` mutable; snapshot by
cloning it. Simple, reversible, **but the clone is O(cells) per version**, which is exactly
the cost MVCC exists to avoid. B is only viable if snapshots are rare (one per request),
in which case it is not MVCC but copy-on-read.

**Recommendation: A.** B is retained as the fallback if §7's measurements show path-copy
churn is worse than a bulk clone at this engine's typical depth (dimensions 3–4, so paths
are short — the measurement should favour A clearly).

## 4. The dimension problem — the part the roadmap does not mention

Cells are not the only mutable state. `Cube::write` calls `dim.add_leaf(member)`, mutating a
dimension that is shared (`Arc<RwLock<Dimension>>`) across cubes **while other readers may
be pinned to an older version**. A versioned store with unversioned dimensions is not a
consistent snapshot.

Split the two kinds of dimension change:

| Change | Frequency | Treatment |
|---|---|---|
| `add_leaf` (new member, new id) | every write that introduces a member | ids never reused, so an old version simply has no cells for the new id. A pinned reader that resolves the NEW name finds no data — benign, not torn. |
| structural: `.rollup`, `.detach`, `.delete_member`, `.order` | rare, admin-time | takes the exclusive write lock, and **bumps the version**. Aggregation semantics change, so a pinned reader must not observe it. |

Structural changes clone the affected `Dimension` (`Arc::make_mut`) and publish it with the
new version. That keeps the invariant "a version is (store root, dimension state)" without
versioning every member id. Cloning a dimension is O(members) — acceptable because these
operations are rare, and it is the one place where Option B's cost is the right trade.

**Deferred (not in scope):** versioning each dimension node so structural changes are also
copy-on-write. If dimension size ever makes the clone hurt, this is the next step.

## 5. API sketch

```rust
// src/storage/version.rs
pub struct Version {
    pub id: u64,
    pub root: Arc<Node>,
    /// Per-version aggregate cache. Dies with the version; nothing global to clear.
    pub cache: RwLock<HashMap<Vec<u32>, f64>>,
}

pub struct VersionStore {
    published: RwLock<Arc<Version>>,   // readers take a cheap Arc clone
    next_id: AtomicU64,
    write_lock: Mutex<()>,             // serialises writers; readers never take it
}

impl VersionStore {
    /// Cheap: one Arc clone. The reader's snapshot is stable for its lifetime.
    pub fn snapshot(&self) -> Arc<Version>;
    /// Copy-on-write from the latest published version; publishes the new one.
    pub fn write(&self, coords: &[u32], value: CellValue) -> Result<Arc<Version>, String>;
}
```

Query paths change from `&mut self` to `&self` + a snapshot: `query_consolidated` stops
needing `&mut` once the cache lives in the version, which also removes four
`query_cache.clear()` calls and the "correct only under single-writer" caveat in
`ROADMAP.md`.

Serialisation: `Arc<Node>` needs serde's `rc` feature (one flag on the existing `serde`
dependency — no new crate). The **on-disk shape does not change**, so `FORMAT_VERSION`
stays 1 and Phase 1a files keep loading. Versions themselves are never persisted; a save
writes the current root.

## 6. Server impact

`src/bin/serve.rs` replaces `Arc<Mutex<Catalog>>` with `Arc<Catalog>` where each cube holds a
`VersionStore`:

- `GET` routes take `snapshot()` and never block.
- `POST /v1/{ds}/query` likewise; it no longer needs `&mut Catalog`.
- Writes (none exposed over HTTP yet — there is no write endpoint) take the write lock.
- The `Mutex` around the catalog goes away entirely, which is the point of the phase.

This is also where the "one coarse lock" complaint in `ROADMAP.md`'s baseline section is
actually fixed.

## 7. Measurement — the roadmap's gate, before committing

The roadmap is explicit: *copy-on-write has real memory-churn cost; this must be measured,
not assumed.* `src/bin/bench_mvcc.rs` (a counting `GlobalAlloc`, no new dependency) measures
the CURRENT store. Baseline, release build, this machine:

| Case | bytes / write | allocs / write | µs / write |
|---|---:|---:|---:|
| 3 dims, overwrite an existing cell | **0** | 0.00 | 0.08 |
| 3 dims, write a new leaf | 408 | 0.95 | 0.27 |
| 4 dims, overwrite an existing cell | **0** | 0.00 | 0.21 |
| 4 dims, write a new leaf | 837 | 1.80 | 0.50 |
| 1 dim, rewrite a 512-deep path | 0 | 0.00 | 16.0 |
| Option-B snapshot (clone) @7,538 cells | 1,204,048 | 1,628 | 692 |
| Option-B snapshot (clone) @19,486 cells | 5,556,104 | 11,926 | 3,579 |

Scan throughput (the number a pinned reader must not lose): **50.3 ns/cell** at 3 dims
(7,538 cells), **77.6 ns/cell** at 4 dims (19,486 cells).

### What the baseline says

1. **An in-place overwrite allocates nothing.** "Within 2× of the overwrite baseline" is
   therefore meaningless — 2 × 0 = 0. The fair yardstick for a copy-on-write *overwrite* is
   today's **new-leaf write**, because both allocate one path.
2. **Option B is settled by the numbers, not by argument.** One clone per query is
   1.2–5.6 MB and 0.7–3.6 ms at only 8–20k cells, and every in-flight request would hold its
   own copy.
3. **Deep paths cost walking, not allocation.** A 512-deep rewrite is 16 µs with zero
   allocation today; path copying adds 512 node copies, so that is where CoW can really hurt.

### The gate (derived from the baseline, not guessed)

| Metric | Threshold | Rationale |
|---|---|---|
| CoW overwrite, bytes/write | ≤ 2 × today's new-leaf: **≤ 816 B** @3 dims, **≤ 1,674 B** @4 dims | a CoW overwrite does the allocation work of a new-leaf write |
| CoW overwrite, time | ≤ 2 × new-leaf: **≤ 0.54 µs** @3 dims, **≤ 1.00 µs** @4 dims | same |
| `snapshot()` | **O(1)**: ≤ 64 B and ≤ 100 ns | an `Arc` clone; anything near the clone column fails the phase |
| Pinned scan while writers commit | **≤ 10% slower**: ≤ 55.3 ns/cell @3 dims, ≤ 85.4 @4 dims | the actual promise of 1b |
| 512-deep path rewrite | ≤ 5 × baseline ≈ **≤ 80 µs** | worst case for path copying; report, do not silently accept |
| Retention | **< 2 KB per additional pinned version** on the 4-dim set | sharing is the whole point; O(cells) per version means it is not MVCC |

If the overwrite numbers fail, fall back to Option B for the data path and keep the
versioned API. If only the deep-path case fails, add a depth threshold above which a
subtree is cloned wholesale.


## 8. Milestones

| # | Slice | Proof |
|---|---|---|
| M0 | `bench_mvcc` baseline on the current store | **done** — the numbers in §7 |
| M1 | Store becomes persistent: `Arc<Node>` children, path-copy `write`; `SparseStore`'s public API unchanged | the 14 existing store tests pass unchanged, plus new ones: a snapshot is unaffected by later writes, and untouched subtrees are shared rather than copied |
| M1b | the §7 gate, measured on the new store | go / no-go before anything else moves |
| M2 | `Version` / `VersionStore` in `Cube`; read paths take a snapshot; per-version cache replaces the four `clear()` calls | reader isolation at the cube level; a cache hit on a stable version, and a miss after a write with no global invalidation |
| M3 | server drops the catalog `Mutex` | HTTP: concurrent readers plus a writer, no deadlock, consistent answers |
| M4 | structural dimension changes clone the dimension and bump the version | a `.rollup` during a pinned scan does not change that reader's numbers |

Each milestone leaves the suite green. M1 and M1b are internal and reversible; M2 changes
`Cube`'s API, so it lands only once the gate has passed. (The earlier draft put
`Version`/`snapshot()` in M1, but with writes still in place a snapshot cannot be isolated —
that is M2's substance, and claiming it in M1 would have been a test that could not pass.)


## 9. Risks and open questions

1. **Per-node `Arc` overhead.** Every node gains an 8-byte pointer plus refcount traffic.
   Acceptable at 3–4 dimensions; measured in M4.
2. **Deep hierarchies.** Path copy is O(depth); a 20-level ragged hierarchy copies 20 nodes
   per write. Ragged `import_lvl` trees are the realistic worst case — include one in the bench.
3. **Long-pinned readers** hold memory. Needs an operational story (a max version age, or a
   warning when a snapshot outlives N commits) — deferred, but name it.
4. **`delete_member` + pinned readers.** A pinned version keeps cells whose member id has
   since been deleted; those cells are unreachable by name but still scanned. Correct, slightly
   wasteful; document it.
5. **`write_splashed`** touches many leaves in one logical write. It becomes many path copies
   or one CoW rebuild — needs its own measurement, since it is the worst case for churn.
6. **Save while writing.** `save_to_disk(&self)` must snapshot once and serialise that
   snapshot, or a concurrent write could produce a file that never existed.

## 10. What I would do first

M1 + M4's harness together: introduce `Version`/`snapshot` without changing the write path,
and write the allocator-counting bench against the **current** store to establish the
baseline numbers. That is small, reversible, and gives the gate something to compare
against before any storage rewrite is on the table.
