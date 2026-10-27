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
