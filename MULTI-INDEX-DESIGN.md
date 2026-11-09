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
- **Trie node cost (baseline):** the present trie stores a `HashMap<u32, Arc<Node>>` per level and
  has **no path compression**, so even a single-child run costs a node per dimension. Nodes are
  the dominant memory term, not cells. This is a *property of the current representation*, not of
  the model: adaptive nodes and path compression (§22) can remove most of it, which is why the
  dimension limit has to be measured per representation.
- **Rough model (baseline layout):** node count is the number of distinct prefixes. With 10⁶
  facts and high cardinalities, a 30-dimension index can approach 10⁶ × 30 nodes in the worst
  case; at ~50–100 bytes per node that is far too much. Realistic sharing (date levels,
  low-cardinality dimensions first) reduces it, and an adaptive layout (§22) reduces it much
  further — **this must be measured per representation, not assumed**.
- **One vs two indexes:** cells are stored once; index nodes roughly double. At 10⁶ facts, a
  single 30-dim index may already be the limiting factor.

Recommendations:
1. **Benchmark before promising 30+ dimensions**: 10⁶ facts, flat index depth 20/30/40,
   measure node count, heap, load time, scan and write times, with one and two indexes.
2. Keep the number of **materialized indexes** small (2–3); lazy loading makes this cheap.
3. If memory is binding, the next lever is the trie representation — adaptive branch nodes,
   path compression (§22) — not a dimension cap. Benchmark the baseline layout and the adaptive
   layout side by side; a soft product limit (documented, not enforced) can follow.
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

---

# Revision 2 — operator decisions, attributes and non-unique hierarchy members

Appended 2026-11-02 after the operator's response. Where this conflicts with §3.3, this
revision wins.

## 11. Decisions recorded

1. **Canonical scheme: flat**, organised as a **star schema** (fact rows carry keys, master data
   lives beside them). How to enforce the star shape is still open — options in §13.
2. **Cell token: `CellId`** into an arena (not `Arc<Cell>`); reclamation via epoch GC (§14.1).
3. **Lazy loading with a delta log**: unloaded indexes are stale; writes append fact-level
   deltas they replay on load.
4. **Delete UX: not pressing** — postponed; the examples in §15 remain for later.
5. **`viewOf` marker: yes — a tag in the data and an icon in the UI** (§16).
6. Dimension counts/benchmark unchanged (§6); per-index caches confirmed.

Consequence of (1): fact identity is the canonical **flat key** (the key columns + measures),
not a hierarchy leaf. The tree view derives a hierarchy leaf from those keys, and the tree's
consolidations are pure metadata. This makes the "what is data vs shape" question sharper, not
simpler — see §15.

**Operator clarification (2026-11-02):** the reason enforcement is hard is the **source data**:
the demo CSV mixes master data into the transaction rows (`Customer Name`, `City`, `State`,
`Category`, `Product Name`, …). A flat canonical model has to split those into masters + keys;
the engine has no modelling module to declare or enforce that. Such a module is required to
finish the project but is **postponed**; until it exists the split is carried by import tooling
and its validation (§20).

## 12. Why attributes break the flat model

The demo columns `Customer ID` and `Customer Name` are master data: the name belongs to the
customer, not to each order line. **Operator clarification:** this is the *source-data admixture*
problem — master data arrives inside the transaction table instead of separate master tables.
Three concrete shapes, to compare:

**Example A — denormalised fact (what an importer produces today).**

| RowID | OrderDate | CustomerID | CustomerName | City | State | Sales |
|---|---|---|---|---|---|---|
| 1 | 2024-01-05 | C-001 | Ava Thompson | Bristol | Tennessee | 261.96 |
| 2 | 2024-01-06 | C-001 | Ava Thompson | Bristol | Virginia | 731.94 |
| 3 | 2024-01-07 | C-002 | Ava Thompson | Bristol | Tennessee | 14.62 |

Problems: the name is repeated 5,560 times; renaming C-001 rewrites every fact (and can leave
disagreement); `Ava Thompson` is also two different customers (a join on name would merge them);
and the flat fact carries level labels (`State`) that are really master data. This shape is
easy to import but is not a star schema.

**Example B — star schema, keys in the fact, masters beside it.**

```
Fact Orders (canonical, flat key)
  RowID, OrderID, DateID, ShipDateID, CustomerID, CityID, ProductID,
  ShipMode, OrderPriority, Measure, CellValue
Master Date      (DateID PK, Date, Year, Quarter, Month, Week)
Master Customer  (CustomerID PK, Name, Segment)
Master Geography (CityID PK, CityName, StateID, StateName, CountryID)
Master Product   (ProductID PK, ProductName, Category, SubCategory)
```

