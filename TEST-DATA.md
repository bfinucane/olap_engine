# Test data and database version — the pinned definition

This file defines **what "the test database" means** and how to prove you are looking at it.
Machine-readable source: [`model-manifest.v2.json`](model-manifest.v2.json) (v1 is kept as
history: `model-manifest.v1.json`). The database file is git-ignored, so the manifest, the
seed script and the content fingerprint are the definition; the frozen release is the exact
artefact.

## 1. Identity of a deployment

Three independent axes; record all three.

| Axis | Current value |
| --- | --- |
| **Engine** | package `0.1.0`, engine commit recorded in the manifest, `protocolVersion 1`, storage `FORMAT_VERSION 1` |
| **Contract** | `contract/PROTOCOL.md` v1 |
| **Model** | **`Superstore line items v2.2`** — file SHA-256 + content fingerprint from the manifest |

Frozen artefact (2026-10-30): 5,177,115 B,
SHA-256 `F2F9A6A33FD89F62317062861268543A99DBD17CBB0651EB44B83D40ADD14042`;
content fingerprint `4E88A9D42E217C298CC504C96A13538F4F2BEE501B32CE65B6525D1952EAEC97`.

The pin has two halves because a seed regeneration is **not byte-reproducible** (bincode walks
HashMap-backed catalogues in iteration order): the **file SHA-256** identifies the frozen
release, the **content fingerprint** (SHA-256 over canonical query evidence) identifies the
model and is stable across regenerations.

**Demo data exists only as CSV + SQL.** The `.bin` files are generated build outputs, not sources,
and they are never upgraded or migrated. With no users and the design still moving, changing demo
data means editing the CSV/SQL seed and regenerating. The catalogue format is **v3** and the loader
rejects older files outright. Regenerating model v2.2 from `examples/superstore-v2.sql` +
`examples/Plateau/superstore-raw.csv` reproduces the pinned content fingerprint (`4E88A9D4…`);
only the file bytes/SHA differ.
## 2. Where the artefacts live

| Path | What | Rule |
| --- | --- | --- |
| `shared\database.bin` | frozen model the shared 8080 server serves | replaced only by a promoted release (D14) |
| `shared\serve.exe` | frozen engine binary for that release | same |
| `Patricia\olap_engine\database.bin` | generated demo output (legacy v1; reseed rather than upgrade) | never served, never REPL-ed |
| `dev-work\database.bin` | dev instance scratch copy | may change freely |

## 3. Model v2 — raw Superstore line items

**Source:** `examples/Plateau/superstore-raw.csv` — 5,560 data rows, 21 columns, one row per
order line item. **Seed:** `examples/superstore-v2.sql` (`.import_measures` creates the cube
from the CSV header). **Grain:** one CUBE CELL per line item per measure; `Orders` therefore
stores 22,240 cells (5,560 line items × 4 measures). `describe.rowCount` counts stored cells,
so it reports **22,240**; the **line-item count is 5,560** and is checked by grouping the
dimensions with `Measure = 'Sales'`.

**Dataset `Orders`** — dimensions (header order, nothing dropped):

| Dimension | Cardinality | Dimension | Cardinality |
| --- | --- | --- | --- |
| RowID | 5,560 | CustomerName | 415 |
| OrderID | 1,600 | Segment | 3 |
| OrderDate | 929 | Country | 1 |
| ShipDate | 975 | City | 24 |
| ShipMode | 4 | State | 24 |
| CustomerID | 695 | PostalCode | 1,584 |
| Region | 4 | ProductID | 5,560 |
| Category | 3 | SubCategory | 16 |
| ProductName | 30 | OrderPriority | 4 |

Measures (measure dimension `Measure`, one member each): `Sales`, `Profit`, `Quantity`,
`Discount`.

**Acceptance totals** (must match exactly to £0.005):

| | Sales | Profit | Quantity | Discount |
| --- | --- | --- | --- | --- |
| **total** | 26,763,427.43 | 1,190,153.82 | 22,450 | 520.00 |

| Region | Sales | Profit | Quantity |
| --- | --- | --- | --- |
| Central | 6,436,941.57 | 356,587.93 | 5,643 |
| East | 6,319,711.15 | 261,155.47 | 5,330 |
| South | 6,441,942.55 | 334,630.96 | 5,731 |
| West | 7,564,832.16 | 237,779.46 | 5,746 |

Dates are string dimensions (`OrderDate` 2021-01-01 … 2024-12-28); they filter and group
normally. Derived Year/Quarter/Month are not materialised.

## 3a. Default members (model declaration + CLI override)

The model may declare default members; a model seed does it with `.model_default <dimension>
<member>` and `.save`. The declaration is part of the model and is persisted in the catalogue
(currently format v3; dimension name -> member name). If the model declares none, there is **no default** and a
query that omits the dimension aggregates all base members.

The CLI can override the model (`.default_member <dim> <member>`), suppress it
(`.default_member <dim> none`), or revoke the override (`.default_member <dim> model`); the
override is stored separately, so a revoked override restores the model declaration. Both layers
are revocable. `.default_member <dim>` shows model default, override and effective value.
## 4. Verifying

```powershell
Patricia\olap_engine\scripts\verify-model.ps1 -DatabaseFile shared\database.bin      # exact artefact + fingerprint
Patricia\olap_engine\scripts\verify-model.ps1 -DatabaseFile shared\database.bin -Regenerate
Patricia\olap_engine\scripts\verify-model.ps1 -BaseUrl http://127.0.0.1:8080/v1 -Token <bearer>
```

Exit `0` = matches; `1` = mismatch (every differing field printed); `2` = setup failure.
The verifier proves the **dataset manifest** (datasets, columns/kinds/cardinalities,
rowCount, measure totals, totals by Region, line-item count) and the **fingerprint**.

## 5. Regenerating the model

```
cd Patricia\olap_engine
target\debug\olap_engine.exe --db <scratch>\v2.bin examples\superstore-v2.sql
```

Script mode starts from an empty catalogue; the trailing `.save` produces the file (format v3). The raw
CSV is the single source: `.import_measures <csv> <cube> <Measure1,Measure2,...>` keeps every
non-measure column as a dimension and stores one cell per measure per row. Two runs agree on
the content fingerprint, not on file bytes.

## 6. Changing the model (versioning policy)

A generated file is disposable; never carry an old `.bin` forward across a format change - reseed. Any change to dataset, columns, cardinalities, row counts or checks is a **new model
version** (v3, …):

1. add/seed the new model and a new manifest; keep the old manifest as history;
2. publish the new file SHA-256 and content fingerprint (fingerprint stable across two runs);
3. the user promotes a new frozen pair (D14) and broadcasts the restart — a model change is a
   separate release event from an engine-code change;
4. the contract owner records it in `contract/SHARED-ENGINE.md`; clients must re-login and
   save before the restart.


**SQL-safe names:** `.import_measures` refuses a CSV header containing a dash (`SubCategory`),
because SQL identifiers cannot contain `-` (it parses as subtraction) — a cube built from such
a header would be unaddressable from the SQL surface. Give an explicit rename instead:
`.import_measures <csv> <cube> <measures> Old-Dash=NewName`. Model v2.2 therefore imports the
raw source's `Sub-Category` as `SubCategory`; the source file itself is unchanged.

v1 (the 36-row demo: `Sales`, `Pivot`, `Attributes`, `RegionAttr`, empty `CostCentre`/
`Geography`/`ProductClass`) is **history**: it is not served, not verified by default, and its
manifest remains for reference.