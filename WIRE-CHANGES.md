# Wire changes — engine-side change feed

**Normative contract:** `contract/PROTOCOL.md` at the workspace root — owner-controlled,
**contract v1** (`protocolVersion 1`). It is the single source of truth for every route,
request/response shape, status code and access rule. **This file is not normative and is
not paste-ready text.**
**What this file is:** the **engine-side per-commit change feed**. Each entry names the
engine commit that changed the wire, says what changed, and says what a client must do.
Use it to audit the diff between two engine commits and to decide which contract version a
client must pin.
**Who reads it:** all three client sessions (Plateau, MiniSheet, Object Explorer) and
anyone auditing the engine's wire surface.
**Stale mirrors:** `tableau-clone/docs/ENGINE-PROTOCOL.md` (last touched 2026-10-25) is a
frozen copy from before auth, access control, writeback and COUNTD. It is **not
normative**; a client that keeps it MUST replace it with a pin to contract v1 and MUST NOT
treat it as authority. The engine session does not edit client trees — contract changes
are requested through `contract/handoffs/` (e.g. `001-plateau-contract-v1.md`) and applied
by the client session.
**State:** engine HEAD **`ae5e2c9`** (2026-10-28, 9 commits local, unpushed); contract v1;
engine conformance green at **49 passed / 0 failed / 1 skipped**. Engine-side join notes
live in `JOIN-HANDOVER.md` (read-only context, not normative).

---

## OPEN BREAKS — read these before building against the engine

These are the known gaps between the engine and contract v1, mirrored from
`contract/PROTOCOL.md` §11 (same numbering). They are **not promises**: a fix lands as a
changelog entry below, and the contract section is marked RESOLVED. A client MUST NOT build
a feature on a broken behaviour.

1. **§11.1 `datePart` is refused.** `groupBy[].datePart` parses but any non-empty value is
   `400 unsupported_expression` ("no temporal bucketing yet"); no capability flag detects
   it. Clients MUST NOT send `datePart`; compute date grouping client-side or do not offer
   it.
2. **§11.2 `maxRows` is advertised but not enforced.** `capabilities.maxRows` is `100000`,
   but the engine neither clamps to it nor emits `row_limit`. Clients MUST clamp their own
   limits and MUST NOT treat `maxRows` as a promise.
3. **§11.3 Measure-scope filters are refused, and `filterOperators` is global-only.** The
   engine reports `["in","equals","notEquals"]` but evaluates them only in **dimension**
   scope and does not report `filterOperatorsByScope`. A client that falls back to the
   global list for measure scope will pass its own validation and then get
   `400 unsupported_filter`. Clients MUST treat measure scope as unsupported
   (`top`/`bottom` included) until this closes.
4. **§11.4 `value` + the measure dimension as an axis is malformed on the wire.** The SQL
   front end refuses it; the wire path renders an empty axis column while pivoting. Clients
   MUST NOT send that combination.
5. **§11.5 System cubes are not rebuilt by HTTP writes.** They refresh on catalogue load
   and after CLI commands, so a long-lived server's `#Cubes`/`#Members`/`#Columns` can lag
   its own writes (e.g. a member created with `createMissing`); user-cube reads are always
   current.
6. **§11.6 SQL tooling: `GROUP BY`/`HAVING`/`DISTINCT` are parsed but not read** (Tier T3
   only; not a client data path). Clients MUST NOT depend on the SQL surface at all.
7. **§11.7 PRE-EXISTING: `COUNT` over a TEXT cell is not a cell count.** A text cell
   replaces the numeric accumulator ("last value wins"), so `COUNT(<text column>)` returns
   the last text. `COUNTD` is the supported exact aggregation for text. Do not offer
   `COUNT` on a text-only column until this is fixed.