- `Customer Name` is stored once; a rename is one master row.
- Two customers may share a name: identity is `CustomerID`.
- The tree view derives `Year→Quarter→Month` from `DateID`, and `Country→State→City` from
  `CityID`/`StateID`; labels come from the masters.
- The flat view presents `DateID, CustomerID, CityID, …`; display labels can be joined in (see
  Example C) but are not part of the fact.

**Example C — derived attribute columns (client convenience, engine-side join).**

`GET /v1/OrdersFlat` describe exposes, in addition to the keys:

| Column | Kind | Origin | Writable |
|---|---|---|---|
| CustomerID | dimension | fact | yes |
| CustomerName | attribute | master Customer | no (write the master) |
| City | attribute | master Geography | no |
| State | attribute | master Geography | no |

A query `SELECT CustomerName, SUM(Sales)` is resolved by the engine joining the fact's
`CustomerID` to the master. This is what lets Plateau, which has no join, use a star schema.
Writing `CustomerName` through the fact is refused unless the schema declares it a
**denormalised snapshot** column (then it is fact data and the master is advisory).

**What enforcement needs to say** (the open question):
- which fact columns are **keys into a master**, which are **fact attributes**,
- which master columns are **attributes** (read-through) and whether any are snapshotted,
- referential integrity: a fact key must resolve in its master (or a declared autocreate policy),
- uniqueness of master keys,
- which hierarchy is derived from which keys (DateID for the date tree, Geography for geo).

## 13. Star-schema enforcement — options to evaluate

The engine currently has dimensions, attribute tables (EAV cubes) and a SQL join, but no
declared foreign keys. Plateau has no join. Four enforcement levels:

**Option 0 — convention + tooling.** The import tool/schema script declares masters and keys;
a validation pass checks referential integrity and rejects attribute columns in the fact. No
engine change; the server cannot guarantee it, and a hand-written CETAS/import can violate it.

**Option 1 — `REFERENCES` in the cube DDL.** Extend the existing `CREATE TABLE`/`CREATE CUBE`
with foreign keys:

```sql
CREATE TABLE Orders (
  RowID STRING, OrderID STRING,
  DateID STRING REFERENCES Date,      -- master dimension Date
  CustomerID STRING REFERENCES Customer,
  CityID STRING REFERENCES Geography,
  ProductID STRING REFERENCES Product,
  ShipMode STRING, OrderPriority STRING,
  Measure MEASURE
)
```
Enforcement: import/write validates that the referenced member exists (or creates it per
policy; the engine already has `createMissing`); `Customer.Name` cannot be inserted into
`Orders` unless declared; `describe` can expose master attributes as derived columns. Cost:
DDL parser work + referential checks + query-time attribute resolution.

**Option 2 — a declared schema manifest / meta-command.** A single declaration loaded with the
database (JSON or `.schema` command) describes fact keys, masters, attributes, hierarchies and
key formats:

```json
{
  "facts": { "Orders": {
      "keys": { "DateID": "Date", "CustomerID": "Customer", "CityID": "Geography",
                "ProductID": "Product" },
      "attributes": ["ShipMode", "OrderPriority"],
      "measures": ["Sales", "Profit", "Quantity", "Discount"] } },
  "masters": {
      "Customer":  { "key": "CustomerID", "attributes": ["Name", "Segment"] },
      "Geography": { "key": "CityID",    "attributes": ["CityName", "StateID", "StateName", "CountryID"] } },
  "hierarchies": {
      "Date":  { "levels": ["Year", "Quarter", "Month", "Date"] },
      "Geography": { "levels": ["CountryID", "StateID", "CityID"] } }
}
```
The importer builds and validates everything from this; the server enforces on write. This is
the option I would evaluate first: it centralises enforcement without committing to a full SQL
DDL implementation, and the same document can be the model pin.

**Option 3 — full star-schema DDL** (`CREATE DIMENSION …`, `ALTER …`, constraints, catalogs).
Most familiar to Infor/TM1 users, largest change.

Comparison:

| | Enforced by server | Client sees joins? | Effort | Risk |
|---|---|---|---|---|
| 0 convention | no | no (data already flat) | none | silent inconsistency |
| 1 REFERENCES | yes | attributes derivable | medium (DDL, checks) | SQL parser growth |
| 2 schema manifest | yes | yes, via describe | medium | new file concept |
| 3 full DDL | yes | yes | large | scope explosion |

