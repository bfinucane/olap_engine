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
| **Model** | **`Superstore line items v2`** — file SHA-256 + content fingerprint from the manifest |

Frozen artefact (2026-10-30): 5,177,295 B,
SHA-256 `4F4F15969209CEC4B746FD607237F4F2D2033D7F077714164332F5197F802BCB`;
content fingerprint `E9423ED713438316C293EC2D58780915BFC14C15013964F548D03BDCBBFDB908`.

The pin has two halves because a seed regeneration is **not byte-reproducible** (bincode walks
HashMap-backed catalogues in iteration order): the **file SHA-256** identifies the frozen
release, the **content fingerprint** (SHA-256 over canonical query evidence) identifies the
model and is stable across regenerations.

## 2. Where the artefacts live

| Path | What | Rule |
| --- | --- | --- |
| `shared\database.bin` | frozen model the shared 8080 server serves | replaced only by a promoted release (D14) |
| `shared\serve.exe` | frozen engine binary for that release | same |
| `Patricia\olap_engine\database.bin` | seeded **v1** reference (kept as history) | never served, never REPL-ed |
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
| Row ID | 5,560 | Customer Name | 415 |
| Order ID | 1,600 | Segment | 3 |
| Order Date | 929 | Country | 1 |
| Ship Date | 975 | City | 24 |
| Ship Mode | 4 | State | 24 |
| Customer ID | 695 | Postal Code | 1,584 |
| Region | 4 | Product ID | 5,560 |
| Category | 3 | Sub-Category | 16 |
| Product Name | 30 | Order Priority | 4 |

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

Dates are string dimensions (`Order Date` 2021-01-01 … 2024-12-28); they filter and group
normally. Derived Year/Quarter/Month are not materialised.

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

Script mode starts from an empty catalogue; the trailing `.save` produces the file. The raw
CSV is the single source: `.import_measures <csv> <cube> <Measure1,Measure2,...>` keeps every
non-measure column as a dimension and stores one cell per measure per row. Two runs agree on
the content fingerprint, not on file bytes.

## 6. Changing the model (versioning policy)

Any change to dataset, columns, cardinalities, row counts or checks is a **new model
version** (v3, …):

1. add/seed the new model and a new manifest; keep the old manifest as history;
2. publish the new file SHA-256 and content fingerprint (fingerprint stable across two runs);
3. the user promotes a new frozen pair (D14) and broadcasts the restart — a model change is a
   separate release event from an engine-code change;
4. the contract owner records it in `contract/SHARED-ENGINE.md`; clients must re-login and
   save before the restart.

v1 (the 36-row demo: `Sales`, `Pivot`, `Attributes`, `RegionAttr`, empty `CostCentre`/
`Geography`/`ProductClass`) is **history**: it is not served, not verified by default, and its
manifest remains for reference.