8. **§11.8 PRE-EXISTING: grouped-no-measures row duplication.** On a cube with a measure
   dimension, a grouped query with no measures returns each group once per measure member
   (e.g. `[["East"],["East"],["West"],["West"]]`). A `domainFor` reader is unaffected; a
   dimension-only table query would show duplicates. Fetch picker values with `domainFor`,
   not rows.

**§11.9 RESOLVED** — the former silent drop of filters whose `field` did not match a
dimension was fixed in engine `ae5e2c9`; see the newest changelog entry. `describe` casing
is now enforced case-sensitively.

---

## How to use this file

1. **Read the newest entries at each engine pull.** Each entry names the engine commit,
   says whether the wire changed, and what a client must do. "No wire change" entries still
   tell you whether a check or a document moved.
2. **At runtime, feature-detect** via `GET /v1/capabilities`; never hardcode a feature list.
   A client that reads `aggregations`, `expressionFunctions`, `supportsJoin`,
   `countDistinctExact`, `supportsInlineDomain` and `supportsWrite` cannot silently offer
   something the engine lacks.
3. **Pin the contract, not this file.** A client declares the contract version and engine
   commit it verified against in `contract/STATUS.md`; `contract/PROTOCOL.md` is the text it
   conforms to. This file only explains what changed between engine commits.
4. **Contract changes go through the owner.** Do not patch a client's copy of the protocol
   and call it done: raise/adopt a handoff under `contract/handoffs/`; the contract owner
   updates `contract/PROTOCOL.md` and `contract/CHANGELOG.md` on evidence.
5. **Engine-side changes are appended here** in the same commit that makes them (with the
   engine SHA), newest first, so nothing is lost between sessions.

---

## Where each wire surface is specified

`contract/PROTOCOL.md` is normative; this table is only a map so an engine change can name
the clause it affects.

| Surface | Contract section |
| --- | --- |
| HTTP/JSON binding, routing, paths/encoding, `protocolVersion`, CORS, SQL tooling (T3) | §0 |
| `POST /v1/login`, bearer tokens, 401 semantics, public routes, token storage | §1 |
| `GET /v1/datasets` | §2 |
| `GET /v1/{dataset}` (describe) | §3 |
| `GET /v1/{dataset}/sample` | §4 |
| `POST /v1/{dataset}/query` (request, filters, expressions, `domainFor`, join, response, absent values, aggregation/COUNTD semantics) | §5 |
| `GET /v1/capabilities` (exact payload in `contract/capabilities.v1.json`) | §6 |
| `POST /v1/{dataset}/write` | §7 |
| `POST /v1/save` | §8 |
| Error `{error, code}` envelope and status mapping | §9 |
| Access control (`#TableSecurity`, `#DimensionSecurity`) and system cubes | §10 |
| Open breaks / known gaps | §11 |
| Conformance tiers T0–T4 | §12 |

Contract/versioning authority: `contract/README.md`. Adoption ledger: `contract/STATUS.md`.
Engine conformance checker: `contract/conformance/check-engine.ps1` (§-numbered clauses;
run it before any wire commit).

---

# Changelog (newest first)

### Uncommitted (wave 3, HEAD `ae5e2c9`) — this file is now the engine-side change feed

- **Docs only; no wire change.** `WIRE-CHANGES.md` no longer claims to be normative or
  paste-ready: the normative contract is `contract/PROTOCOL.md` v1 (owner-controlled), and
  this file is the per-commit engine feed. The former "contract additions, paste-ready"
  section is replaced by the map above; the stale-mirror status of
  `tableau-clone/docs/ENGINE-PROTOCOL.md` is stated in the header.
- **Client action:** nothing on the wire. A client that kept the old Plateau document as
  authority MUST pin contract v1 instead (see `contract/handoffs/`).

### `ae5e2c9` — fix(wire): refuse filters whose `field` is not a dimension

- **Wire change (client-visible):** a dimension-scope filter whose `field` does not name a
  dimension of the dataset is now a **loud error**, not a silently dropped predicate. It
  was the plausible-wrong-answer failure mode: `SliceQuery.filters` is keyed by dimension
  name, so a mismatch never applied and the client received an unfiltered answer.