## 14. Non-unique hierarchy members — the two Bristols

**Example D — automatic hierarchy from names is ambiguous.**

| Row | City | State | Sales |
|---|---|---|---|
| 1 | Bristol | Tennessee | 100 |
| 2 | Bristol | Virginia | 200 |
| 3 | Bristol | Tennessee | 50 |

An automatic `State → City` hierarchy on **names** creates one `Bristol` member with two
parents. The engine's model allows one parent per hierarchy (`child_to_parent`), so the build
either errors, silently keeps one parent (wrong rollup), or, if `Bristol` is made a leaf and
`Tennessee`/`Virginia` its parents, the member cannot be a child of both. Names are not
identities.

Three ways out, in increasing cost:

1. **Qualified member names** — `City` member is `Bristol, TN` / `Bristol, VA` (or
   `US/TN/Bristol`). Works today; display is noisy and any label change is a schema change;
   state names also need qualifying by country (`Georgia` the state vs the country).
2. **Surrogate keys** — `CityID` (and `StateID`, `CountryID`) are the dimension members;
   `CityName`, `StateName` are attributes in a master table. Uniqueness is structural. Display
   needs attribute columns (Example C) or a client that resolves the attribute. This is the
   star-schema answer and works with today's engine *provided* clients get attributes.
3. **Surrogate identity with display names** — the engine's member identity becomes an id, with
   a separate display name (as aggregates already have `display_names`). Then two members can
   both display `Bristol` and clients see friendly labels. This is an engine-model change
   (leaf identity is currently the name), and it is the cleanest long-term answer.

Note the related trap: **auto-constructed hierarchies from columns are only valid if each
level's member is unique within its parent.** The safe construction rule is "key columns, not
labels"; the loader should reject a hierarchy whose keys are not unique rather than inventing
one.

## 15. Delete UX — examples to evaluate

Same fact set, both views. Candidates:

**(a) Tree view.** `DELETE MEMBER 'Q1-2024' FROM Time`
- intent: reshape (remove the consolidation) — facts untouched; tree view loses the quarter
  rollup; flat view still has its `Quarter` label column (if flat is canonical and stores the
  label) — the two views now disagree about "Q1 exists".
- if the leaf hierarchy is derived from the flat key, removing a level member means changing
  the **derivation**, not the facts.

**(b) Flat view.** `DELETE MEMBER 'Q1-2024' FROM OrdersFlat`
- the member is a label on facts; deleting it either (i) deletes facts labelled Q1 (data loss)
  or (ii) is refused/reshapes the label domain. The operator must say which.

**Recommended command set** (explicit intent, never inferred from the name):

```
RESHAPE Time REMOVE 'Q1-2024'            -- schema only: facts and labels unchanged; derived
                                         -- tree index rebuilt; flat data untouched
PURGE FACTS FROM OrdersFlat
      WHERE Quarter = 'Q1-2024'          -- data: removes canonical facts + all index entries
DROP VIEW OrdersTree                     -- metadata: index removed; facts untouched
```

Examples to evaluate: (i) should `RESHAPE` be allowed when the flat canonical stores the label?
(ii) should `PURGE` require a fact-grain coordinate, or is a level predicate acceptable?
(iii) should the engine expose "this member is a consolidation / a label / a fact key" in
describe so the UI can warn?

## 16. `viewOf` marker — payload example

On the wire (describe/capabilities/dataset list), e.g.:

```json
{
  "dataset": "OrdersTree",
  "viewOf": "factset:orders",
  "scheme": "tree",
  "schemeGroup": "Orders",
  "canonical": false,
  "writable": "leaf"
}
```
and for the canonical flat view:

```json
{
  "dataset": "OrdersFlat", "viewOf": "factset:orders", "scheme": "flat",
  "schemeGroup": "Orders", "canonical": true, "writable": "full"
}
```
Clients group by `schemeGroup`, warn before summing two views of one fact set, and choose the
writable one. This is a wire addition (owner ruling).

The four fields are the **data tag**; the UI additionally shows an **icon** for a view that is
not canonical (operator decision 5). The tag is wire metadata (owner ruling); the icon is a
client-side rendering decision.

## 17. Delta-log example (lazy loading)

One record per write, canonical coordinate + new cell token (or tombstone):

