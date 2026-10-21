# Handover: attribute joins, engine → Plateau

**Status:** implemented in the engine and pushed. Nothing is needed from the engine side to
start; this document is the whole contract.
**Engine commit:** `115d063` (or later). If `GET /v1/capabilities` does not contain
`"supportsJoin": true`, the engine on your port is an old binary — see §1.
**Owner of this document:** the OLAP engine session. The Plateau tree is yours; the
normative protocol text lives in `tableau-clone/docs/ENGINE-PROTOCOL.md` (§8 below is the
section to paste there).

---

## 1. Run the engine

```powershell
cd C:\Users\Startklar\Documents\deepseek-harness\default-workspace
.\start-engine.cmd            # port 8080, serves database.dev.bin
```

`start-engine.cmd` is the reliable route on this machine: PowerShell's execution policy
refuses unsigned `.ps1` files, so `.\start-engine.ps1` is blocked outright. The `.cmd`
wrapper starts PowerShell with `-ExecutionPolicy Bypass` for that one script — the
machine-wide policy is unchanged. Arguments pass through: `-Port 8090`, `-Fresh`, `-Cors`.

Leave the window open; that window **is** the server, and Ctrl+C stops it. The script copies
`database.bin` → `database.dev.bin` and serves the copy, so the seeded reference is never
mutated. Running `serve.exe` directly skips that copy and loads `database.bin` itself — fine
for read-only probing, but use the script for anything else.

Check what you are talking to:

```powershell
Invoke-RestMethod http://127.0.0.1:8080/v1/capabilities | Select-Object supportsJoin
```

**Browsing `http://127.0.0.1:8080/` returns
`{"error":"Unknown path '/'.","code":"invalid_argument"}` — that is correct, not a fault.**
Every endpoint lives under `/v1/`; there is no index page. Use `/v1/capabilities` or
`/v1/datasets`.

The seeded catalogue has `Sales` (Region/Segment/Category, implicit `value`) and `RegionAttr`
(Region, Attribute) — enough to try a join immediately.

---

## 2. Discovery — how to find a joinable cube and its attributes

Three additive v1 fields exist for exactly this. They are safe to ignore on an old engine.

| Where | Field | Meaning |
|---|---|---|
| `capabilities` | `supportsJoin: true` | an OLDER engine ignores a `join` block **silently**, so check this before offering the feature. Otherwise the user gets un-joined data with no error. |
| `describe` | `measureDimension: "Attribute" \| null` | names the column whose **members** are the attachable attribute names |
| `describe` | `isAttributeTable: true` | this cube is an attribute source, not a measure cube |

`measureDimension` matters because attribute values are text: in `columns[]` the
attribute-name column looks like an ordinary string dimension. Nothing else distinguishes it.

The flow:

1. `GET /v1/datasets` → candidate names.
2. `GET /v1/{ds}` for each → `columns[]`, `measureDimension`, `isAttributeTable`.
3. A join key must be a dimension present in **both** cubes — intersect `columns[].name`.
   That is a **conformed** dimension.
4. The attachable attributes are the member names of the joined cube's `measureDimension`
   column: `POST /v1/{attr}/query {"domainFor": ["Attribute"]}` → `domain`.
5. `POST /v1/{primary}/query` with a `join` block (§3).

Real `GET /v1/RegionAttr` on the seeded catalogue:

```json
{"columns":[{"cardinality":4,"dataType":"string","logicalType":"string","max":null,"min":null,"name":"Region","notNull":true,"nullable":false,"suggestedKind":"dimension"},{"cardinality":1,"dataType":"string","logicalType":"string","max":null,"min":null,"name":"Attribute","notNull":true,"nullable":false,"suggestedKind":"dimension"}],"dataset":"RegionAttr","isAttributeTable":true,"measureDimension":"Attribute","primaryKey":null,"rowCount":4}
```

and the attribute list:

```powershell
$body = '{"dataset":"RegionAttr","protocolVersion":1,"domainFor":["Attribute"]}'
Invoke-RestMethod -Uri http://127.0.0.1:8080/v1/RegionAttr/query -Method Post `
  -ContentType 'application/json' -Body $body