- **Exact behaviour:** every dimension-scope filter's `field` is validated once, before the
  operator is dispatched, case-sensitively:
  - an exact dimension name (as `describe` spells it) proceeds;
  - a **hierarchy name** of a dimension the dataset uses → `400 unsupported_filter` naming
    the field, the hierarchy and its owner dimension (filters are keyed by dimension name);
  - anything else → `400 invalid_argument` naming the field and the dataset, with a message
    saying matching is case-sensitive;
  - `notEquals` shares the validation (its old duplicate check is gone);
  - measure-scope filters are untouched (still refused, §11.3).
- **No capability, response-shape or `protocolVersion` change.** Clients MUST send filter
  fields exactly as `describe` returns them.
- This closes the old **§11.9** observation (contract `PROTOCOL.md` §11.9 is marked
  RESOLVED; §5.4 gained the filter-field-validation MUST). Pinned by the engine's plan/wire
  tests for `in`/`equals`/`notEquals` unknown fields, the hierarchy-name case, and valid
  filtering; the conformance checker gained clause **§5.10**, now green at 49 passed /
  0 failed / 1 skipped.

### Wave 2 (`02c4347`) — exact `COUNTD` (`countDistinctExact: true`)

- **`capabilities.aggregations` is now `["SUM","AVG","COUNT","MIN","MAX","COUNTD"]`** (COUNTD
  appended; `protocolVersion` stays 1), and **`capabilities.countDistinctExact` is `true`.**
  The flag is honest: COUNTD is computed EXACTLY, so a client must stop labelling distinct
  counts approximate/estimated and must not suppress the aggregation.
- **Definition:** COUNTD = the number of DISTINCT BASE LEAF VALUES of the measure in the
  group, over its NON-EMPTY cells. Weights are ignored (same rule as AVG/MIN/MAX). A group
  with no cell for the measure stays the engine's `-` marker and goes on the wire as `null`,
  never `0`.
- **Equality:** numeric equality is exact by value — `-0.0` folds onto `0.0`, every NaN
  payload folds onto ONE value, `±inf` are distinct, nothing is rounded. Text (attribute
  cubes) is exact and case-sensitive, and never equal to a numeric cell.
- **Cost:** the per-group set is allocated only for COUNTD; SUM/AVG/COUNT/MIN/MAX keep their
  fixed O(1) accumulator.
- **SQL front door (engine tooling, not a client path):** `COUNTD(x)` and
  `COUNT(DISTINCT x)` both compute COUNTD and agree; `DISTINCT` on any OTHER aggregation
  (`SUM(DISTINCT …)`) is refused with a typed error, never silently computed without it.
- **Calculated fields:** COUNTD inside a formula is still `unsupported_expression` by name
  (`calc.rs` untouched) — only a measure pill can ask for it, not a calculated field.
- Pinned by `countd_counts_distinct_values_not_cells`, `countd_ignores_rollup_weights`,
  `countd_skips_absent_cells_within_a_partial_group`,
  `countd_numeric_equality_folds_zero_and_nan`,
  `countd_text_values_are_exact_and_case_sensitive`,
  `count_distinct_is_countd_not_plain_count`, `distinct_on_other_aggregations_is_refused_loudly`,
  `countd_is_computed_exactly_through_the_plan_path`,
  `countd_inside_a_calculated_field_is_refused_by_name`; capabilities pins updated:
  `capabilities_section4_keys_and_honest_declarations`,
  `capabilities_aggregations_match_what_actually_runs`,
  `query_computes_the_advertised_aggregations_and_refuses_the_rest`. New golden fixture
  `tests/scripts/countd.sql` (+ expected); `aggregation_functions.expected.txt` differs by one
  line because the MEDIAN refusal message lists the implemented aggregations (COUNTD is now
  in that list) — expected, not a weakening.

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
  silently dropped predicate. Measure-scope filters remain unsupported (Open break 3). No
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
  yet** — see Open break 4.
