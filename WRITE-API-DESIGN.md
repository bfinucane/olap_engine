# Write API — contract proposal for MiniSheet writeback

**Status:** proposal, no code. Engine side of writeback.
**Question this answers:** what must the engine provide before a spreadsheet can write back,
and what decisions does that force?

---

## 1. What already exists (verified in `src/`)

The DATA MODEL is ready. Only the transport is missing.

| Capability | Where | Status |
| --- | --- | --- |
| Upsert one leaf cell, numeric or string | `Cube::write` | ✅ |
| Write a consolidated cell, spread over leaves by rollup weights | `Cube::write_splashed` (`Replace`, `Add`) | ✅ |
| Auto-create members on write | `Dimension::add_leaf` via `write` | ✅ |
| Reject writes to derived system cubes | `Cube::write` guard | ✅ |
| Bulk import from CSV | `Cube::import_csv` | ✅ |
| SQL `INSERT INTO ... VALUES` | `engine::sql` | ✅ internally, refused over HTTP |
| **Any HTTP write** | — | ❌ `ensure_read_only` allows one SELECT |
| **Persistence of an HTTP write** | — | ❌ the server never calls `save_to_disk` |
| **WRITE permission enforcement** | — | ⚠️ `#TableSecurity`/`#DimensionSecurity` parse `WRITE`; nothing enforces it yet |

So: the engine can already write, but not over the wire, and a write would be lost on restart.

## 2. Proposed request shape

One endpoint, batched, because a spreadsheet commits ranges, not single cells:

```json
POST /v1/{dataset}/write
{
  "cells": [
    { "coords": { "Region": "East", "Product": "Widget", "Measure": "Sales" }, "value": 123.45 },
    { "coords": { "Region": "East", "Product": "Gadget", "Measure": "Sales" }, "value": 0 },
    { "coords": { "Region": "West", "Product": "Widget", "Measure": "Comment" }, "value": "text" }
  ],
  "splash": "reject" | "replace" | "add",     // see §3; default "reject"
  "createMissing": false,                     // see §4; default false
  "ifVersion": 42                             // optional optimistic lock, see §5
}
```

Response:

```json
200 {
  "written": 3,
  "version": 43,                              // new data version, for the next write
  "cells": [                                  // read-back of what landed, in request order
    { "coords": { ... }, "value": 123.45 }
  ]
}
```

**Coordinates are a `{dimension: member}` map, not a positional tuple.** A tuple's order is
the client's guess about the cube's dimension order; a map is order-independent, matches
`describe`, and survives a dimension being added. `OLAPVALUE`'s tuple is display-ordered and
must be mapped by the client — that is a client-side conversion, not an engine one.

Errors (loud, because a silently dropped write is data loss):

| Code | HTTP | When |
| --- | --- | --- |
| `unknown_dataset` | 404 | cube missing, or the role may not READ it |
| `forbidden` | **403** | the role may read but not WRITE (see §6) |
| `unknown_member` | 400 | a coordinate names a member that does not exist and `createMissing` is false |
| `invalid_argument` | 400 | arity mismatch, wrong value type for an attribute table, bad `splash` |
| `consolidated_write` | 400 | a coordinate is consolidated and `splash` is `"reject"` |
| `conflict` | 409 | `ifVersion` does not match the current version |

## 3. Writing a consolidated cell: the decision that changes numbers

The engine can distribute a value across a consolidation by weights, and it already does that
for SQL `INSERT` (it chooses splash automatically). For a spreadsheet this must NOT be
implicit:

- `"splash": "reject"` (default) — a write to a rollup cell fails. Predictable: the caller
  learns that the cell is not a leaf.
- `"splash": "replace"` — distribute so the rollup's total becomes `value`.
- `"splash": "add"` — distribute `value` as a delta on top of the current leaves.

**Open question 1:** MiniSheet's writeback is "user edits the value shown in a report cell".
Which default does it want? `replace` matches spreadsheet intuition but rewrites every leaf
under the member, including cells the user never saw; `add` preserves them but makes "set the
cell to 5" mean "add 5". A weighted split also has a rounding residual when the weights do
not divide evenly — decide whether the residual goes to the largest leaf, the first leaf, or
is refused. **Recommendation: `reject` by default, `replace` opt-in per request, and the
client decides policy.** The engine should not silently choose.

## 4. Auto-creating members

`Cube::write` creates a missing member today. On the wire that turns a typo into a new member
in a dimension — visible in `#Members`, and hard to remove. 

**Open question 2:** strict (`createMissing: false`, default) or permissive? Recommendation:
strict. MiniSheet authors write into existing cells of a report; a new member should be a
deliberate structural action, not a side effect of a write.

## 5. Concurrency

Two users editing one report will collide. The engine already versions data (Phase 1b MVCC):
every write bumps a version, and a read can report it.

- Without `ifVersion`: last write wins.
- With `ifVersion`: the write is applied only if no write happened since that version;
  otherwise `409 conflict`, and the client refetches.

**Open question 3:** does MiniSheet want optimistic locking in v1? Recommendation: ship the
version token in the write response from day one (cheap), enforce `ifVersion` when supplied,
and let the client choose. Report views are read-mostly but multi-user editing is the whole
point of writeback.

## 6. Permissions (this is security S4, folded in)

A write endpoint makes the paused S4 load-bearing. Before any write lands:

- the role must have `WRITE` on the cube (`#TableSecurity`), not just `READ`;
- if the coordinate names a dimension member the role may not see (`#DimensionSecurity`
  `NONE`), the write must be refused — writing into a hidden cell both leaks its existence
  (the write would fail or not depending on it) and lets one user change numbers another
  cannot see;