# -> domain = {"Attribute":["Manager"]}
```

---

## 3. The request

Add `join` to a normal `POST /v1/{dataset}/query` body:

```json
{
  "dataset": "Sales",
  "protocolVersion": 1,
  "groupBy": [{ "name": "Region" }],
  "join": { "cube": "RegionAttr", "using": "Region", "attributes": ["Manager"] }
}
```

| Field | Required | Meaning |
|---|---|---|
| `cube` | yes | the attribute cube to attach from |
| `using` | no | the conformed dimension to key on. Omit to infer it — inference succeeds only when exactly one dimension is shared |
| `attributes` | yes (may be empty) | member names of the joined cube's measure dimension, **in the order you want the columns** |

---

## 4. The response

Attribute columns are **appended after** the queried columns, in the requested order, and
appear in `columnMeta` as `kind: "dimension"`, `dataType: "string"`. They are **not** in
`measureNames` or `measureLabels`.

Real response for the request above:

```json
{"columnMeta":[{"dataType":"string","defaultAggregation":"COUNT","kind":"dimension","logicalType":"string","name":"Region","semanticType":"nominal"},{"dataType":"number","defaultAggregation":"SUM","kind":"measure","logicalType":"real","name":"value","semanticType":"quantitative"},{"dataType":"string","defaultAggregation":"COUNT","kind":"dimension","logicalType":"string","name":"Manager","semanticType":"nominal"}],"columns":["Region","value","Manager"],"dimensionNames":["Region"],"domain":{},"elapsedMs":1,"measureLabels":["SUM(value)"],"measureNames":["value"],"rows":[["Central",6436941.57,"Ava Thompson"],["East",6319711.15,"Ben Carter"],["South",6441942.55,"Cara Diaz"],["West",7564832.16,"Dan Ellis"]],"truncated":false}
```

Note what is *not* there: `Manager` appears in `columns` and `columnMeta` (as a dimension), but
not in `measureNames`/`measureLabels`, which still contain only `value`.

**A key with no attribute row yields `null`** — not `"-"`, not `""`:

```json
"rows": [["East", 150.0, null]]
```

`orderBy` may reference an attribute column; enrichment happens before ordering and limiting.

---

## 5. Errors

All join failures are `invalid_argument` (HTTP 400) except a missing cube, which is
`unknown_dataset` (HTTP 404). Verbatim messages, so you can match on `code` and show the rest:

| Cause | `code` | Verbatim `error` (measured, not invented) |
|---|---|---|
| joined cube does not exist | `unknown_dataset` (404) | `Dataset 'Nope' not found.` |
| `using` not present in both cubes | `invalid_argument` (400) | `Invalid argument: Cannot JOIN 'Sales' with 'RegionAttr' USING 'Segment': it must be a conformed dimension (present in both cubes).` |
| no shared dimension and no `using` | `invalid_argument` (400) | `Cannot JOIN 'A' with 'B': they share no conformed dimension. A JOIN keys off a dimension with the same name in both cubes; write JOIN ... USING (<dimension>).` |
| several shared dimensions, no `using` | `invalid_argument` (400) | `Cannot infer how to JOIN 'A' with 'B': they share 2 conformed dimensions (Region, Product). Specify one with USING (<dimension>).` |
| key not in `groupBy` | `invalid_argument` (400) | `Invalid argument: Cannot JOIN 'Sales' with 'RegionAttr': the conformed dimension 'Region' is not projected, so there is no key to join the attributes onto.` |
| `attributes` names a non-attribute | `invalid_argument` (400) | `Invalid argument: Cannot JOIN 'Sales' with 'RegionAttr': 'Nope' is not an attribute of 'RegionAttr'.` |
| attribute name collides with an output column | `invalid_argument` (400) | `... collides with an output column of the same name` |

The envelope is the standard one: `{"error": "...", "code": "..."}`. Match on `code`; the
message text is user-facing guidance and may be improved, so do not parse it.

---

## 6. Rules the UI must respect

1. **The conformed key must be in `groupBy`.** There is no hidden-field mechanism, so an
   attribute whose key isn't a visible grouping column is refused ("not projected"). If the
   design wants "attach manager without showing region", that needs an engine feature, not a
   client trick.
2. **Attributes are appended, not interleaved.** Reorder on your side if the sheet wants them
   elsewhere.
3. **One join per request.** `join` is a single object, so this is structural — you cannot
   even express two.
4. **LEFT join only.** Every primary row is kept; a key with no attribute row is `null`.
   There is no INNER/RIGHT/FULL over the wire.
5. **Attribute values are strings and are not measures.** They cannot be aggregated, and they
   never appear in `measureNames`.
6. **The joined cube must be an attribute table** (`isAttributeTable: true`) with a measure
   dimension whose members are the attributes.

---

## 7. Not supported, by design or by omission

- `RIGHT`, `FULL`, `CROSS` joins, and joins on an arbitrary predicate. A join keys off a
  conformed dimension because cubes are not relational tables.
- More than one join per query.
- Joining on a *measure* or on a hierarchy alias; the key is a dimension name.
- A write/enqueue surface: the engine has no mutation endpoint over HTTP.

---

## 8. Protocol text for ENGINE-PROTOCOL.md

> ### `join` (optional, `POST /v1/{dataset}/query`)
>
> Attaches attributes from an attribute cube through a **conformed dimension** — one with the
> same name in both cubes. There is no arbitrary predicate.
>
> ```json
> "join": { "cube": "RegionAttr", "using": "Region", "attributes": ["Manager"] }
> ```
>
> `cube` — the attribute cube. `using` — the conformed dimension; omitted means infer, which
> fails as `invalid_argument` when more than one dimension conforms. `attributes` — members of
> the joined cube's measure dimension, in output order.
>
> Attribute columns are **appended** after the queried columns as `kind: "dimension"`,
> `dataType: "string"`. A key with no attribute row yields `null`. `orderBy` may reference an
> attribute column.
>
> Left join only: every primary row is kept. One join per request. The key must appear in
> `groupBy`. Errors: `invalid_argument` (non-conformed or ambiguous key, key not projected,
> unknown attribute, name collision) or `unknown_dataset` (no such joined cube).
>
> **Additive v1 fields for discovery:** `capabilities.supportsJoin` (boolean),
> `describe.measureDimension` (string|null), `describe.isAttributeTable` (boolean). A v1
> client that ignores them still works; a client that wants to offer joins must read
> `supportsJoin` first, because an older engine ignores a `join` block silently.

---

## 9. Where the engine side lives

| File | Role |
|---|---|
| `src/engine/attribute_join.rs` | one implementation, shared by SQL and the wire: `resolve` validates, `apply` stitches |
| `src/engine/plan.rs` | `JoinRequest` on the wire, `apply_join` before order/limit |
| `src/engine/wire.rs` | `supportsJoin`, `measureDimension`, `isAttributeTable`, `null` mapping |
| `tests/wire_tests.rs` | 6 join tests: happy path, inference, nulls, ordering, and the five error cases |
| `PLATEAU-WIRE-FINDINGS.md` | the same contract recorded engine-side |

Run them:

```powershell
cd Patricia\olap_engine
cargo test --offline
```

---

## 10. Ten-minute verification checklist

```powershell
$base = 'http://127.0.0.1:8080'
(Invoke-RestMethod "$base/v1/capabilities").supportsJoin              # True
(Invoke-RestMethod "$base/v1/RegionAttr") | Select-Object isAttributeTable, measureDimension
# isAttributeTable=True, measureDimension=Attribute

$q = '{"dataset":"Sales","protocolVersion":1,"groupBy":[{"name":"Region"}],"join":{"cube":"RegionAttr","using":"Region","attributes":["Manager"]}}'
Invoke-RestMethod -Uri "$base/v1/Sales/query" -Method Post -ContentType 'application/json' -Body $q |
  Select-Object -ExpandProperty rows
# 4 rows, each Region/value/Manager

# and the failure you should surface nicely:
$bad = '{"dataset":"Sales","protocolVersion":1,"groupBy":[{"name":"Region"}],"join":{"cube":"RegionAttr","using":"Segment","attributes":["Manager"]}}'
try { Invoke-RestMethod -Uri "$base/v1/Sales/query" -Method Post -ContentType 'application/json' -Body $bad }
catch { $_.ErrorDetails.Message }   # code = invalid_argument, mentions "conformed dimension"
```

If any of that disagrees with this document, the engine is at a different commit — say so
rather than working around it.