- **Also fixed in that pass:** a generated default member (the filter injected for a
  dimension a query leaves unnamed) is used only if it exists in the dimension's default
  hierarchy, so a multi-hierarchy dimension no longer produces `Member 'Furniture' not
  found` on an ordinary query.

### `e0dc743` (base), `f849a47`, `0160454`, `9740cf5` — writeback and persistence

- **Wire additions:** `POST /v1/{dataset}/write` (batched, atomic, client-chosen `splash`
  and `createMissing`, `200 {written,version}`, last-write-wins) and `POST /v1/save`
  (explicit persistence, `admin` when auth is on). `capabilities.supportsWrite` advertises
  both. Contract: `contract/PROTOCOL.md` §7 and §8. A client must feature-detect
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
- Contract: `contract/PROTOCOL.md` §10.2.

### `ced7534` — table access: hidden cubes look non-existent

- A role may be denied a cube (`#TableSecurity`). The wire makes that invisible rather than
  returning 403: `GET /v1/datasets` omits it, and `GET /v1/{cube}`, `/sample`, `/query` and
  `POST /v1/sql` (any statement naming the cube, including joins and subqueries) answer
  **404 `unknown_dataset`**, byte-for-byte the same shape as a cube that does not exist.
- `#TableSecurity(#Role, #Cube) -> READ|WRITE|NONE` is a new system cube, listed in
  `#Columns`. With no rule the default is ACCEPT; rules restrict, they do not grant.
- Clients that key on `unknown_dataset` to mean "deleted/typo" should treat it as "not
  available to you" — the engine deliberately does not distinguish.
  Contract: `contract/PROTOCOL.md` §10.1.

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
- Contract: `contract/PROTOCOL.md` §1.

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
  own writes (Open break 5) — the server DOES write now.
- Contract: `contract/PROTOCOL.md` §10.3.

### `e99f5c8` — calculated fields are EVALUATED

`Measure.expr` (an inline AST, or a name into `expressions`) used to be refused with
`unsupported_expression`. It now runs. **No protocol field changed** — the AST shape in
`plan::AstNode` already matched the client's `expression.ts` tag for tag, so the request a
client already sends works.

- Supported now: aggregates `SUM/AVG/COUNT/MIN/MAX` per field, arithmetic, comparisons,
  `AND/OR/NOT`, `IF`, `ZN`, `IFNULL`.
- Still refused: LOD (`supportsLod` stays `false`) and every function not in
  `expressionFunctions`.
- `capabilities.expressionFunctions` is `["ZN","IFNULL"]` — **advertise exactly what runs.**
  A client showing a formula box should offer only these plus the aggregates.
- **One aggregation per field**: `SUM([x]) / AVG([x])` is refused by name. If a formula
  editor would produce that, disable it rather than let the user hit the error.
- Semantics worth surfacing in the UI: division by zero is **absent**, not `0`; an absent
  input makes the expression absent (that is what `ZN`/`IFNULL` exist for); an absent `IF`
  condition yields absent rather than the `ELSE` branch.
- Contract: `contract/PROTOCOL.md` §5.5.

### `d5d282d` — SQL `LIMIT`/`OFFSET`

Engine-internal: the SQL front end only. **No client change.** (Clients do not send SQL.)

### `d2fcf41` / `90273b0` — a read-only SQL console at `/sql`

Engine-internal tooling: `GET /sql` (page) and `POST /v1/sql` (`{"sql":"SELECT …"}`,
SELECT only, access-checked like any data route). It is **not part of the client contract**
and no client needs to implement anything. It exists to debug the engine by hand. A SQL box
belongs in the client that owns a formula language, not in the engine. Contract: §0.5 (T3).

