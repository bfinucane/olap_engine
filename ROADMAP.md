# Roadmap

This document lays out the intended direction of the Patricia OLAP Engine. The
guiding principle is **inside-out**: we harden the core (architecture,
concurrency, persistence) *before* layering user-facing features on top. New
capabilities should sit on a foundation that is already safe to build on.

The ordering below is **dependency-driven**, not priority-by-appeal. Each phase
assumes the previous one has landed, because later work (security, calculations,
query languages) is far cheaper — and sometimes only possible — once the
earlier structural work exists.

---

## Guiding principles

1. **Inside-out.** Fix the core before adding features. A feature that lands on
   a shaky foundation has to be rewritten when the foundation changes.
2. **One source of truth.** The catalog is the unit of consistency. Any state
   that must stay coherent (dimensions + cubes + caches) is governed by a single
   concurrency and persistence model.
3. **Explicit failure.** No `expect()`/`unwrap()` on I/O or persistence paths in
   the steady state; errors surface as typed results, not panics.
4. **Smallest reversible slice.** Prefer a thin, working vertical slice over a
   speculative framework. Land it, verify it, then widen it.

---

## Where we are today (baseline / constraints)

These are the concrete facts the plan is built around — not aspirational.

* **Single-process, in-memory.** `Catalog` lives in one process. The interactive
  REPL owns it; batch mode builds a fresh one per run.
* **Whole-file, all-or-nothing persistence.** `save_to_disk` /
  `load_from_disk` serialize the *entire* catalog with `bincode`. Saves use
  `expect(...)` (panic on failure) and rewrite the whole file on `.save` /
  `.exit`.
* **Coarse and uneven concurrency.** `Dimension` is shared via
  `Arc<RwLock<Dimension>>`, but `Cube` is owned **by value** inside
  `Catalog` (`cubes: HashMap<String, Cube>`), and `query_slice` / `write` take
  `&mut self` on the cube. In practice there is **no safe multi-threaded
  mutation** today.
* **Cache invalidation is global and ad hoc.** `query_cache` is cleared on every
  write (`self.query_cache.clear()`). This is correct only under strictly
  single-threaded, single-writer assumptions.
* **No users, sessions, or access control.** Everything runs as one implicit
  user.
* **SQL is the only query surface.** No calculated members and no MDX.

---

## Phase 1 — Client/Server architecture + transaction safety

**Why first.** This is the biggest structural gap, and Phases 2–4 all assume a
session/concurrency model that does not exist yet. It is cheap now and expensive
to retrofit later.

Phase 1 is deliberately split into **1a (durability)** and **1b (the version /
MVCC store)**, because they are different in kind: 1a is a small, contained
change that makes today's single-writer world safe; 1b is a storage-engine
redesign that delivers concurrency. 1a is correct regardless of what 1b becomes,
so it lands first and is never wasted work.

### Phase 1a — Durable persistence (small, do first)

**Goal.** Make `.save`/`.exit` crash-safe and non-destructive, with typed errors
instead of panics. This is the *durability* snapshot: a consistent point-in-time
copy taken purely for the act of writing to disk.

**Scope.**

1. **Atomic write (temp file + rename).** Never mutate the live file in place
   (`File::create` truncates it to zero *before* writing — a crash mid-save
   destroys the database). Serialize to a uniquely-named sibling temp file,
   `flush` + `sync_all` (fsync) it, then `rename` it over the target. `rename`
   on the same filesystem is atomic: a reader/crasher sees the complete old file
   or the complete new file, never a partial one.
2. **Typed errors.** Introduce `PersistError` (`Io` / `Encode` / `Decode` /
   `UnsupportedVersion`) implementing `std::error::Error`. `save_to_disk` /
   `load_from_disk` return `Result`; callers (the REPL) surface the error
   instead of the process aborting.
3. **Versioned, self-identifying format.** Wrap the payload with a `magic`
   header + `FORMAT_VERSION`, because `bincode` is **not self-describing** (no
   field names/types on the wire). Without a header, adding a field in a later
   phase would silently misread old files. On load: reject a bad `magic`,
   refuse a newer `version` (`UnsupportedVersion`), and run migrations for older
   ones (a no-op until the first real schema change).
