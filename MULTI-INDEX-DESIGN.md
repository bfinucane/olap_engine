# Multiple indexes over one fact set ("2B") — design draft

Status: **DRAFT for operator review, 2026-11-01.** No code written. Storage/persistence changes
will need a contract-change handoff before implementation (PROCESS.md), because durability with
wire-visible effects is contract-relevant.

Scope: a *fact set* stored once, addressable through **more than one Patricia trie** (index),
each appearing as a distinct cube/dataset. The operator decisions this draft implements are in
§1. Terms are defined in §2, the model in §3, and the consequences in §4–§9.

---

## 1. Operator decisions captured

1. **2B**: derived views over one fact store, not duplicated cubes.
2. **Cell content lives in a heap**; leaves point to the cell (Infor-style), rather than
   embedding the value. Caches may be **per index**, even though the cell is shared.
3. **Only the cube in use needs to be loaded** — i.e. a trie is loaded lazily; a fact set with
   two indexes need not materialise both.
4. **The flat cube is writable**, but only with **complete addressing** (a coordinate for every
   dimension); **no splashing** on it.
5. Empty cells are normal (sparse).
6. **One security policy per fact set**, even if several indexes expose it.
7. A fact is a **single fact set**; several tries are **indexes into it**; the UI may group them.
8. Scale target: **~10⁶ facts**. Dimension counts: ~30 is enough for a hierarchical cube; a
   flattened cube may need more.
9. The **flat form may be the base form**, not necessarily the hierarchical one. The flat-only
   case needs further design (one candidate is an additional SQL keyword).

---

## 2. Terms

| Term | Meaning |
|---|---|
| **Fact set** | The data: a set of facts, each with a canonical coordinate and a cell value. Exists once per dataset group. |
| **Cell heap** | The store of cell values. A fact's value lives here, addressed by a **cell id** (or an `Arc`); it is not embedded in an index leaf. |
| **Index (trie)** | A Patricia trie over one **scheme** (an ordered list of dimensions and, where relevant, hierarchy levels). Leaves hold cell ids. Multiple indexes share the same cell ids. |
| **Scheme** | The coordinate space of one index: which dimensions/levels form the key. The *flat scheme* lists fact-grain columns; the *tree scheme* replaces a hierarchy's levels with one dimension. |
| **Canonical scheme** | The scheme that owns fact identity and the persisted fact coordinates. Every write must be expressible in it. |
| **Derived scheme** | Any other scheme; its coordinate for a fact is computed from the canonical coordinate (and the hierarchy that defines the level split). |

---

## 3. Target model

### 3.1 Cell heap and leaf indirection

Today `Node { children: HashMap<u32, Arc<Node>>, value: Option<CellValue> }` embeds the value in
the trie leaf; snapshots work because nodes are persistent (`Arc`, path-copy). Two tries cannot
share a value that way without a shared cell object.

Proposed: a fact's value is a **cell token** (a `CellId` into a fact-set heap, or `Arc<Cell>` —
see §9.1). Both indexes' leaves store the same token. A write allocates a new cell (immutable)
and path-copies the affected index nodes, so existing snapshots keep seeing the old token/value.
This preserves the current MVCC property (`VersionStore` snapshots are `Arc` bumps) without
versioning the heap in place.

Consequences:
- **Memory:** the value is stored once per fact, not once per index. Each leaf gains a token
  (8 bytes) instead of an inline `CellValue`.
- **Reclamation:** with `Arc<Cell>`, Rust's refcounting frees cells when last index/snapshot
  drops them (analogous to the Infor heap pointer, with refcounts instead of manual heap GC). If
  a compact `CellId` arena is preferred, an epoch/GC pass is required instead (§9.1).
- **Serialization:** the on-disk form must be *fact set → heap + indexes*, not cube → trie.

### 3.2 Indexes and schemes

A fact set owns **N indexes**. Each index is (scheme definition, trie). Two initial shapes:

- **Flat scheme**: every fact-grain column is a dimension. Key = (Year, Quarter, Month, Region,
  …). Complete addressing; writable.
- **Tree scheme**: a date hierarchy's levels are collapsed into one `Time` dimension (leaf =
  e.g. `2024-01`); aggregates are consolidations, never stored.

The relation between them is the **hierarchy** (Year → Quarter → Month → leaf day/month). The
flat coordinate of a fact is derived from the tree leaf's ancestors; the tree leaf is the flat
coordinate's finest level member. A flat coordinate is valid only if its level members are
consistent with that hierarchy (e.g. `Q1 / 2024-02` is fine only if `2024-02`'s ancestors are
`2024 / Q1`; otherwise the write is refused — it could not correspond to any fact).

### 3.3 Canonical scheme

One scheme is canonical: it defines fact identity and the persisted coordinates. The operator
suggests the **flat scheme may be canonical**. That is viable if:

- every fact has a flat coordinate at fact grain (all fact-bearing columns present);
- the level relationship is declared (the hierarchy used to derive the tree view), so the tree
  coordinate is computable from the flat tuple;
- the finest level member uniquely identifies the fact (one fact per canonical coordinate).