```
{"seq":1042,"op":"upsert","fact":["D-2024-01-05","C-001","G-Bristol-TN","P-42"],"cell":918273}
{"seq":1043,"op":"delete","fact":["D-2024-01-06","C-002","G-Bristol-VA","P-17"],"cell":null}
```
A stale index records the last `seq` it incorporates. On load it either replays the delta or
rebuilds from the canonical index (batch path); the delta is compacted at save/checkpoint.

## 18. SQL convenience — removed from the core decisions

The operator's correction: deriving levels and parsing data is a **modelling** exercise, not a
query-time operation. Query time must reference modelled levels/columns; it must not parse
strings or infer structure. Therefore:

- the SQL-keyword idea is **not a design decision** for the multi-index work;
- `YEAR(Date)`/`QUARTER(Date)`-style parsing is dropped (it would put modelling in queries);
- if both shapes are modelled (flat and tree indexes), clients query the flat one directly and
  need no keyword;
- an optional console convenience (`EXPAND Date INTO (Year, Quarter, Month)`, or standard
  `GROUP BY ROLLUP(...)` for subtotals) may be revisited later as **T3 engine tooling** — it
  exposes already-modelled levels and changes no storage or semantics. It is not required.

This leaves the design focused on the model: fact set, schemes, masters, indexes.
## 19. Open decisions after revision 3

1. Star-schema enforcement: option 0/1/2/3 (§13), and if option 1/2, what happens on a missing
   master member (refuse / autocreate / quarantine)?
2. Are master attributes read-only through the fact view, or may they be snapshotted?
3. City identity: qualified names, surrogate keys + attribute exposure, or display-name support
   in the dimension model (§14)?
4. Delete UX: **postponed** (not pressing). The examples in §15 remain for when it is picked up.
5. Benchmark 10⁶ facts × depth 20/30/40 before promising a wide flat cube (§6).

The modelling module (star-schema declaration, level derivation, master validation) is required
to finish the project and is **postponed**; §20 records the load-time contract the design assumes
so it can be implemented inside that module later.
## 20. Load-time contract (applied edit)

Everything that interprets data belongs here, once, not per query.

| Input | Load-time action | Failure |
|---|---|---|
| dates | parse an ISO date into the declared hierarchy (tree scheme) or into Year/Quarter/Month columns (flat scheme) | unparseable value names the row/column |
| city/state labels | assign or require surrogate keys (`CityID`, `StateID`); validate uniqueness within parent | duplicate key or ambiguous label refuses the load |
| master data | declare master tables (Customer, Product, Geography, Date) and their attributes; facts carry only keys | an attribute column in a fact table is refused unless declared a snapshot |
| fact keys | check referential integrity against masters (or a declared autocreate policy) | unknown key refuses the row (or autocreates, per declaration) |
| hierarchies | build from **key columns**, never from display labels; validate one parent per child and a unique path | ambiguous member refuses the hierarchy build |
| measures | declared per fact set; the cell heap stores them (vector or measure dimension) | unknown measure refuses |

Query time: `describe` exposes the declared columns/levels; queries address them directly.
`ROLLUP` (later, optional T3) only produces subtotals over those columns. No query parses,
derives or infers model structure.

Until the modelling module exists, the import script/tool is responsible for the table above and
must fail loudly instead of guessing. Today's `superstore-v2.sql` imports names straight from the
CSV — exactly the admixture the operator flagged; the import rules above are what that module
will formalise.

## 21. Revision 3 — applied edits (2026-11-02)

1. SQL-keyword item removed from the decisions and demoted to an optional T3 appendix (§18);
   `YEAR()/QUARTER()` parsing dropped; shapes are modelled, queries reference them.
2. Load-time contract added (§20): parsing/derivation, city keys, masters, referential integrity
   all at intake; validation fails loudly.
3. View marker clarified: data tag (`viewOf`/`schemeGroup`/`canonical`/`writable`) plus a UI icon
   for non-canonical views (§16); delete UX marked postponed (§19).
4. Operator clarifications recorded: flat canonical + star schema addresses the **source-data
   admixture**; a modelling module is required but postponed.
## 22. Adaptive node representation (operator note, 2026-11-02)

The current node (`children: HashMap<u32, Arc<Node>> + value`) is the baseline, not the design.
Multidimensional in-memory engines use adaptive branch nodes to trade lookup speed against
memory across sparse spaces; this section records how that applies here.

### 22.1 Branch variants

