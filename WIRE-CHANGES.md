# Wire changes — read this first

**Who this is for:** the Plateau session. The engine's wire contract is the shared
interface, and this file is its changelog and index. `JOIN-HANDOVER.md` covers joins in
detail; everything else that changed is recorded here.
**Normative source:** `tableau-clone/docs/ENGINE-PROTOCOL.md` (theirs). Nothing here is
normative — it is the paste-ready text to put THERE, plus what a client must do about it.

---

## How this is meant to be used

1. **At each pull**, read the changelog below. Each entry names the engine commit and says
   whether a client must change anything.
2. **At runtime, feature-detect** via `GET /v1/capabilities`. This is the safety net: a
   client that reads `aggregations`, `expressionFunctions`, `supportsJoin` and
   `countDistinctExact` cannot silently offer a feature the engine lacks.
3. **Paste the normative blocks** into ENGINE-PROTOCOL.md on your side. The engine session
   will not edit that file — it is Plateau's.
4. When the engine changes the wire again, the entry is appended here, not sent as a
   message, so nothing is lost between sessions.

---

## Changelog (newest first)

### `value` means ALL MEASURES when the cube has a measure dimension

- **Wire:** `measures: [{"name": "value"}]` used to be refused ("'value' is not a measure")
  on a cube with a measure dimension. It now expands to **one column per measure member**,
  in display order, with `measureNames`/`measureLabels` set accordingly. An absent cell is
  `null`.
- **SQL:** `SELECT Region, value FROM T` used to break the result out by measure into
  separate ROWS under a column named `value`. It now pivots the same way as the wire:
  `Region | Sales | Cost`.
- **An empty measure dimension expands to nothing**, so the query returns **0 rows** rather
  than an error — the honest answer for a table with no data yet. A table created and not
  yet populated is a normal cube; `describe` reports `rowCount: 0` and `cardinality: 0`.
- **`value` cannot be combined with the measure dimension as an axis**: they contradict, and
  the request is refused with "Cannot select both measure dimension and specific measures."
- Measure-less cubes are unchanged: there `value` IS the stored cell.

**Also fixed:** a generated default member (the filter injected for a dimension a query
leaves unnamed) is now only used if it exists in the dimension's **default hierarchy**. A
multi-hierarchy dimension could have its first-created member in another hierarchy, which
produced `Member 'Furniture' not found` on a perfectly ordinary query.

### system cubes (`#` prefix) and `describe.isSystem`

A front end can now **list the database** with an ordinary query: `#Cubes`, `#Dimensions`,
`#Hierarchies` and `#Members` are read-only cubes describing the catalogue itself. They
are the "lists" a spreadsheet front end needs, and the dictionaries the security work will
build on.

- **New datasets:** `#Cubes(#Cube, #Property)`, `#Dimensions(#Dimension, #Property)`,
  `#Hierarchies(#Dimension, #Hierarchy)`, `#Members(#Dimension, #Hierarchy, #Member)`,
  `#Edges(#Dimension, #Hierarchy, #Member, #Parent)`. All measure-less: their value is a
  string, and a query appends the usual implicit `value` column (`#Edges` carries the
  rollup weight as a number).
- **Structure, not just lists:** `#Members` is keyed by hierarchy because a consolidated
  member exists only in the hierarchies that define it; `#Edges` gives parent/child pairs
  with the weight, which is everything a client needs to fold a tree. Roots are the members
  that never appear as a `#Member` in `#Edges`.
- **System cubes are not default-filled.** SQL normally injects the default member of every
  dimension a query does not name; for `#` cubes that silently reduced a listing to one
  arbitrary cell. Metadata reads as "all rows unless filtered".
- **They appear in `/v1/datasets`**, deliberately — a client discovers the database through
  that list. `describe` now carries **`isSystem: boolean`** so a cube picker can hide them.
- **`#` must be percent-encoded in a URL**: `GET /v1/%23Cubes`. The engine now decodes the
  path, so a raw `#` would also work if a client could send one (HTTP clients treat it as a
  fragment and usually cannot). In SQL the name is written bare: `SELECT … FROM #Cubes`.
- **They are read-only.** `INSERT INTO #Cubes …` is refused: "'#Cubes' is a system cube: it
  is maintained by the engine and is read-only."
- **They describe USER content only** — system cubes do not list themselves or their
  dictionary dimensions, because a member list containing its own contents is
  self-referential.
- **Refreshed** on load and after every CLI command. The HTTP server never writes, so its
  system cubes cannot go stale while it runs.


### `e99f5c8` — calculated fields are EVALUATED

`Measure.expr` (an inline AST, or a name into `expressions`) used to be refused with
`unsupported_expression`. It now runs. **No protocol field changed** — the AST shape in
`plan::AstNode` already matched `src/core/expression.ts` tag for tag, so the request you
already send works.

- Supported now: aggregates `SUM/AVG/COUNT/MIN/MAX` per field, arithmetic, comparisons,
  `AND/OR/NOT`, `IF`, `ZN`, `IFNULL`.
- Still refused: LOD (`supportsLod` stays `false`) and every function not in
  `expressionFunctions`.