If instead the tree scheme is canonical, the flat coordinate is derived from the leaf's
ancestor chain, which is always well-defined. **Recommendation:** start with the *hierarchical*
scheme canonical (facts are leaves, a mature concept here) and the flat scheme derived; the
flat-as-canonical variant is a later, symmetric extension. This does not change the user-facing
result: both appear as cubes/indexes.

### 3.4 Lazy loading and writes

Only the index in use is loaded. A fact set persists as: heap + canonical index + optional
derived indexes + per-index **stale marker/version**. Loading a derived index:
- if its file is current → deserialize it;
- if stale → rebuild from the canonical index (or replay a canonical write log), then persist.

Writes through a loaded index update the heap (new cell token) and that index; other indexes
are marked stale. Writes through a non-canonical index must be expressible in the canonical
scheme (complete addressing + consistency, §3.2) so the canonical index can be updated; if the
canonical index is not loaded, the write appends a **fact-level delta** (canonical coordinate,
new token / tombstone) to a log that a stale index replays when loaded. A rebuild/scan at 10⁶
facts is seconds, so "mark stale, rebuild on load" is acceptable for v1; the delta log avoids
reprocessing the whole heap.

### 3.5 Caching

Per operator preference, **caches are per index**: each index has its own version-tagged cache
(as `VersionStore` does today). Invalidation is driven by a **fact-set epoch** that advances on
every write; a read that finds a newer epoch lazily discards its own cache entries. This keeps
reads lock-free and avoids copying aggregate results between views.

---

## 4. Writes, deletes and the contradiction

### 4.1 Writes

- **Flat cube:** complete addressing required, exactly as the operator stated — every dimension
  member must be supplied. There is no splashing: a coordinate either identifies a fact level
  (write the fact) or violates the hierarchy consistency rule (refuse with a typed error).
- **Tree cube:** complete addressing means supplying a leaf member per dimension; writing to an
  aggregate coordinate keeps the existing splash semantics (`reject|replace|add`), because that
  is a semantic *expansion to leaves*, not a new fact.
- A write through either view produces exactly one canonical fact and one cell token; both
  indexes point to it. Last write wins.

### 4.2 Deletes — three different operations