| Variant | Lookup | Memory (rough) | Use when |
|---|---|---|---|
| singleton | O(1) | one `(id, Arc)` inline | exactly one child (very common in deep paths) |
| small vector | O(n), n ≤ 8 | ~16·n | few children |
| sorted vector | O(log n) | ~16·n | sparse node, moderate child count |
| bitmap + pointer array | O(1) via rank/select (popcount) | C/8 + 8·n | medium density, large coordinate span |
| dense direct array | O(1) | 8·C | high occupancy and/or small C |

`n` = child count; `C` = dense coordinate span at that level (`max_id + 1` for the dimension,
not its cardinality: ids are sequential but deletions leave gaps). A dense array indexed by a
32-bit id is only safe under a span cap (e.g. 4–16K); beyond that use bitmap or sparse.

### 22.2 Promotion and demotion

Use **estimated bytes**, not only fill ratio, and compare variants:

- sparse ≈ 16·n, bitmap ≈ C/8 + 8·n, dense ≈ 8·C;
- promote to dense when `8·C < 2·(16·n)` (≈ occupancy > 25%) **and** `C ≤ span cap`;
- promote to bitmap when `C/8 + 8·n < 16·n` (i.e. `C < 64·n`) and dense is not preferable;
- demote with hysteresis (e.g. promote at > 0.30 occupancy, demote below 0.15) to avoid thrash;
- conversions are path-copy writes: the old snapshot keeps the old representation (MVCC is
  preserved), and the cost is O(children) on the write that converts.

Examples with the demo cardinalities:

| Level | Dimension | C (span) | Default |
|---|---|---|---|
| Year | 4 | 4 | dense, always (32 B) |
| Quarter | 16 | 16 | dense (128 B) |
| Month | 48 | 48 | dense (384 B) |
| City | ~1,000 | ~1,000 | sparse/bitmap; dense only if most cities appear under each parent |
| CustomerID | ~10,000 | ~10,000 | bitmap or sparse; never dense per node |
| unique-ish (RowID) | 10⁶ | 10⁶ | sparse/path-compressed; dense excluded by span cap |

So the correct statement is **"low-span / low-cardinality levels default to dense; occupancy and
span decide the rest"** — not "upper levels are always dense". Level-based defaults come from the
dimension metadata at that position in the scheme (span, expected fan-out, position).

### 22.3 Path compression

Runs of nodes with exactly one child and no value collapse into one edge carrying the path ids
(`Vec<u32>`): a Patricia trie. This matters most for wide flat schemes, where the last dimensions
are often unique per fact: without compression that is one node *per dimension per fact*; with
compression it is one edge per fact carrying its remaining ids (~4 bytes each), eliminating the
per-node/HashMap overhead. Insert splits a compressed edge at the divergence; delete merges it.
`scan_subcube` must test every id in a compressed path against its allowed set (prefix pruning
still works when the filter dimension falls beyond the compressed segment).

### 22.4 MVCC, persistence and format

- Variants and compressed edges are part of the node; path-copy keeps snapshots correct, and
  promotion/demotion never mutates an existing version.
- Persistence must serialise the variant tag (format bump in the fact-set/views work). No wire
  change: this is internal storage.
- Caches stay per index; a write that converts nodes still bumps only the affected index/store
  version.

### 22.5 Revised expectation for the 10⁶-fact flat cube

The earlier estimate (≈18–22 M nodes for 22 dimensions) assumed HashMap nodes. With singleton +
sorted/bitmap/dense variants and path compression, the dominant per-level node overhead is
replaced by: small dense arrays at low-span levels, bitmaps at medium span, and compressed path
ids at high span. Illustratively, a 22-dimension, 10⁶-fact index with low-cardinality date/geo
levels first and unique-ish tail dimensions could fall from ~20 M nodes to a few million
branch/leaf nodes plus ~4 bytes × remaining dimensions per fact (~50–90 MB of path ids) —
plausibly a 3–10× reduction, **but the branching factor in the middle levels decides it and only
a benchmark can settle it**. Two indexes still roughly double index memory; cells remain shared
and single-copy.

### 22.6 Benchmark additions

Compare, at 10⁵ and 10⁶ facts:

- representations: baseline HashMap; singleton+sorted; +bitmap; +dense (spans 64/1K/16K);
  +path compression;
- thresholds: occupancy 0.25 vs 0.50; hysteresis on/off;
- schemes: tree (8 dims), flat (22), wide (30), with 1 and 4 measures; one and two indexes;
- metrics: RSS, node/edge counts, serialized size, load/save time, Q1–Q8 latencies, write 10k
  cells, conversion counts.

