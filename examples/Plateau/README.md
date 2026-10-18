# olap_engine test fixtures

Generated from Plateau's bundled Superstore sample by
`node scripts/export-engine-fixtures.mjs`. Regenerate rather than edit.

The raw dump is included for completeness, but it is a poor cube fixture: with
`Row ID` and `Order ID` among the dimensions every row is its own coordinate and
nothing ever rolls up. The projected files below are the useful ones.

| File | Load with | What it exercises |
| --- | --- | --- |
| `superstore-raw.csv` | — | The original 22 columns, 5,560 rows |
| `cube-region-segment-category.csv` | `.import` (3 dims) | A dense cube: 36 of at most 48 coordinates populated, so every group-by returns real totals. Grand total **26,763,427.43**, verified against the raw data |
| `hierarchy-parent-child.csv` | `.import_pc` | Consolidation weights; two children can carry opposite signs, as a cost-centre tree does |
| `hierarchy-levels.csv` | `.import_lvl` | A ragged `All > Region > State > City` tree |
| `hierarchy-product.csv` | `.import_lvl` | `All > Category > Sub-Category` |
| `measure-pivot.csv` | `.import` (2 dims + Measure) | The measure pivot: unpivot back to a relational result |
| `attribute-strings.csv` | `.import` (2 dims) | String cells rather than numbers |

## Import contract

`.import` reads dimension members from columns `0..dim_count-1` and the value
from column `dim_count`. Any further trailing columns are ignored, so the raw
dump imports into a 21-dimension cube and silently yields no aggregation.

The square fixture is therefore the important one: it is small, fully populated,
and every number can be checked by hand against the raw data.