The contradiction the operator identified ("deleting an aggregate only reshapes; deleting the
same name in the flat view deletes data") is resolved by never overloading one command:

| Operation | Effect on facts | Effect on indexes |
|---|---|---|
| **Reshape** — remove/rename an aggregate-level member (Q1, 2024) | none | hierarchy changes; derived views recompute their level members |
| **Purge facts** — delete at a fact-grain coordinate | removes cell tokens | every index loses those leaves (loaded ones directly, stale ones on rebuild) |
| **Drop view** — remove a non-canonical index | none | one index file removed |

The rule is: **data-bearing status is a property of the hierarchy level, not of the column it
appears in.** In the flat view, a `Quarter` value is a *label* repeated on facts; in the tree
view, `Q1` is a *consolidation*. Deleting "Q1" therefore means "reshape" through the tree index
and "purge facts labelled Q1" only if the caller explicitly asks for a fact purge. The UI must
carry the intent; the engine must not infer it from a name. This is the single most important
semantic decision in the design and is testable.

Empty cells are permitted and expected; a leaf can be absent for a coordinate whose higher-level
labels exist. (The engine is sparse already.)

### 4.3 Consistency checks

A write through the flat view is refused if its level members do not describe a realizable leaf
of the declared hierarchy. A write that would create a new level member (e.g. `Q5`) is refused
unless the hierarchy itself is being extended by a separate schema operation.

---

## 5. Security

One policy per **fact set** (operator decision 6): the existing control cubes are interpreted
against the canonical coordinates; every index inherits the policy via the scheme mapping.

- Denying a dataset hides the **fact set/group** (all its indexes), not one view.
- Denying a dimension/member maps to canonical members: hiding `Q1` hides the facts whose tree
  leaf is under `Q1`, in every index; hiding a leaf month hides it in both.
- Write permission is checked at the fact set, not per index.
- `#Cubes`/`#Columns` list each index as a dataset; a grouping/`viewOf` marker is desirable but
  would be a wire addition (owner ruling needed). Without it, clients may sum two datasets that
  are views of the same facts — the UI grouping and documentation must prevent that.

---

## 6. Dimensions, memory and performance (10⁶ facts)

**No hard dimension cap exists** in the engine (the key is `Vec<u32>`; depth is unbounded in
code). The practical constraints are real:

- **Key length:** 4 bytes per dimension per cell in an index leaf; a 30-dimension index means
  30 ids on each path.
- **Trie node cost:** this trie stores a `HashMap<u32, Arc<Node>>` per level and has **no path
  compression**, so even a single-child run costs a node per dimension. Nodes are the dominant
  memory term, not cells.
- **Rough model:** node count is the number of distinct prefixes. With 10⁶ facts and high
  cardinalities, a 30-dimension index can approach 10⁶ × 30 nodes in the worst case; at
  ~50–100 bytes per node that is far too much. Realistic sharing (date levels, low-cardinality
  dimensions first) reduces it greatly, but **this must be measured, not assumed**.
- **One vs two indexes:** cells are stored once; index nodes roughly double. At 10⁶ facts, a
  single 30-dim index may already be the limiting factor.

Recommendations:
1. **Benchmark before promising 30+ dimensions**: 10⁶ facts, flat index depth 20/30/40,
   measure node count, heap, load time, scan and write times, with one and two indexes.
2. Keep the number of **materialized indexes** small (2–3); lazy loading makes this cheap.
3. If memory is binding, the next lever is the trie representation (sorted-array children,
   path compression), not a dimension cap. A soft product limit (documented, not enforced) can
   follow the benchmark.
4. Dimension order in a scheme is an optimisation: put the dimensions used for filtering early
   (the present `scan_subcube` prunes level by level; later dimensions cannot prune).

Writes cost O(depth) per updated index plus one cell allocation; at 10⁶ facts and two indexes
this is fine. Reads are index-specific, cache-friendly.

---

## 7. Flat as the base form, and the flat-only case

The storage design is neutral to which scheme is canonical (§3.3), but the semantics are not:

- **Flat canonical** requires the level relationship to be declared so the tree view can be
  derived. If the flat columns are just independent labels (no hierarchy), a tree view is
  impossible — correctly so.
- **Flat-only** cube (no hierarchy at all) is already representable: a cube with many
  dimensions, sparse cells, complete-addressing writes. The open question is *query ergonomics*:
  writing `SELECT quarter, month, SUM(sales)` on a flat cube is ordinary SQL; the operator's
  concern that "all columns need to be mentioned" needs a concrete failing example before the
  engine adds a keyword.
- The suggested **SQL keyword** is orthogonal to storage: it belongs to the SQL front end (tier
  T3, engine tooling, not the client contract). Candidates worth evaluating once the requirement
  is pinned down: standard `GROUP BY ROLLUP/CUBE/GROUPING SETS` (produces multiple aggregation
  levels in one result), or an engine-specific level-selection clause. It does **not** require a
  second trie; it may even remove the need for one.

---

## 8. What this costs in this codebase

1. **Cell indirection** in `SparseStore`/`Node` (token instead of inline value), with MVCC
   preserved; benchmark against the current inline layout. This alone is a deep change to every
   read/write path.
2. **Fact-set container**: a catalog-level object owning the heap, the canonical index and the
   derived indexes; `Cube` becomes a *view* over it. `CatalogDiskFormat { dimensions, cubes }`
   becomes `{ dimensions, fact_sets, views }` → `FORMAT_VERSION` bump, migration/rejection of
   old files, atomic save of a directory rather than a file.
3. **Lazy index load** + stale markers + rebuild/delta log; per-index `VersionStore`-style
   caches over a shared heap epoch.
4. **Scheme/derivation metadata**: declared hierarchy ↔ level split, validation on write, error
   codes; `#Cubes`/`#Columns`/`describe` for views; optional `viewOf` (wire change → owner).
5. **Deletion intents** (reshape / purge / drop view), security mapping, and client grouping.
6. **SQL/UI ergonomics** (rollup keyword, grouping) last; it is independent of 1–5.

Rough order of magnitude: cell indirection + fact-set/views + persistence is the bulk; lazy
loading and delta logs are second; delete/security semantics and client metadata are policy
work as much as code.

---

## 9. Open decisions

1. **Cell token:** `Arc<Cell>` (automatic reclamation, simpler, 8-byte pointer) vs `CellId` into
   an arena (compact, needs epoch GC). Recommendation: `Arc<Cell>` for the first implementation
   unless memory profiling says otherwise.
2. **Canonical scheme:** hierarchical (recommended first) vs flat (operator's suggestion). Both
   are possible; the derived view's mapping must be declared either way.
3. **Stale-index strategy:** rebuild from canonical on load (simple, seconds at 10⁶) vs append
   a fact delta log (faster, more format). Recommendation: rebuild for v1, delta later.
4. **Delete UX:** how the UI distinguishes "reshape Q1" from "purge facts labelled Q1"; the
   engine should require an explicit intent rather than guessing.
5. **Wire metadata:** should a view advertise `viewOf` / grouping? This is a wire change and
   needs the contract owner.
6. **Dimension guidance/soft cap** after the benchmark; no code cap today.
7. **SQL keyword**: what exactly should it express? Needs a concrete example before design.
8. **Cache scope:** per index (operator preference) — confirm that aggregate results may be
   cached independently in each index and invalidated by a shared epoch.

---

## 10. Suggested next steps

1. Operator review of this draft; resolve §9 items 1–3 and 7.
2. Write a contract handoff for the storage/format change (heap + fact sets + views) **before**
   code, plus a ruling on optional `viewOf` metadata.
3. Prototype phase A: cell indirection in the current single-index engine, preserving MVCC;
   benchmark memory and read/write against today at 10⁵ and 10⁶ cells.
4. Prototype phase B: one fact set with two schemes (flat + tree) on a synthetic date hierarchy,
   lazy load, stale rebuild, per-index caches; validate the deletion intents and security
   mapping.
5. Only then decide whether the flat-only SQL keyword is needed; measure the 30-dimension case
   before promising it.