The benchmark answers the dimension question: with adaptive nodes the practical limit may be
set by the **middle-level branching factor and the span cap**, not by a flat dimension cap.

### 22.7 Open choices

1. Variant set for the first implementation: singleton + sorted vector + dense, with bitmap and
   path compression later? Or all at once (a much harder MVCC/serialisation change)?
2. Dense span cap: 4K, 16K, or profile-driven?
3. Occupancy thresholds (0.25/0.25 hysteresis?) and whether conversion cost should be amortised
   across writes.
4. Dimension-id gaps: accept sparse spans (dense excluded), renumber ids on compaction, or keep
   bitmap only.
5. Is path compression in scope for the first fact-set implementation, or a follow-up after the
   two-index model works?

## 23. Revision 4 — adaptive nodes recorded (2026-11-02)

Added §22 and revised §6: the dimension/memory limit is **representation-dependent**. The
baseline HashMap layout's estimate is retained as baseline, but the design now assumes adaptive
branch nodes (singleton/sorted/bitmap/dense) with occupancy-based promotion, level defaults from
dimension span, and path compression, all gated by the §22.6 benchmark. This does not change the
decisions in §11; it changes how the memory question is answered.
### 22.8 The id-gap issue (why span ≠ cardinality)

Dimension ids are **internal, monotonic and never reused**: `add_leaf`/aggregate creation do
`let id = next_id; next_id += 1`, and `delete_member` removes the id from the maps without
decrementing `next_id`. So a dimension with 1,000 live members can have ids up to 50,000 after
loads that create and delete members (re-imports, corrections, scratch loads).

A dense array indexed by id must allocate `max_id + 1` slots, not `live_members` slots:

- City dimension, 1,000 live cities, ids up to 50,000 → one dense node is 50,001 × 8 B ≈ **400 KB**
  even if that node has 10 children; a sparse/sorted node is ≈ 160 B.
- Bitmap has the same span problem (`span/8` bits) but is far cheaper: 50,000 bits ≈ 6 KB per
  node; still bad if there are thousands of nodes at that level.
- Sparse variants do not care about the span at all.

Options considered:

| Option | Cost | Decision |
|---|---|---|
| dense only when span ≤ cap; otherwise bitmap/sparse | none | **chosen for v1** |
| renumber ids on a user-triggered **cleanup** step (`1..n`) | must remap every index and cell coordinate atomically; internal ids, so an offline pass is safe | **chosen** as a maintenance step, §25 (not automatic) |
| local ordinal per node (compact slot + id→slot map) | O(1) lost, extra structure per node | rejected for now |

The practical rule: **dense is keyed by span, and the span cap is benchmark-driven**; do not
assume the live cardinality bounds the span.

### 22.9 Decisions recorded (2026-11-02)

