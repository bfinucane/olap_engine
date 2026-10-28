# Wire changes — read this first

**Who this is for:** the Plateau session. The engine's wire contract is the shared
interface, and this file is its changelog and index. `JOIN-HANDOVER.md` covers joins in
detail; everything else that changed is recorded here.
**Normative source:** `tableau-clone/docs/ENGINE-PROTOCOL.md` (theirs). Nothing here is
normative — it is the paste-ready text to put THERE, plus what a client must do about it.
**State:** engine HEAD `9a49cec` (2026-10-28) — `9740cf5` plus the wave-1 fixes in
`7fc893b` (wire), `a8bb732` (http) and `9a49cec` (sql). The protocol document was last
updated 2026-10-25, before the security and writeback work, so its paste-ready section
below is longer than usual.

---

## OPEN BREAKS — read these before pasting or building against the engine

1. **`datePart` is documented but REFUSED.** ENGINE-PROTOCOL.md §3 says `groupBy[].datePart`
   truncates to `year | quarter | month | week | day`, with no capability gate. This engine
   returns `400 unsupported_expression` for ANY non-empty `datePart` ("no temporal
   bucketing yet"). Plateau sends `datePart` for date shelves (`dataSource.ts:743`), so date
   grouping against this engine fails today. **NOT fixed in wave 1.** Until it is, Plateau
   must not offer date grouping on an engine dataset, or must compute it client-side; the
   engine must either implement truncation or add a capability field plus protocol text so
   a client can detect it.
2. **`maxRows` is advertised but not enforced.** `capabilities.maxRows` is `100000`, but the
   engine never truncates to it and never returns `row_limit`. A client must clamp its own
   requested limits (Plateau already does). Do not treat `maxRows` as a promise that a
   larger `limit` will be reduced.
3. **`value` + the measure dimension as an axis is not refused on the wire.** The SQL front
   end refuses `SELECT Measure, value …` ("Cannot select both measure dimension and
   specific measures."), but the plan/wire path has no such check: it renders the axis
   column as an empty string while pivoting the measures, i.e. a malformed row. **Do not
   send that combination.** Open defect, not fixed in wave 1.
4. **System cubes are not rebuilt by HTTP writes.** They are regenerated on catalogue load
   and after CLI commands, but `POST /v1/{dataset}/write` does not refresh them, so a
   long-lived server's `#Cubes`/`#Members`/`#Columns` can lag behind its own writes until
   the next load/CLI run. Reads of USER cubes are always current.
5. **PRE-EXISTING observation (not new in wave 1): a grouped query with NO measures on a
   cube that has a measure dimension returns each group once per MEASURE MEMBER.** For the
   Sales fixture, `groupBy:[{"name":"Region"}], measures:[], domainFor:["Region"]` answers
   rows `[["East"],["East"],["West"],["West"]]` (one per measure member at each group).
   Plateau's `domainFor` call reads only `response.domain`, so its filter-picker path is
   unaffected; a dimension-only table/query against such a cube would show the duplicate
   rows until this is fixed.

---

## How this is meant to be used

1. **At each pull**, read the changelog below. Each entry names the engine commit and says
   whether a client must change anything.
2. **At runtime, feature-detect** via `GET /v1/capabilities`. This is the safety net: a
   client that reads `aggregations`, `expressionFunctions`, `supportsJoin`,
   `countDistinctExact` and `supportsWrite` cannot silently offer a feature the engine
   lacks.
3. **Paste the normative blocks** from "The contract additions, paste-ready" into
   ENGINE-PROTOCOL.md on your side. The engine session will not edit that file — it is
   Plateau's.
4. When the engine changes the wire again, the entry is appended here, not sent as a
   message, so nothing is lost between sessions.

---

# The contract additions, paste-ready

> Everything in this part is written to be pasted into `ENGINE-PROTOCOL.md` as-is (one
> block per protocol section). The changelog that follows is history; this part is the
> text the protocol is missing.

> ### Transport (additional operations)
>
> | Operation | Method and path |
> | --- | --- |
> | `datasets` | `GET /v1/datasets` |
> | `write` | `POST /v1/{dataset}/write` |
> | `save` | `POST /v1/save` |
>
> These are additive to protocol v1 and do not change the JSON of `describe`, `sample`,
> `query` or `capabilities`. `datasets` is discovery. `write` and `save` are the writeback
> surface and are gated by `capabilities.supportsWrite`.

> ### §1 `describe` (additive fields, and one nullable field)
>
> - `isSystem: boolean` — the cube is engine-maintained metadata (a `#`-prefixed cube), not
>   user data. A cube picker should hide these; they are still listed by
>   `GET /v1/datasets`, deliberately, so a client discovers the database through one call
>   and filters with this flag.
> - `primaryKey` — MAY be `null` (or absent). This engine does not report a primary key
>   today; a client must not assume the field is always a string.
>
> `describe` is otherwise unchanged. `rowCount` is the number of stored leaf cells of the
> cube (for a cube with a measure dimension, one per measure member at each coordinate),
> which is the same quantity `sample.rowCount` reports.

> ### §2 `sample` — query-string encoding
>
> `limit`, `offset` and `columns` arrive as query-string VALUES and are percent-decoded
> before use: `+` means a space, `%XX` (two hex digits, either case) is one byte. A column
> name is data, so `columns=Row+ID` and `columns=Row%20ID` both name the column `Row ID`,
> while `columns=Row%2BID` names a column literally called `Row+ID`. A malformed escape —
> a `%` not followed by two hex digits, or bytes that are not valid UTF-8 — is
> `400 invalid_argument`, never a guessed or truncated name.
>
> `sample.rowCount` counts rows VISIBLE to the caller. With dimension access in force it is
> not the whole cube (see Access control).

> ### §3 `query` — the `value` measure
>
> On a cube that HAS a measure dimension, `measures: [{ "name": "value" }]` means ALL
> MEASURES: it expands to one measure column PER MEMBER of the measure dimension, in the
> dimension's display order, and `columns`, `measureNames` and `measureLabels` report the
> expanded names (`SUM(Sales)`, `SUM(Cost)`, …). Cells are the aggregated values; an absent
> cell is `null`. On a cube whose measure dimension is still empty, the expansion yields
> **0 rows** rather than an error — the honest answer for a table created but not yet
> populated.
>
> On a measure-less cube the existing meaning is unchanged: `value` IS the stored cell, and
> a query that names no measures gets that implicit `value` column.
>
> Do **not** also put the measure dimension in `groupBy`: the SQL front end refuses that
> combination, and the plan path currently renders it badly (see Open break 3). Send either
> `value` as a measure, or the measure-dimension members as measures, never both.

> ### §3 `query` — `domainFor`
>
> `domainFor` is honoured on EVERY query shape — grouped or not, with or without measures.
> When the request's `domainFor` is non-empty, the response `domain` map carries the full
> member list of each named column, computed by the SAME routine the pure-domain request
> uses, so the two forms cannot drift. A pure-domain request (`groupBy` and `measures` both
> empty) still answers with `domain` only: no rows, no columns.
>
> Dimension access applies to the domain map: a member the caller may not see is omitted,
> so folding the lists into a query cannot leak a picker value. The capability
> `supportsInlineDomain` stays `true`.

> ### §4 `capabilities` (additive field)
>
> - `supportsWrite: boolean` — whether `POST /v1/{dataset}/write` and `POST /v1/save` exist.
>   A client must check this before offering an editable grid or a Save action: an older
>   engine answers the unknown route with `invalid_argument`, which is a confusing way to
>   discover that writeback is absent.
>
> `maxRows` is INFORMATIONAL in this engine: it advertises `100000` but does not truncate
> to it and never returns `row_limit` (see Open break 2). A client that needs a hard bound
> must clamp its own limits.

> ### §4 `capabilities` — the `filterOperators` report, and its scope
>
> This engine reports `filterOperators = ["in", "equals", "notEquals"]`, and all three are
> evaluated for DIMENSION-scope filters:
>
> - `notEquals` is the complement of `equals` over the dimension's own members. Only LEAF
>   members are used, so a consolidation cannot re-admit an excluded leaf. A member that
>   does not exist is `400 invalid_argument`; if some stored leaf cannot be addressed
>   through the member list, the request is refused with `400 unsupported_filter` rather
>   than silently dropping rows.
> - A filter whose `field` names a HIERARCHY rather than a dimension is refused with
>   `unsupported_filter` by name — the slice keys filters by dimension, so such a filter
>   would otherwise be silently ignored.
>
> MEASURE-scope filtering is not implemented: `top`/`bottom` and every other measure-scope
> predicate are refused with `unsupported_filter`, and `filterOperatorsByScope` is not
> reported. A client must not offer a measure-scope filter on this engine.

> ### Errors — additional `code` values
>
> The v1 codes are unchanged. The writeback/security work adds these, with the HTTP status
> the engine returns:
>
> | `code` | Status | Meaning |
> | --- | --- | --- |
> | `forbidden` | 403 | The caller may not perform this operation (write to a cube it may not write, `POST /v1/save` without `admin`). Never used for a read: a read of a hidden cube is `unknown_dataset` / 404. |
> | `unknown_member` | 400 | A write named a member that does not exist and `createMissing` was false. |
> | `consolidated_write` | 400 | A write targeted a consolidated coordinate without choosing a `splash` mode. |
> | `save_failed` | 500 | `POST /v1/save` could not write the catalogue file. |
> | `method_not_allowed` | 405 | Wrong method on `POST /v1/login` only. Every other wrong-method route answers `400 invalid_argument`, the engine's general "bad request" answer. |
>
> Map an unknown `code` to `engine_error` (retry/report), never to a client bug: the list
> above is closed for this engine, but a future engine may add more.

> ### §5 `datasets` → `DatasetsResponse`
>
> ```json
> {
>   "datasets": [
>     "#Columns", "#Cubes", "#DimensionSecurity", "#Dimensions",
>     "#Edges", "#Hierarchies", "#Members", "#TableSecurity",
>     "Markets", "Sales"
>   ]
> }
> ```
>
> The names are sorted and keep the catalogue's EXACT casing. A client uses each name
> verbatim as `{dataset}` in every other route (`GET /v1/{name}`), so a case-folded or
> reordered answer is a bug, not cosmetics. System cubes (`#` prefix) are included
> deliberately: a client discovers the whole database in one call and hides them with
> `describe.isSystem`. Cubes the caller may not read are OMITTED, not refused.
>
> `GET` only; any other method is `400 invalid_argument`. A `#` name must be
> percent-encoded in a URL path: `GET /v1/%23Cubes`.

> ### §6 `write` → `WriteResponse`
>
> The engine can be written to. A batch is ATOMIC: every cell is validated before any cell
> is applied, so a refused batch changes nothing. `splash` and `createMissing` are the
> CLIENT's choices; the server only vetoes, through access rules.
>
> ```http
> POST /v1/{dataset}/write
> Content-Type: application/json
>
> {"cells":[{"coords":{"Region":"East","Measure":"Sales"},"value":123.45}],
>  "splash":"reject", "createMissing":false}
>
> 200 {"written":1,"version":43}
> ```
>
> - `cells` — 1 to 10,000 cells; an empty or oversized batch is `invalid_argument`.
> - `coords` — a `{dimension: member}` map with exactly ONE entry per cube dimension
>   (dimension names are case-insensitive). Members resolve in the dimension's DEFAULT
>   hierarchy; a consolidated member that exists only in another hierarchy cannot be
>   addressed yet.
> - `value` — a number, or a string for an attribute table (`isAttributeTable: true`).
>   Text written into an aggregating cube is `invalid_argument`: the accumulator treats a
>   text cell as "last value wins" and would silently replace that group's total. Numbers
>   are allowed in attribute tables (those cubes never aggregate).
> - `splash` — `reject` (default) refuses a write to a CONSOLIDATED coordinate; `replace`
>   distributes the value so the rollup total becomes it; `add` distributes the value as a
>   delta on top of the current leaves. Case-insensitive. The engine never picks a mode for
>   you.
> - `createMissing` — default **false**: an unknown member is `unknown_member`. `true`
>   creates the member; an engine access rule may still veto creation.
> - `version` — the cube's data version after the write. INFORMATIONAL: the contract is
>   last-write-wins, so it is not a precondition for the next write.
>
> Errors: `400 invalid_argument | unknown_member | consolidated_write`, `403 forbidden`,
> `404 unknown_dataset`. A hidden cube is 404 on reads but **403 on writes** — a refused
> write must be loud rather than look like a missing cube.
>
> Derived system cubes are refused with `invalid_argument` (the engine rebuilds them, so a
> write would vanish). The security control cubes (`#TableSecurity`, `#DimensionSecurity`)
> are refused with `forbidden`: they are configuration, changed through an admin SQL
> session, not through the cell API.
>
> Permissions (when authentication is on): the caller needs table `WRITE` for the dataset
> (a `READ`-only or `NONE` rule is `403 forbidden`), and dimension `WRITE` for every
> coordinate it changes. Creating a member needs `WRITE` for the WHOLE dimension, because a
> per-member rule cannot exist for a member that does not exist yet.

> ### §7 `save` → `SaveResponse`
>
> ```http
> POST /v1/save
>
> 200 {"saved":true,"path":"database.bin"}
> ```
>
> Persists the in-memory catalogue to the file the server was started with (`--db`). Writes
> live in memory only until an explicit save: there is no debounce and no implicit save.
> Requires the `admin` role when authentication is on (`403 forbidden` otherwise); in
> development mode (`--allow-anonymous`) any caller may save. If the file cannot be written:
> `500 save_failed`.

> ### Authentication — the wire details (already merged; restated for the writeback era)
>
> - `POST /v1/login` is the only token issuer. Body `{"user": "...", "password": "..."}`.
>   `200 {"token": "<64 hex chars>", "user": "...", "roles": ["..."], "expiresAt": 1792805349}`
>   — `expiresAt` is Unix SECONDS and the session lifetime is 12 hours. Sessions live in
>   memory: a restart logs everyone out. There is no refresh endpoint; re-login is the
>   refresh.
> - Wrong password and unknown user return the SAME `401 {"error":"Invalid user name or
>   password.","code":"unauthorized"}` by design (a decoy hash is computed for unknown
>   users so timing does not leak either).
> - A malformed or incomplete login body is `400 invalid_argument`; a non-POST on
>   `/v1/login` is `405 method_not_allowed`.
> - Every route except `GET /v1/capabilities` requires `Authorization: Bearer <token>`.
>   Missing, malformed, unknown and expired tokens all return ONE body:
>   `401 {"error":"Authentication required. POST /v1/login to obtain a bearer token.","code":"unauthorized"}`.
>   There is NO `WWW-Authenticate` header — do not wait for a challenge; switch on the
>   status and `code`.
> - Auth is checked BEFORE routing: an unauthenticated request to an unknown path is also
>   401, so it cannot be used to probe which datasets exist.
> - `GET /v1/capabilities` is public (feature flags only, never data). A token that IS
>   presented there must still be valid — a stale token gives 401 — so a public probe
>   should omit the header when it has no session.
> - CORS: with `--cors`, `Access-Control-Allow-Headers: Content-Type, Authorization`;
>   `OPTIONS` preflight answers 204. CORS is off by default.
> - `GET /sql` (the console PAGE) is unauthenticated engine tooling, explicitly not part of
>   this contract. `POST /v1/sql` requires a token like any other data route.

> ### Access control on the wire (additive, optional — this engine implements it)
>
> Table access is the control cube `#TableSecurity(#Role, #Cube) -> READ | WRITE | NONE`.
> The default is ACCEPT: a cube with no rule is visible and writable. Rules only restrict,
> and the most restrictive explicit rule across the caller's roles wins.
>
> - `GET /v1/datasets` OMITS cubes the caller may not read.
> - `GET /v1/{dataset}`, `/sample`, `/query`, `POST /v1/sql` (any statement naming the
>   cube, including joins and subqueries) and a query's `join` respond **404
>   `unknown_dataset`** for a hidden cube: byte-for-byte the same shape a missing cube
>   gives. A client cannot tell "deleted/typo" from "not available to you", and must not
>   try.
> - `POST /v1/{dataset}/write` to a hidden cube is **403 `forbidden`** instead, because a
>   refused write must be loud. Consequence: a write probe CAN distinguish hidden from
>   absent; do not use writes to test whether a cube exists.
>
> Dimension access is `#DimensionSecurity(#Role, #Dimension, #Member) -> NONE | READ |
> WRITE`; the member `#ALL` means the whole dimension. A denied MEMBER is absent, not an
> error: query rows for it disappear and every aggregate excludes its cells (a grand total
> is smaller), `sample` filters the cells before `offset`/`limit` and `rowCount` counts
> visible rows, member lists omit it — including the inline `domain` map folded into a
> normal query, so a picker list cannot reveal a denied member — and SQL sees the same
> reduced totals. A dimension denied in FULL (`#ALL` = NONE) hides every cube that uses it:
> 404 and omitted from `/v1/datasets`. `describe` is unchanged — a column's `cardinality`
> still counts the whole dimension, so it can reveal that hidden members exist (not their
> names); treat it as a metadata hint, not a permission boundary.
>
> Reads must never be used to infer whether a hidden cube or member exists: by design there
> is no difference in the answer.

> ### System cubes (`#` prefix)
>
> Cubes whose name starts with `#` are engine-maintained metadata, not user data:
> `#Cubes`, `#Dimensions`, `#Hierarchies`, `#Members`, `#Edges`, `#Columns`, plus the
> writable control cubes `#TableSecurity` and `#DimensionSecurity`. They are listed by
> `/v1/datasets` and flagged by `describe.isSystem`; they answer ordinary `query`/`sample`
> requests (they are cubes), and `#Columns` describes every cube's schema, including the
> system cubes themselves. The six derived cubes are READ-ONLY through the cell API
> (`invalid_argument`); the two control cubes are written only through an admin SQL session
> (`403 forbidden` through the cell API). `#` must be percent-encoded in a URL
> (`/v1/%23Cubes`). Note that the control cubes are not read-restricted beyond table
> access: a role that can read them can see the rules. They are configuration, not a
> secret store.

---

# Changelog (newest first)

### Wave 1 (`7fc893b`, `a8bb732`, `9a49cec`) — sample encoding, honoured `domainFor`, honest `filterOperators`, serve/SQL docs

- **Wire:** `sample`'s query-string values (`limit`, `offset`, `columns`) are now
  percent-decoded in one place: `+` becomes a space, `%XX` is one byte (hex either case),
  and a malformed escape is `400 invalid_argument`. This fixes `columns=Row+ID` /
  `columns=Row%20ID` for any column name a client must URL-encode. The path decoder is
  unchanged (`%23Cubes` still works; a `+` in a path stays a literal plus).
- **`domainFor` is honoured on every query shape.** A grouped query (with or without
  measures) now fills the response `domain` map whenever `domainFor` is non-empty. It is the
  dimension's FULL member list from the same routine the pure-domain form uses, and it
  honours dimension access (denied members omitted). `capabilities.supportsInlineDomain`
  stays `true`. No JSON shape changed. Pinned by
  `query_domain_for_is_honoured_when_group_by_is_present` (no-measures and measures shapes,
  two `domainFor` columns, parity with the pure-domain answer);
  `query_domain_for_returns_the_domain_map` is still green.
- **`filterOperators` is honest:** `["in", "equals", "notEquals"]`, and all three are
  evaluated — for DIMENSION-scope filters. `notEquals` is the complement of `equals` over
  the dimension's own members (leaf members only, so a consolidation cannot re-admit an
  excluded leaf). An absent member is `invalid_argument`; unaddressable leaves or a
  hierarchy used as the filter `field` are `unsupported_filter` by name rather than a
  silently dropped predicate. Measure-scope filters remain unsupported (see §4 above). No
  JSON shape changed. Pinned by `query_every_advertised_filter_operator_evaluates`,
  `not_equals_filter_keeps_everything_but_the_listed_member`.
- **No wire shape changed by the wave-1 work.** `serve`'s own route/auth comments were
  corrected (they still described the pre-writeback server).

### `aa3fd91` — `value` means ALL MEASURES when the cube has a measure dimension

- **Wire:** `measures: [{"name": "value"}]` used to be refused ("'value' is not a measure")
  on a cube with a measure dimension. It now expands to **one column per measure member**,
  in display order, with `measureNames`/`measureLabels` set accordingly. An absent cell is
  `null`. An empty measure dimension expands to nothing, so the query returns **0 rows**
  rather than an error — the honest answer for a table created and not yet populated
  (`describe` reports `rowCount: 0`, `cardinality: 0`). Measure-less cubes are unchanged:
  there `value` IS the stored cell. SQL pivots the same way (`Region | Sales | Cost`) and
  refuses the combination with a measure-dimension axis; **the wire does not refuse it
  yet** — see Open break 3.
- **Also fixed in that pass:** a generated default member (the filter injected for a
  dimension a query leaves unnamed) is used only if it exists in the dimension's default
  hierarchy, so a multi-hierarchy dimension no longer produces `Member 'Furniture' not
  found` on an ordinary query.

### `e0dc743` (base), `f849a47`, `0160454`, `9740cf5` — writeback and persistence

- **Wire additions:** `POST /v1/{dataset}/write` (batched, atomic, client-chosen `splash`
  and `createMissing`, `200 {written,version}`, last-write-wins) and `POST /v1/save`
  (explicit persistence, `admin` when auth is on). `capabilities.supportsWrite` advertises
  both. Full paste-ready text is §6 and §7 above. A client must feature-detect
  `supportsWrite` before offering an editable grid.
- `f849a47`: text (`string`) values go to ATTRIBUTE tables only; a string written into an
  aggregating cube is `invalid_argument`, because the accumulator's text cell is "last
  value wins" and would silently replace the group total. `0160454`: the security control
  cubes are `forbidden` through the cell API (configuration, not data). `9740cf5`: the CLI
  importers refuse text in numeric cubes too, so both front doors agree.
- **Behaviour change from security:** a query's attribute `join` is access-checked against
  the joined cube — a role denied that cube gets `404 unknown_dataset` for the join.

### `6375b0d` — dimension access: rows and totals change, nothing errors

- A role can be denied a MEMBER of a dimension (or the whole dimension with `#ALL`).
  Unlike table access there is no 404 for a member: the data is simply not there. Query
  rows for a denied member are absent and every aggregate over it excludes its cells (a
  grand total is smaller, not an error); `sample` filters before `offset`/`limit` and
  `rowCount` counts visible cells; `domainFor`/member lists omit it; SQL sees the same
  reduced totals. `#ALL` denial: every cube using that dimension answers `404
  unknown_dataset` and is omitted from `/v1/datasets`. `describe` is unchanged
  (`cardinality` counts the whole dimension — a metadata hint, not a permission boundary).
- `#DimensionSecurity(#Role, #Dimension, #Member)` is a new admin-writable control cube,
  listed in `#Columns` and `/v1/datasets`. A client needs no code change to be safe; it
  just receives smaller numbers, and should NOT try to distinguish "no data" from "no
  access" — by design there is none.
- Paste-ready text: see "Access control on the wire" above.

### `ced7534` — table access: hidden cubes look non-existent

- A role may be denied a cube (`#TableSecurity`). The wire makes that invisible rather than
  returning 403: `GET /v1/datasets` omits it, and `GET /v1/{cube}`, `/sample`, `/query` and
  `POST /v1/sql` (any statement naming the cube, including joins and subqueries) answer
  **404 `unknown_dataset`**, byte-for-byte the same shape as a cube that does not exist.
- `#TableSecurity(#Role, #Cube) -> READ|WRITE|NONE` is a new system cube, listed in
  `#Columns`. With no rule the default is ACCEPT; rules restrict, they do not grant.
- Clients that key on `unknown_dataset` to mean "deleted/typo" should treat it as "not
  available to you" — the engine deliberately does not distinguish. Paste-ready text: see
  "Access control on the wire" above.

### `4428f8d` (S1), `afc235c` — authentication: login, bearer tokens, `authRequired`

- Every `/v1` route except `POST /v1/login` and `GET /v1/capabilities` requires
  `Authorization: Bearer <token>`. Capabilities is deliberately PUBLIC: it is how a client
  discovers that a token is needed (`authRequired`), so protecting it would hide the answer
  exactly when it matters. It exposes feature flags only, never data.
- Missing, malformed, unknown and expired tokens all return **401** with one body
  (`{"error": "Authentication required. …", "code": "unauthorized"}`); wrong password and
  unknown user are indistinguishable at login; sessions live in memory (12 hours);
  `expiresAt` is Unix seconds; there is no refresh endpoint.
- `afc235c`: auth runs BEFORE routing (an unknown path is 401 too, so it cannot probe
  datasets), and `--cors` now allows `Authorization` in `Access-Control-Allow-Headers`.
  Creating users is a CLI action, not a wire one: `serve.exe --add-user admin --role admin`
  (prompts), `serve.exe --list-users`.
- Paste-ready text: see "Authentication" above.

### `b0b53e7`, `a5a76c9` — system cubes (`#` prefix) and `describe.isSystem`

- A front end can list the database with an ordinary query: `#Cubes`, `#Dimensions`,
  `#Hierarchies`, `#Members` are read-only cubes describing the catalogue itself; `#Edges`
  adds parent/child pairs with rollup weights, and `#Columns` is the schema of everything,
  including the system cubes. `#` names are written bare in SQL and percent-encoded in a
  URL (`GET /v1/%23Cubes`).
- They are NOT default-filled by SQL (metadata reads as "all rows unless filtered"), they
  describe USER content only (no self-referential listings), and they appear in
  `/v1/datasets` deliberately. `describe` carries **`isSystem: boolean`** so a cube picker
  can hide them.
- They are read-only. `INSERT INTO #Cubes …` is refused: "'#Cubes' is a system cube: it is
  maintained by the engine and is read-only."
- Refresh model: rebuilt on catalogue load and after every CLI command. **The HTTP server
  does not rebuild them after a write**, so a long-lived server's system cubes can lag its
  own writes (Open break 4) — the stale line in the previous revision ("The HTTP server
  never writes") is gone: the server DOES write now.

### `e99f5c8` — calculated fields are EVALUATED

`Measure.expr` (an inline AST, or a name into `expressions`) used to be refused with
`unsupported_expression`. It now runs. **No protocol field changed** — the AST shape in
`plan::AstNode` already matched `src/core/expression.ts` tag for tag, so the request you
already send works.

- Supported now: aggregates `SUM/AVG/COUNT/MIN/MAX` per field, arithmetic, comparisons,
  `AND/OR/NOT`, `IF`, `ZN`, `IFNULL`.
- Still refused: LOD (`supportsLod` stays `false`) and every function not in
  `expressionFunctions`.
- `capabilities.expressionFunctions` is `["ZN","IFNULL"]` — **advertise exactly what runs.**
  A client showing a formula box should offer only these plus the aggregates.
- **One aggregation per field**: `SUM([x]) / AVG([x])` is refused by name. If your formula
  editor would produce that, disable it rather than let the user hit the error.
- Semantics worth surfacing in the UI: division by zero is **absent**, not `0`; an absent
  input makes the expression absent (that is what `ZN`/`IFNULL` exist for); an absent `IF`
  condition yields absent rather than the `ELSE` branch.

### `d5d282d` — SQL `LIMIT`/`OFFSET`

Engine-internal: the SQL front end only. **No client change.** (Plateau does not send SQL.)

### `d2fcf41` / `90273b0` — a read-only SQL console at `/sql`

Engine-internal tooling: `GET /sql` (page) and `POST /v1/sql` (`{"sql":"SELECT …"}`,
SELECT only, access-checked like any data route). It is **not part of the client contract**
and Plateau need not implement anything. It exists to debug the engine by hand. A SQL box
belongs in Plateau, where the formula language lives.

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
`describe.measureDimension`, `describe.isAttributeTable`, and the `join` block
(`{cube, using?, attributes[]}`; attributes append as output columns, a missing attribute
value is JSON `null`).

---

## What Plateau should do, in order

1. **Read `capabilities` before enabling anything.** If that sounds like boilerplate, note
   that the reason `supportsJoin` exists is that an old engine ignores a `join` block with
   no error at all — the user gets un-joined data and believes it; and an old engine ignores
   `supportsWrite` the same way.
2. **Handle `null` measure cells.** This is the only change here that can corrupt a display
   without an error.
3. **Offer the aggregations you find in `aggregations`**, not a fixed list.
4. **Calculated fields are the engine's job** if you send the AST: your parser produces
   `AstNode` already, so no work is needed on the request side — but stop computing
   expressions client-side for anything the engine accepts, or the two will disagree.
5. **Do not offer date grouping on an engine dataset** until Open break 1 is closed.
6. **`domainFor` and `notEquals` are now honoured** (wave 1) — no workaround needed for
   either. The filter operators are DIMENSION-scope: do not offer a measure-scope filter
   until the engine implements one.

## Keeping this current

Every change that alters a request/response field, a capability, or the meaning of a value
gets an entry here **in the same commit that makes it**, with the engine SHA. One file,
append-only, newest first. If this file and ENGINE-PROTOCOL.md disagree, the protocol wins —
and the disagreement is a bug worth raising. When a change is still in the working tree,
name the HEAD it is built on and say "uncommitted"; replace that with the real SHA once it
lands.
