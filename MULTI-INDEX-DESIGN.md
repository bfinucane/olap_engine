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
4. **Delete UX: open** — examples in §15 to evaluate.
5. **`viewOf` marker: yes**, mark it on the wire (§16).
6. Dimension counts/benchmark unchanged (§6); per-index caches confirmed.

Consequence of (1): fact identity is the canonical **flat key** (the key columns + measures),
not a hierarchy leaf. The tree view derives a hierarchy leaf from those keys, and the tree's
consolidations are pure metadata. This makes the "what is data vs shape" question sharper, not
simpler — see §15.

## 12. Why attributes break the flat model

The demo columns `Customer ID` and `Customer Name` are master data: the name belongs to the
customer, not to each order line. Three concrete shapes, to compare:

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

## 17. Delta-log example (lazy loading)

One record per write, canonical coordinate + new cell token (or tombstone):

```
{"seq":1042,"op":"upsert","fact":["D-2024-01-05","C-001","G-Bristol-TN","P-42"],"cell":918273}
{"seq":1043,"op":"delete","fact":["D-2024-01-06","C-002","G-Bristol-VA","P-17"],"cell":null}
```
A stale index records the last `seq` it incorporates. On load it either replays the delta or
rebuilds from the canonical index (batch path); the delta is compacted at save/checkpoint.

## 18. SQL keyword — examples to evaluate

If the goal is flat-table SQL convenience, two candidates:

**(a) Standard `ROLLUP`:**

```sql
SELECT Year, Quarter, Month, SUM(Sales)
FROM OrdersFlat
GROUP BY ROLLUP(Year, Quarter, Month)
```
One result set with subtotals (NULL marks the rolled-up level). No new storage; the engine's
SQL front end would implement the grouping sets.

**(b) An engine-specific level clause**, e.g. asking for the hierarchy's levels as columns:

```sql
SELECT LEVELS(Date), SUM(Sales) FROM OrdersTree GROUP BY LEVELS(Date)
```
returns `(Year, Quarter, Month, SUM)` per fact with its ancestors. This is the "flat view over
the tree" without a second index; it is the cheapest way to get your quarter/month columns.

The concrete failing query you have in mind determines which is needed; `ROLLUP` is standard and
understood by SQL users, the level clause is engine-native.

## 19. Open decisions after revision 2

1. Star-schema enforcement: option 0/1/2/3 (§13), and if option 1/2, what happens on a missing
   master member (refuse / autocreate / quarantine)?
2. Are master attributes read-only through the fact view, or may they be snapshotted?
3. City identity: qualified names, surrogate keys + attribute exposure, or display-name support
   in the dimension model (§14)?
4. Delete UX (§15): allow `RESHAPE` on a label that is also fact data? require fact-grain
   `PURGE`?
5. SQL keyword: `ROLLUP` or level clause (§18)?
6. Benchmark 10⁶ facts × depth 20/30/40 before promising a wide flat cube (§6).