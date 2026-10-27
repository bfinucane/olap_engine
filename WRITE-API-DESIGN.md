# Write API — the contract MiniSheet writes back through

**Status: IMPLEMENTED** (engine commit `e0dc743`). This document describes what the engine
does today; it is not a proposal. Where a decision went against the original proposal, why is
recorded in §12.

**Audience:** the MiniSheet session. Everything here is verified by tests in
`tests/serve_http_tests.rs` and `src/engine/write.rs`.

---

## 1. The endpoints

| Endpoint | Purpose |
| --- | --- |
| `POST /v1/{dataset}/write` | batched cell writes, atomic, client-controlled splash/autocreate |
| `POST /v1/save` | persist the in-memory catalogue to disk (admin) |
| `GET /v1/capabilities` | `supportsWrite: true` — feature-detect writeback |

Every endpoint except `GET /v1/capabilities` and `POST /v1/login` needs
`Authorization: Bearer <token>` (see `PLATEAU-AUTH-HANDOVER.md` for the login flow).

## 2. The write request

```json
POST /v1/{dataset}/write
{
  "cells": [
    { "coords": { "Region": "East", "Product": "Widget", "Measure": "Sales" },
      "value": 123.45 },
    { "coords": { "Region": "East", "Product": "Widget", "Measure": "Comment" },
      "value": "text for an attribute cube" }
  ],
  "splash": "reject",
  "createMissing": false
}
```

```json
200 { "written": 2, "version": 43 }
```

- **`coords` is a `{dimension: member}` map**, not a positional tuple. Dimension keys are
  matched case-insensitively against the cube's dimensions; a missing, extra or empty
  coordinate is `invalid_argument` naming the expected dimensions.
- **`value`** is a JSON number, or a string — but a STRING only into an ATTRIBUTE table
  (`CREATE ATTRIBUTE TABLE`, `isAttributeTable: true`). A string in an aggregating cube is
  `invalid_argument`: the accumulator treats a text cell as "last value wins", so a comment
  written into a numeric measure would silently replace that group's total depending on scan
  order. `null` and booleans are refused. A numeric value in an attribute table IS allowed
  (some attributes are numeric), because attribute cubes never aggregate.
- **`splash`** — see §3. Absent means `reject`.
- **`createMissing`** — see §4. Absent means `false`.
- **`version`** in the response is the cube's data version after the write. It is
  INFORMATIONAL (§5), not a token any later request must present.

**The batch is atomic**: every cell is validated (arity, member existence, splash legality,
value type) before any cell is written. A rejected batch changes nothing.

**Limit:** 10,000 cells per request (`write::MAX_CELLS_PER_REQUEST`), so one request cannot
hold the store lock for an unbounded time. Larger edits must be chunked by the client — a
failed chunk leaves the earlier chunks applied, so chunk on page/block boundaries.

## 3. Writing a consolidated cell = splashing (client's choice)

A consolidated coordinate is not a store location; it is a rollup. Writing it distributes the
value over its leaves by rollup weights. The ENGINE NEVER PICKS THE MODE:

| `splash` | Behaviour |
| --- | --- |
| `"reject"` (default) | the write fails with `consolidated_write` and names the coordinate |
| `"replace"` | distribute so the rollup's total becomes `value` |
| `"add"` | distribute `value` as a delta on top of the current leaves |

- Splashing is arithmetic, so it needs a NUMERIC value; a string on a consolidated coordinate
  is `invalid_argument`.
- `splash` is harmless on a leaf coordinate (behaves as a plain write), so a client can send
  one policy for a whole range without inspecting the hierarchy first.
- `replace` rewrites every leaf under the member, including leaves the user never saw; `add`
  preserves them but "set the cell to 5" means "add 5". Those are the client's semantics to
  choose, which is why the engine does not choose.
- Uneven weights leave a rounding residual. Today the engine allocates per leaf at full f64
  precision and does not renormalise; if MiniSheet needs penny-rounding, that must be decided
  client-side (round the values it sends) or added as an explicit engine feature. It is NOT
  silently handled.

## 4. Creating members is the client's choice, vetoed by the server

- `createMissing: false` (default): a coordinate naming an absent member is refused with
  `unknown_member`, and the message suggests the flag. A misspelled member cannot become part
  of the model because of a cell edit.
- `createMissing: true`: the engine creates missing members while writing (this is the
  behaviour TM1/Infor lack; the user called that an oversight).
- THE SERVER CAN VETO IT. Creating a member requires the dimension's WHOLE-DIMENSION level
  (`#DimensionSecurity`, `#ALL` row) to be `WRITE`; a per-member rule cannot apply to a member
  that does not exist yet. A denied dimension is refused with `forbidden`. Under the
  default-accept model, a role with no rules may create.

## 5. Concurrency: last write wins

There is no `ifVersion`, no locking and no 409. If two clients write the same cell, the later
request wins. The `version` in the response exists for clients that want to detect staleness
themselves (e.g. to refresh a view); the engine does not enforce anything with it.

## 6. Permissions (security S4, built with this endpoint)

A write is checked against the same security cubes as reads, and it FAILS LOUDLY:

- the role needs table level `WRITE` on the dataset (`#TableSecurity`); `READ` alone is
  refused;