- `capabilities.expressionFunctions` is now `["ZN","IFNULL"]` — **advertise exactly what
  runs.** A client showing a formula box should offer only these plus the aggregates.
- **One aggregation per field**: `SUM([x]) / AVG([x])` is refused by name. If your formula
  editor would produce that, disable it rather than let the user hit the error.
- Semantics worth surfacing in the UI: division by zero is **absent**, not `0`; an absent
  input makes the expression absent (that is what `ZN`/`IFNULL` exist for); an absent `IF`
  condition yields absent rather than the `ELSE` branch.

### `d5d282d` — SQL `LIMIT`/`OFFSET`

Engine-internal: the SQL front end only. **No client change.** (Plateau does not send SQL.)

### `d2fcf41` / `90273b0` — a read-only SQL console at `/sql`

Engine-internal tooling: `GET /sql` (page) and `POST /v1/sql` (`{"sql":"SELECT …"}`,
SELECT only). It is **not part of the client contract** and Plateau need not implement
anything. It exists to debug the engine by hand. A SQL box belongs in Plateau, where the
formula language lives.

### `6569921` / `e7c49ae` — four more aggregations, and absence becomes `null`

- `capabilities.aggregations` is now `["SUM","AVG","COUNT","MIN","MAX"]`. Anything absent
  from that list is refused with `unsupported_aggregation` — **do not offer it**.
- **`capabilities.countDistinctExact` flipped `true` → `false`.** It was claiming an exact
  distinct count for a `COUNTD` that never existed. If your UI read it, it should now label
  distinct counts as unavailable (or just not offer COUNTD).
- **A measure with no cells in a group now arrives as `null`, not the string `"-"`.**
  This is a conformance fix (an absent value is not a dash), but it is the one change that
  can break a client silently: anything doing `String(cell)` or `cell.length` must handle
  `null`. Recommended rendering: an em dash or empty cell.
- Still true from before: strings for dimensions and attributes, numbers for measures.

### `1813c65` — join discovery fields

See `JOIN-HANDOVER.md`. Short version: `capabilities.supportsJoin`,
`describe.measureDimension`, `describe.isAttributeTable`, and the `join` block.

---

## The contract additions, paste-ready

> ### §4 `capabilities` (additive fields)
>
> - `supportsJoin: boolean` — whether `join` is honoured. An engine without it ignores a
>   `join` block **silently**, so a client must check before offering the feature.
> - `aggregations: string[]` — the aggregation names that run. Anything absent is refused.
> - `expressionFunctions: string[]` — the non-aggregate functions a calculated field may
>   use. Currently `["ZN","IFNULL"]`.
> - `countDistinctExact: boolean` — `false` while `COUNTD` is unimplemented.
> - `supportsLod: boolean` — `false`; LOD expressions are a second query, not an expression.
>
> ### §1 `describe` (additive fields)
>
> - `measureDimension: string | null` — the cube's measure dimension. For an attribute cube
>   this names the column whose **members** are the attachable attribute names.
> - `isAttributeTable: boolean` — this cube is an attribute source, not a measure cube.
>
> ### §3 `measures[]` — calculated fields
>
> A measure is either `{ "name", "aggregation" }` or `{ "name", "expr" }`, where `expr` is
> an AST (`{"type":"binary","op":"-","left":…,"right":…}`) or a key into the request's
> `expressions` map. Supported AST nodes: `number`, `boolean`, `null`, `field` (inside an
> aggregate only), `unary`, `binary`, `call`, `if`. A `call` may be an aggregating function
> (`SUM/AVG/COUNT/MIN/MAX`, one field argument) or a `capabilities.expressionFunctions`
> entry. An unsupported function is `unsupported_expression` naming it and listing what is
> supported — never a silent zero.
>
> Semantics: division by zero is `null`; an absent input makes the result `null` (use
> `ZN`/`IFNULL` to convert); an absent `IF` condition is `null` rather than the `ELSE`
> branch; one aggregation per field per expression.
>
> ### §3 response — absent values
>
> A measure cell with no data in the group is `null`. The engine's internal marker `"-"`
> is not sent to clients.

---

## What Plateau should do, in order

1. **Read `capabilities` before enabling anything.** If that sounds like boilerplate, note
   that the reason `supportsJoin` exists is that an old engine ignores a `join` block with no
   error at all — the user gets un-joined data and believes it.
2. **Handle `null` measure cells.** This is the only change here that can corrupt a display
   without an error.
3. **Offer the aggregations you find in `aggregations`**, not a fixed list.
4. **Calculated fields are now the engine's job** if you send the AST: your parser produces
   `AstNode` already, so no work is needed on the request side — but stop computing
   expressions client-side for anything the engine accepts, or the two will disagree.
5. **Decide where the formula console lives.** Engine session's view: Plateau, because the
   parser is there. The engine's `/sql` page is a debugging tool, not a product surface.

---

## Keeping this current

Every change that alters a request/response field, a capability, or the meaning of a value
gets an entry here **in the same commit that makes it**, with the engine SHA. One file,
append-only, newest first. If this file and ENGINE-PROTOCOL.md disagree, the protocol wins —
and the disagreement is a bug worth raising.