4. **Consistent snapshot.** Take the snapshot once, under a single documented
   boundary, so the file reflects one coherent point in time (today's
   per-dimension locking can interleave with a concurrent writer).
5. **Pinned codec config.** Use an explicitly configured `bincode` (fixed
   endianness / varint policy) so the on-disk bytes don't shift under a
   dependency upgrade.

**Definition of done.** Killing the process mid-save leaves `database.bin` as
either the old complete file or the new complete file (never truncated); loading
a garbage/older file yields a clean typed error and no panic; a `save` → `load`
round-trip is covered by a permanent test.

### Phase 1b — Versioned (MVCC) storage engine (the real work)

**Goal.** Adopt Infor-OLAP-style **multiversion concurrency control** so long
operations proceed on the version they began on, writes create new versions,
and obsolete versions/pages are reclaimed when no reader is pinned to them.

**Background (why this is a storage-engine change, not a persistence patch).**
The Infor model divides memory into **pages**, keeps a **list of versions**,
where each version is the *list of pages it is made of*. A writer produces a new
version using **copy-on-write** (only changed pages are new; the rest are shared
by pointer); a long-running reader keeps walking its captured version and never
observes the writer's changes. When the reader finishes, references drop, the
version retires, and pages reachable only from retired versions are freed.

This is exactly the **snapshot** abstraction — but implemented so readers and
writers run *concurrently*, which is the part Phase 1a does **not** provide
(1a's snapshot freezes everything for the duration of the write).

**Fitting it to this codebase.** The `SparseStore` is a Patricia trie, which is
already a *persistent data structure in spirit* — unchanged subtrees are shared
naturally. So the sharing unit here should likely be **trie nodes / subtrees**
rather than literal fixed-size pages: a "version" is a root pointer plus the set
of nodes created since. This mirrors the classic immutable-trie approach and
avoids importing a page allocator that the workload may not need.

**Caveat to weigh (the memory-churn cost).** Copy-on-write has real overhead.
Infor hit memory-churn problems hard enough to write their *own* `malloc`
replacement because the platform allocator was too slow for their page churn.
Node/subtree-level CoW can similarly *increase* churn and write amplification for
small writes. This must be **measured**, not assumed: the version model pays off
for large analytical scans over big cubes, and we should confirm the same is true
here before committing. A memory-churn/allocator strategy is an explicit part of
1b's scope, not an afterthought.

**Scope.**

1. Storage becomes version-addressed: a version = root pointer + created nodes.
2. Copy-on-write writes; readers pin a version and never block on writers.
3. Version retirement + node reclamation once no reader is pinned (GC).
4. A memory/allocator strategy for churn, informed by measurement.
5. `query_cache` becomes version-aware rather than globally cleared.

**Definition of done.** A long read continues on its version while a concurrent
write commits a new one; readers never see a torn state; retired versions are
reclaimed; churn is measured and bounded.

**Out of scope (both 1a and 1b).** Authentication, authorization, network
security, replication.

### Phase 1c — Protocol + server/client split

**Goal.** A server process that owns the catalog and answers requests from one
or more clients; the in-process/batch path keeps working for tests.

**Scope.**

1. **Protocol.** A serializable request/response enum (serde/bincode-friendly).
   The existing SQL/REPL command handling becomes the server's request handler —
   no query semantics change.
2. **Split.** `server` mode (owns the catalog, accepts connections) and `client`
   mode (REPL that forwards commands). The golden-test harness keeps running
   against an embedded catalog.
3. **Transaction boundary.** Define an explicit write transaction (commit all
   cube writes + cache invalidation + durable persistence, or roll back), on top
   of the 1b version store.

**Definition of done.** A server serves reads and writes from multiple clients
without data races or cache corruption; the golden-test harness still passes
against the embedded path.


---

## Phase 2 — Users, security, and data-access limits

**Why here.** Once there is a server and a session concept, identity has a
natural home. Access control is best expressed as a constraint applied to the
data the query engine already resolves.

**Goal.** Authenticated sessions, roles, and row/slice-level access limits —
enforced at the engine's existing chokepoints.

**Scope.**

1. **Identity & roles.**
   * Users, password hashing (a maintained crate — e.g. `argon2`), and roles.
   * Persist users/ACLs alongside the catalog (extend the versioned on-disk
     format from Phase 1).

2. **Authorization model.**
   * Coarse: which cubes and dimensions a role may reference at all.
   * Fine (slice-level): which **members** a role may read. `query_slice`
     already computes per-dimension leaf-id sets and weight maps — that is the
     correct place to **intersect with an "allowed members" set**, so access is
     enforced on resolved coordinates, not on SQL text.

3. **Isolation guarantees.**
   * Write isolation (who may INSERT/splash/rollup); a single write lock per
     transaction is the starting point.
   * Audit the "no silent data leak" property: a denied member must read as
     absent, not as an error that reveals its existence, where policy requires.

**Definition of done.** A session can be authenticated; a role's member-level
restrictions are provably enforced on read results; denied operations fail
cleanly without leaking structure.

---

## Phase 3 — Calculations

**Why here.** Calculated members are the highest-value "calculations" feature and
map cleanly onto the existing weighted-aggregation model. They should land
*after* security so they can themselves be permissioned.

**Goal.** Calculated measures and calculated members as first-class citizens.

**Scope.**

1. **Calculated measures** — derived members expressed over base measures
   (e.g. `Profit = Revenue - Costs`). The engine already multiplies per-leaf
   weights (`weight_maps`) and splashes values, so a calculated measure is a
   derived member whose value is an expression over base measures.
2. **Calculated dimension members** — members defined by an expression rather
   than a stored consolidation tree.
3. **Expression evaluation** — a small, sandboxed expression evaluator over
   resolved measures (arithmetic + a curated function set); no arbitrary code.
4. **Persistence & ordering** — store calculations with the cube; define their
   position in display order and how they interact with `.order`.

**Definition of done.** A calculated measure can be defined, queried like any
other measure, permissioned, and survives save/load.

**Design note.** Keep calculation semantics small and unambiguous now; a future
MDX layer (Phase 4) should be able to *reuse* this evaluator rather than
duplicate it.

---

## Phase 4 — Multidimensional query syntax (MDX)

**Why last.** MDX is a full query language (axes, tuples, sets) that *subsumes* a
large part of what calculations express. Building it before calculations and
security would force rewrites. Treat MDX as a north star and **arrive at it
incrementally**.

**Goal.** Multidimensional query expressions, approached via a pragmatic
intermediate step.

**Scope.**

1. **Intermediate: richer SQL first.** Extend the current SQL surface
   (calculated columns in `SELECT`, `WITH` clauses, members as axes) to learn the
   semantics we actually need before committing to an MDX grammar.
2. **MDX surface (later).** A parser and evaluator for a defined subset: axes,
   tuples, set expressions, and reuse of the Phase 3 evaluator.
3. **Unified semantic layer.** Ensure MDX and SQL resolve members, hierarchies,
   and calculations through the *same* engine entry points so behavior cannot
   diverge.

**Definition of done (intermediate).** The richer SQL surface is stable and
exercised by golden tests, and its semantics are documented as the contract MDX
must satisfy.

**Explicit non-goal for now.** A complete, spec-conformant MDX implementation.
This is a multi-phase project of its own, not "a next step."

---

## Immediate next step

**Phase 1a — durable persistence.** It is small, self-contained, and correct
regardless of how the rest of Phase 1 evolves:

* It is the prerequisite for a multi-user server: a full `bincode` rewrite
  guarded by `expect()` will not survive load.
* It has no dependencies and is independently testable.
* It converts the single most dangerous operation (`.save`) from "can destroy
  your data" to "safe under crash."
* The consistent-snapshot concept it introduces is the foundation Phase 1b's
  version store builds on.

**Phase 1b** (the MVCC/version store) is the larger architectural commitment and
the true source of concurrent isolation. It is scoped as its own phase, with an
explicit **measure-before-committing** gate on memory churn.

---

## Tracking

| Phase | Theme | Depends on | Status |
|------:|---------------------------------------|------------|-------------|
| 1a | Durable persistence (atomic save) | — | Not started |
| 1b | Versioned/MVCC storage engine | 1a | Not started |
| 1c | Protocol + server/client split | 1a, 1b | Not started |
| 2 | Users, security, access limits | 1c | Not started |
| 3 | Calculations | 1c, 2 | Not started |
| 4 | Multidimensional syntax (MDX) | 1c, 2, 3 | Not started |