- a coordinate whose dimension member the role cannot see cannot be written (`READ`/`NONE`
  member or `#ALL` rule refuses), because writing into a hidden cell changes data the caller
  cannot verify;
- a member-level `READ` rule caps writes to `forbidden` (read allowed, write not);
- **a denied dataset answers 403 `forbidden`**, deliberately NOT the 404 used for reads. For a
  read, 404 hides structure; for a write, a fake success or a "missing" answer would be worse
  than admitting the denial.

## 7. Persistence: one explicit save

Writes live in the engine's in-memory catalogue. Nothing is written to disk until:

```text
POST /v1/save      (requires the 'admin' role when authentication is on)
200 {"saved": true, "path": "..."}
```

- No debounce, no automatic save, no save on shutdown: nothing is persisted by surprise, and
  the client decides when a unit of work becomes durable.
- Saving rewrites the whole catalogue file. It is not an incremental journal; a crash after
  the response is safe, a crash before it loses the writes since the last save.
- `save_failed` (500) means the file could not be written; the in-memory data is unchanged.

## 8. What cannot be written through this endpoint

- **Derived system cubes** (`#Cubes`, `#Dimensions`, `#Hierarchies`, `#Members`, `#Edges`,
  `#Columns`): the engine rebuilds them, so a write would be lost — `invalid_argument`.
- **Security control cubes** (`#TableSecurity`, `#DimensionSecurity`): they are engine
  CONFIGURATION, changed through an admin SQL session (the CLI), not through the data API —
  `forbidden`. Without this, any role with default-accept write access could grant itself
  permissions. (Found while reviewing this document against the code; fixed and tested.)
- **Text into an aggregating cube**: refused (§2). Text has a designated home, the attribute
  table, and that is a modelled decision rather than a workaround for the accumulator. The
  CLI enforces the same rule for BOTH bulk importers: `.import` refuses a non-numeric value
  in an aggregating cube (naming the row), and `.import_attr` refuses the cube outright,
  because it is the string-valued importer and would otherwise write text unconditionally.
- Anything requiring a **hierarchy-qualified coordinate** (`Hierarchy:Member`): coordinates
  resolve in the dimension's DEFAULT hierarchy, like SQL `INSERT`. A consolidated member that
  exists only in another hierarchy cannot be addressed yet. Recorded limitation, not silent
  behaviour.

## 9. Error contract

| Code | HTTP | Meaning |
| --- | --- | --- |
| `invalid_argument` | 400 | malformed body, arity mismatch, wrong value type, unknown `splash`, derived cube |
| `unknown_member` | 400 | coordinate names an absent member and `createMissing` is false |
| `consolidated_write` | 400 | consolidated coordinate with `splash: "reject"` |
| `forbidden` | 403 | no table `WRITE`, denied member, no autocreate right, control cube, save without admin |
| `unknown_dataset` | 404 | no such cube (or the role cannot read it at all) |
| `save_failed` | 500 | the catalogue file could not be written |

## 10. What the engine deliberately does NOT do

- **Recalculation** — consolidations are computed on read; nothing to recalc after a write.
- **Undo / transactions across requests** — a batch is the transaction; wider units are client
  composition.
- **Locks** — last write wins (§5).
- **Validation of spreadsheet formulas** — values arrive as numbers or strings.

## 11. What MiniSheet must do

1. Feature-detect on `capabilities.supportsWrite` before offering an editable grid.
2. Build `coords` from column headers, using the cube's dimension names (a hyperblock knows
   them from `describe`).
3. Choose and send `splash` per edit: `reject` for "edit a leaf", `replace`/`add` for an edit
   on a rollup row.
4. Decide `createMissing`; keep it `false` unless the sheet is deliberately modelling.
5. Work in memory and `POST /v1/save` at a natural commit point (e.g. an explicit Save action).
6. On `403`, tell the user they may not write; on `404`, the dataset is unavailable to them.
7. On `409`… there is none. Two users editing one cell: last save wins; refresh to see it.

## 12. Design history — what was proposed and rejected

Recorded so the reasoning survives:

- **`ifVersion` / 409 optimistic locking** — proposed, rejected ("last write wins"). `version`
  remains in the response, informational only.
- **Debounced save + save on shutdown** — proposed, rejected in favour of one explicit save.
- **Echoing written cells in the response** — proposed, dropped: the client already knows what
  it sent, and a read-back is `POST query`.
- **Server-chosen splash** — proposed as "reject by default, client opts in"; the user made it
  fully client-controlled: writing a consolidated cell IS splashing and the server only
  executes.
- **Autocreate as an engine-wide switch** — rejected; it is per request, with the security
  cubes as the server-side veto.

## 13. Tests that pin this contract

- `src/engine/write.rs` (17 unit tests): leaf and string writes, case-insensitive dimension
  names, arity errors, strict vs `createMissing`, consolidated refusal, `replace` setting the
  rollup total, splash on a leaf, unknown splash mode, batch atomicity, derived-cube refusal,
  control-cube refusal, unknown dataset.
- `tests/serve_http_tests.rs` (6 HTTP tests): a write lands and is readable; nothing is
  persisted before an explicit save and everything after it; a `READ`-only role gets 403; a
  client-chosen `createMissing` creates; a consolidated write needs a splash mode; a join
  cannot reach a denied cube; save requires admin.