- a denied cube or dimension answers **403 `forbidden`** for writes, deliberately NOT the
  404 used for reads: for a write, a silent fake success is worse than admitting the denial.

This is why S4 was paused rather than dropped: it cannot be built before the endpoint.

## 7. Persistence

The server holds the catalogue in memory and never saves. A write-API server must.

**Open question 4:** which policy?

| Policy | Behaviour | Trade-off |
| --- | --- | --- |
| Save after every batch | durable immediately | a save per edit; the file is rewritten wholesale |
| Debounced save (e.g. 1 s) + save on shutdown | near-durable | loses up to the window on a hard kill |
| Explicit `POST /v1/save` from the client | client controls | a forgotten save loses data |

Recommendation: **debounced save plus an explicit save endpoint**, and document the window.
A WAL is the real answer for durability but is a much larger change; it should be a promise
only when it exists.

## 8. What the engine does NOT need to add

- **Recalculation** — the engine computes consolidations on read, so there is nothing to
  recompute after a write.
- **Undo** — client state.
- **Locks** — optimistic versions instead.
- **Transactions spanning requests** — a batch is atomic; anything wider is a client
  composition.

## 9. Suggested wire surface summary

| Endpoint | Purpose |
| --- | --- |
| `POST /v1/{dataset}/write` | batched cell writes, atomic, returns new version |
| `POST /v1/save` | flush to disk (admin) — or rely on debounce + shutdown |

Everything else MiniSheet needs (lists, OLAPVALUE, describe) already exists.

## 10. Decisions needed before implementation

1. Splash default and residual policy (§3).
2. Strict vs auto-creating members (§4).
3. Optimistic locking in the client contract (§5).
4. Persistence policy (§7).
5. Confirmation that writeback targets **leaf** cells of measure cubes, and attribute
   (string) cubes for text — not consolidated cells (if so, §3 simplifies to `reject`).

Answer those and the endpoint is a contained slice: one route, one access check, a batch
validator, a version token, a save policy — roughly the size of S2.

---

# IMPLEMENTED — decisions as the user gave them

The five open questions above are answered and the endpoint is built. What changed:

1. **Splash is the client's choice.** `"splash": "reject" | "replace" | "add"`; absent =
   `reject`. The server performs the client's choice and never picks one. Verified: a
   `replace` on a consolidated cell sets the rollup total (100 + 100 over two leaves).
2. **Autocreate is the client's choice**, `createMissing` (default false), and the SERVER
   additionally vetoes it through dimension access (S3): creating a member requires the
   WHOLE-DIMENSION level to be `WRITE`, because a per-member rule cannot exist for a member
   that does not exist yet. This is the "oversight" the user noted in TM1/Infor being
   corrected rather than inherited.
3. **Last write wins.** There is no `ifVersion`, no 409, no locking. The response carries
   `version` for information only — it is not a precondition.
4. **Persistence is an explicit act.** `POST /v1/save` writes the catalogue to disk; there is
   no debounce and no save on shutdown, so nothing is persisted by surprise. Saving requires
   the `admin` role when authentication is on.
5. **Writing a consolidated cell IS splashing**, and the client controls the mode (§1).

## The wire as built

```text
POST /v1/{dataset}/write
{ "cells": [ { "coords": {"Region":"East","Product":"Widget","Measure":"Sales"},
               "value": 123.45 } ],
  "splash": "reject",          // optional: reject (default) | replace | add
  "createMissing": false }     // optional, default false

200 { "written": 1, "version": 43 }
400 invalid_argument | unknown_member | consolidated_write
403 forbidden
404 unknown_dataset
```

`POST /v1/save` -> `200 {"saved": true, "path": "..."}`; `403` for a non-admin.

`capabilities.supportsWrite` advertises the endpoint so a client can feature-detect instead
of discovering writeback by getting `invalid_argument` on an unknown route.

## Implementation notes an integrator needs

- **Atomic batches.** Every cell is validated (arity, member existence, splash legality,
  value type) before any cell is written, so a rejected batch changes nothing.
- **Max 10,000 cells per request** (`write::MAX_CELLS_PER_REQUEST`), so a hostile request
  cannot hold the store lock for an unbounded time.
- **Coordinates resolve in the DEFAULT hierarchy**, like SQL `INSERT`. A consolidated member
  that exists only in another hierarchy cannot be addressed yet (queries do support
  `Hierarchy:Member`); this is a recorded limitation, not a silent behaviour.
- **Rejected writes are HONEST**: 403 with `forbidden`, never the 404 used for hidden reads.
  A write that cannot happen must not look like it did.
- **A denied dimension member cannot be written**: writing into a cell the caller cannot see
  would change data they cannot verify.
- **Derived system cubes stay read-only**; the control cubes (`#TableSecurity`,
  `#DimensionSecurity`) are NOT writable through this endpoint either — they are engine
  configuration, written through SQL by an admin.
- **No cache invalidation needed**: the write bumps the cube's version, and the version-tagged
  cache is invalidated lazily on the next read.

## Tests

14 unit tests for the writer (leaf/string writes, case-insensitive dimension names, arity,
strict vs autocreate, consolidated refusal, splash replace, splash on a leaf, unknown splash
mode, batch atomicity, derived-cube refusal, unknown dataset) and 6 HTTP tests (write lands
and persists only after save, READ-only role refused, autocreate is the client's choice,
consolidated write needs splash and sets the total, a join cannot reach a denied cube, save
requires admin).