1. **Path compression**: included from the start in the prototype/benchmark (it is the main lever
   for wide flat schemes); the production implementation lands it as a **follow-up** after the
   two-index fact-set model works. (Reading operator items 1 and 4 together; if "compression from
   the start" was also meant for production, say so — that is the one open point.)
2. **Span cap**: benchmark-driven. No fixed number: measure 4K/16K/64K spans and choose from the
   memory/latency curve. The cap governs both dense arrays and bitmap span.
3. **Id gaps**: no automatic renumbering. Dense only when span ≤ cap; otherwise bitmap/sparse.
   A **user-triggered cleanup step** (§25) can renumber ids on demand; it is explicit, offline, and
   extensible with future optimisation steps.
4. Benchmark matrix unchanged (§22.6); add the span/occupancy sweeps of (2).

## 24. Revision 5 — adaptive choices recorded (2026-11-02)

Added §22.8 (id-gap explanation) and §22.9 (operator decisions: compression in the prototype from
the start, production follow-up; benchmark-driven span cap; no id renumbering in v1). No change to
§11 decisions or §20 load-time contract.
## 25. User-triggered maintenance ("cleanup")

Decision (2026-11-02): the id-gap fix is a **maintenance operation the user triggers**, not an
automatic rewrite, and the mechanism is a **step registry** so future optimisations can be added
without inventing new commands.

### 25.1 Shape

```
maintain --db <file> --steps ids[,heap,delta,nodes] [--dry-run] [--backup] [--verify]
```

or, for the operator, a `scripts\maintain-db.cmd` wrapper (execution-policy safe) with the same
options. **CLI only** (decided 2026-11-02): there is no server admin endpoint for maintenance. Each step has a name, a description, a safety class and a before/after report. `--steps
all` runs the registered steps in a fixed order; a failing step aborts before the file is
replaced. **Offline first:** the command refuses to run while the shared server is listening
(the file must not be in use), loads the database, applies the steps, saves atomically
(temp + fsync + rename), and keeps the previous file when asked. A server-side admin endpoint is
possible later but is a wire change (contract handoff); v1 does not need it.

### 25.2 The `ids` step (renumber to remove gaps)

1. For every dimension, collect live ids (leaf bank + aggregates reachable in the hierarchies +
   `default_member_id`), sort by old id, assign `1..n`, build `old → new`.
2. Rewrite the dimension: `leaf_id_to_name`, `member_types`, every hierarchy's `member_to_id`
   values, `display_names` keys, `consolidations` parent keys and child ids, `child_to_parent`,
   `root_order`, `display_order`; set `next_id = n + 1`. Member **names and order are unchanged** —
   only internal ids move.
3. Rewrite every index/trie over every fact set: walk cells and remap each coordinate; for a
   compressed (Patricia) edge, remap every id in its path. The cell heap (CellIds) is untouched.
4. If a delta log exists, **checkpoint it into the canonical index first**, then rewrite its ids
   or truncate it; stale derived indexes are dropped and rebuilt from the canonical index.
5. Rebuild derived system cubes (`#Members`, `#Edges`, `#Columns`, …) from the remapped metadata.
6. Drop caches; publish a new version; save atomically.
7. Report per dimension: live members, old span, new span, reclaimed slots; and totals (cells
   remapped, bytes saved on the next save, duration).

The **model identity must not change**: `verify-model` (content fingerprint) is run before and
after; a difference aborts and restores the backup. A second run is a no-op (idempotent).

### 25.3 Extensible step registry

| Step | Future use | Safety |
|---|---|---|
| `ids` | remove id gaps (this section) | offline; atomic |
| `nodes` | re-encode tries with adaptive/dense/compressed nodes (§22) | offline; atomic |
| `heap` | compact the CellId arena / reclaim dead cells | offline; atomic |
| `delta` | checkpoint and truncate the delta log | offline; atomic |
| `cache` | drop per-index caches (no file change) | online-safe |
| `system` | rebuild derived system cubes | online-safe when quiescent |
| `verify` | model fingerprint + referential checks | read-only |

Each step reports what it did; new steps are added to the registry rather than surfaced as new
commands. Order is fixed (`delta` → `ids` → `heap` → `nodes` → `system` → `verify`) so a later
step can rely on the previous one.

### 25.4 Why offline and user-triggered

- Renumbering touches **every index and every cell coordinate**; doing it under live readers
  would replace the whole catalogue, which the MVCC design (persistent tries, versioned caches)
  deliberately avoids. Offline it is a linear rebuild, with a backup and an atomic rename.
- It is rare (after churn, before a release, or when the span cap is pushing bitmap/dense
  choices); an automatic trigger would surprise operators and complicate save/release.
- **Release hygiene (decided 2026-11-02):** `promote-release` runs `maintain --steps all --backup`
  on the candidate **before** recording hashes, then re-runs `verify-model` (fingerprint must be
  unchanged), then swaps and broadcasts. A maintenance failure aborts the promotion with the
  candidate and the current release untouched. Until the maintenance command ships with the
  fact-set storage work, promotion stays explicit and must not silently claim hygiene it did not
  perform.

### 25.5 Acceptance

- `--dry-run` prints the per-dimension gap report and saves nothing.
- Running `ids` on the demo model (no deletions) changes nothing (no gaps; second run idempotent).
- On a synthetic dimension with 10,000 live members and ids to 50,000, `ids` yields span =
  10,000 and the model fingerprint is unchanged.
- Server-listening detection: refuses with a clear message; no file written.
- Backup + restore: a deliberately failing step leaves the original file intact; a successful run
  leaves `<file>.bak` (decision 3).
- `--steps all` is the default and reports each step; `--dry-run` writes nothing.
- Promotion hygiene: with maintenance available, the promoted hashes are from the maintained
  candidate; the model fingerprint before/after maintenance and after promotion is identical.

### 25.6 Decisions (2026-11-02)

1. **Surface: CLI only.** `maintain-db.cmd` / the engine `--maintain` flag. No server admin
   endpoint now; if one is ever added it needs the `admin` role and a quiescent instance, plus a
   contract handoff because it is a wire change.
2. **Default steps: `all`** — every registered step runs in the fixed order
   (`delta` → `ids` → `heap` → `nodes` → `system` → `verify`). `--steps ids` remains available
   for a targeted, cheap run.
3. **Backup: keep `<file>.bak`.** Maintenance writes the pre-run file to `<file>.bak` and leaves
   it; the next successful maintenance replaces it. Promotion keeps its own archive under
   `shared\previous\<stamp>\`; the two are separate and both retained.
4. **Promotion = release hygiene.** `promote-release` runs `maintain --steps all --backup` on the
   candidate before hashing/swapping, then re-runs `verify-model` (fingerprint must be identical),
   then records the new hashes. Failure aborts before anything is swapped.
5. **Trigger rights confirmed:** operator-only (offline CLI). A future remote surface, if ever
   added, is admin + quiescent; not part of this design.

`--dry-run` remains the way to judge a cleanup before applying it; it prints the gap report and
writes nothing.
## 26. Revision 6 — user-triggered cleanup recorded (2026-11-02)

Added §25: a step-based maintenance command, `ids` compaction algorithm and safety, an extensible
registry (nodes/heap/delta/cache/system/verify), offline-first invocation, acceptance tests, and
open choices. The id-gap decision in §22.9 item 3 now points at this step instead of "later".
## 27. Revision 7 — cleanup decisions (2026-11-02)

Recorded the operator's answers to §25.6: CLI only (no admin endpoint); default `--steps all`;
keep `<file>.bak`; promotion runs maintenance as release hygiene before hashing/swapping and
re-verifies the fingerprint; operator-only confirmed. §25.4 promotion flow and §25.5 acceptance
updated accordingly. The maintenance command itself ships with the fact-set storage work; until
then promotion remains explicit and must not claim hygiene it did not run.
## 28. Implementation status (2026-11-09)

Phase A has started on purpose, without the migration path: old catalogue files are demo data and
are regenerated, so the loader now accepts only the current format.

- **`69c9b36` — cell heap + `CellId` leaves.** `Node.value` is a `CellId`; `SparseStore` owns an
  append-only heap shared by snapshots; exact/scan/print resolve through it. This is the shared
  cell identity the multiple index tries need. Catalogue `FORMAT_VERSION = 3`; older formats are
  rejected, no migration branch, shared pair not upgraded in place.
- **Next:** index definitions + fact set + the second scheme (flat/tree views over one heap), lazy
  index load/delta log, per-index caches, delete intents, security mapping, benchmark. None of that
  is wired yet; the current cubes still own one trie each.
## 29. Fact set and attributes (operator, 2026-11-09)

A fact set **owns facts only**: its fact coordinates are dimension **keys** and its measures are
the cells in the heap. **Attributes are not facts.** Master/descriptive data lives in separate
attribute tables, keyed by the same member identities, and a fact set references them — it does
not copy them into fact rows.

- A fact row is `keys + measures` (e.g. `CustomerID`, `CityID`, `ProductID`, `DateID`); a customer
  name, city label, category or state is stored once in its attribute table.
- Attribute tables can be shared by several fact sets; the fact set declares which keys reference
  which attribute table. Attributes are resolved by key lookup, not stored per fact.
- Attribute tables are **non-aggregating cubes**, each with its **own trie** over its keys. They
  can be shared by several fact sets; the fact set declares which keys reference which attribute
  cube. Attributes are resolved through that attribute cube's trie, not stored per fact.
- **Fact-set indexes** cover fact keys and measures only. An attribute value never becomes a
  level in a *fact* trie; an attribute hierarchy is resolved through the attribute cube's own
  trie, and the fact set is then filtered by the corresponding key.
- A denormalised attribute in a fact is allowed only when the schema explicitly declares it a
  **snapshot** column; otherwise attributes are read-through and writable only in their table.
- Demo split: `Orders` facts = `RowID`, `OrderDateID`/`ShipDateID`, `CustomerID`, `CityID`,
  `ProductID`, `ShipMode`, `OrderPriority` + the four measures; `Date`, `Customer`, `Geography`
  and `Product` become attribute tables. Acceptance totals must be unchanged. City identity is a
  surrogate key (`CityID`), not the label, so the two Bristols remain distinct.

First implementation slice: attribute cube (non-aggregating, own trie) + `FactSet` key mapping, then the two-index
test over the split demo data. No wire change until the model release declares the schema.