### `6569921` / `e7c49ae` — four more aggregations, and absence becomes `null`

- `capabilities.aggregations` became `["SUM","AVG","COUNT","MIN","MAX"]` at that commit.
  Anything absent from the CURRENT list is refused with `unsupported_aggregation` — **do not
  offer it**. (Superseded for COUNTD in wave 2: the current list appends `COUNTD`.)
- **`capabilities.countDistinctExact` flipped `true` → `false`** at that commit: it had been
  claiming an exact distinct count for a `COUNTD` that did not exist yet, so `false` was the
  honest answer and a client was told to treat distinct counts as unavailable. **(Superseded
  in wave 2 (`02c4347`): COUNTD exists and is exact, and the flag is `true` again — see
  the newest entry.)**
- **A measure with no cells in a group now arrives as `null`, not the string `"-"`.**
  This is a conformance fix (an absent value is not a dash), but it is the one change that
  can break a client silently: anything doing `String(cell)` or `cell.length` must handle
  `null`. Recommended rendering: an em dash or empty cell.
- Still true from before: strings for dimensions and attributes, numbers for measures.
- Contract: `contract/PROTOCOL.md` §5.10–§5.11.

### `1813c65` — join discovery fields

`capabilities.supportsJoin`, `describe.measureDimension`, `describe.isAttributeTable`, and
the `join` block (`{cube, using?, attributes[]}`; attributes append as output columns, a
missing attribute value is JSON `null`). Engine-side join notes: `JOIN-HANDOVER.md`.
Contract: `contract/PROTOCOL.md` §5.8.

---

## What clients should do, in order

1. **Read `capabilities` before enabling anything.** If that sounds like boilerplate, note
   that an old engine ignores a `join` block with no error at all — the user gets un-joined
   data and believes it; and an old engine ignores `supportsWrite` the same way.
2. **Handle `null` measure cells.** This is the only change in this feed that can corrupt a
   display without an error.
3. **Offer the aggregations found in `aggregations`**, not a fixed list. `COUNTD` is
   implemented and EXACT on this engine (`countDistinctExact: true`) — do not label it
   approximate and do not suppress it. A calculated-field formula still cannot use COUNTD
   (`unsupported_expression`); only a measure pill can request it. (SQL tooling agrees —
   `COUNTD(x)` and `COUNT(DISTINCT x)` are the same computation — but SQL is not a client
   path.)
4. **Calculated fields are the engine's job** if the AST is sent: the client already parses
   its formula language, so no request-side work is needed — but stop computing expressions
   client-side for anything the engine accepts, or the two will disagree.
5. **Do not offer date grouping on an engine dataset** until Open break 1 closes.
6. **`domainFor` and `notEquals` are honoured** (wave 1) — no workaround needed. Filters
   are DIMENSION-scope: do not offer a measure-scope filter until Open break 3 closes.
7. **Send filter fields exactly as `describe` spells them** (case-sensitive). Since
   `ae5e2c9` a mismatch is a loud `400 invalid_argument`/`400 unsupported_filter`, never a
   silently unfiltered answer.
8. **Pin contract v1** and declare the verified engine commit + tier in
   `contract/STATUS.md`; the old client-repo protocol copy is stale and non-normative.

## Keeping this current

Every engine change that alters a request/response field, a capability, a status code, or
the meaning of a value gets an entry here **in the same commit that makes it**, with the
engine SHA — one file, append-only, newest first. This feed is engine-side and
**non-normative**; the contract owner updates `contract/PROTOCOL.md` and
`contract/CHANGELOG.md` in the same change (or records why not), and a client's conformance
claim cites the contract, not this file. When a change is still in the working tree, name
the HEAD it is built on and say "uncommitted"; replace that with the real SHA once it
lands. The old statement that a client-repo document wins no longer applies: if this feed
and `contract/PROTOCOL.md` disagree, the contract is right and the disagreement is a bug to
raise with the contract owner.
