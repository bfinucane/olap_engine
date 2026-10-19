# Non-additive aggregations — design proposal

**Status:** IMPLEMENTED (M1–M3 below). Decisions 1, 2 and 4 taken; decision 3 taken as the
honest default (COUNTD stays unimplemented, `countDistinctExact` now `false`).
**Today:** `Capabilities::default()` advertises `["SUM", "AVG", "COUNT", "MIN", "MAX"]`, and
`execute_plan` refuses anything else with `unsupported_aggregation` rather than returning a
sum under an `AVG` label.

---

## 0. What was built, and where it differed from this proposal

1. **"No data" was already `-`.** The proposal guessed the marker would be `""`; the engine
   had always used `-` (`query_slice` emits it for a measure with no cells in a group). So
   decision 4 needed no type change — only a wire mapping: `-` → JSON `null`. Found while
   implementing; the SQL transcript keeps `-`.
2. **SQL supports aggregate functions too**, since the engine works "based on the same
   internal routines": `SELECT AVG(Sales) FROM T WHERE ...`. The output column is named
   after the MEASURE (matching a bare `SELECT Sales`), so two aggregations of the same
   measure in one statement collide — that needs a column-mapping feature, not a silent one.
3. **The SQL dialect injects a default member** for every dimension that is neither an axis
   nor filtered, so `SELECT Region, Sales` means "Product = its first member", not "all
   products". A multi-cell group in SQL therefore has to come from a hierarchy or a
   `WHERE` on a consolidated member. This surprised the first fixture written for it.
4. **`COUNTD` remains unimplemented and `countDistinctExact` is now `false`.** It previously
   claimed `true` for a capability that never ran, which is the kind of claim this project
   has been careful not to make.

Tests: `tests/aggregation_tests.rs` (semantics below the SQL/wire layers),
`tests/scripts/aggregation_functions.sql` (end-to-end golden, including the fractional-weight
and absent-measure cases), and the updated `capabilities`/`wire` pins.

---

## 1. What the engine already gives us

`Cube::query_slice` (`src/cube/cube.rs:486`) does the whole job in one pass:

1. resolve each axis/column to leaf id → weight maps (`weight_maps`),
2. `store.scan_subcube(&scanner_filters)` — **only stored cells are visited**,
3. per cell, multiply by each dimension's weight,
4. accumulate into `grouped_results[row_key][measure_name]` with `+=`.

Step 4 is the only place SUM is hardcoded. Everything non-additive needs the same pass with
a different accumulator, and **no extra scan and no extra memory per cell** — which answers
the "can nonempties be counted efficiently?" question directly: yes, because the scan is
already sparse. Counting non-empty cells is a side effect of visiting them. Counting
*empty* cells would be arithmetic on the cross-product of leaf counts, also free, but is a
different number (see §3).

## 2. Two facts that shape the design

**Cells are the unit, not rows.** There is no row store: a cell lives at a leaf coordinate.
"COUNT" therefore counts the non-empty cells contributing to a group — matching
`COUNT(measure)` in SQL, not `COUNT(*)`. The user's framing was "only base values are
counted", which is exactly this: base (leaf) cells, never consolidated members.

**Weights are per dimension and fractional.** A ragged hierarchy gives a leaf a weight like
0.5, and SUM multiplies by it. Non-additive aggregations must decide what a weight means,
and there is no universally right answer — see decision 2.

## 3. Proposed semantics

| Aggregation | Definition over one group's non-empty leaf cells | Empty group |
|---|---|---|
| `SUM` | Σ v (today, weighted) | `0` (today's behaviour, keep) |
| `COUNT` | number of non-empty leaf cells | `0` |
| `AVG` | mean of the base leaf values | `null` |
| `MIN` / `MAX` | smallest / largest base leaf value | `null` |
| `COUNTD` | **deferred** (see decision 3) | — |
| `MEDIAN`, `STDEV`, `VAR` | out of scope for now: they need either two passes or all values held per group | — |

"Base leaf value" means the stored `CellValue::Numeric` **before** any weight is applied.
That is a deliberate difference from SUM on a ragged hierarchy: see decision 2.

## 4. Decisions needed

### Decision 1 — empty cells (the question you raised)

**Recommend:** ignore them.
- `COUNT` = non-empty cells. An empty cell is not a value; counting it would make
  `COUNT` a property of the schema (the cross-product size) rather than of the data.
- `AVG` divides by the non-empty count, so a sparse group is not dragged toward zero —
  which is what would happen if empties were treated as 0.
- `MIN`/`MAX` skip them. Treating empty as 0 would make `MIN` return 0 for almost every
  sparse cube, which is useless.
- `SUM` keeps today's behaviour (`0`), because that is what makes additive rollups work.

`SUM` and the others therefore disagree about empty groups (`0` vs `null`). That is
correct and worth stating in the protocol: additive totals have an identity element,
min/avg do not.

If you would rather have `COUNT(*)` too (all possible leaf combinations), it is cheap —
the cross-product of `get_leaf_descendants_in` sizes, no scan — but it needs its own name
(`COUNT_ROWS`?) or an explicit flag, because silently redefining `COUNT` would break the
SQL meaning.

### Decision 2 — what a fractional weight means for MIN/MAX/AVG

**Recommend: ignore weights; aggregate the base leaf values.**

- Two cells with values 10 and 20 under a parent whose weights are 0.5 and 1.0:
  `SUM` = 25, `AVG` = 15, not (0.5·10 + 20)/1.5 = 13.3.
- It is explainable in one sentence ("the non-additive aggregations look at the stored
  values, not the rollup shares"), and it is what "only base values are counted" implies.
- Consequence to document: on a ragged hierarchy, `AVG ≠ SUM / COUNT`. That is not a bug;
  they are answers to different questions.

**The alternative** is a weighted mean, Σ(w·v)/Σw, which keeps `AVG = SUM/COUNT` whenever
all weights are 1. It breaks down exactly where this engine is most interesting: a
hierarchy with signed weights (Profit = Revenue − Cost) can make Σw = 0, and the average
of a subtraction has no obvious meaning. Supporting it would need a rule for the zero
denominator, which is a smell.

**A third option** is to refuse non-additive aggregations whenever a query touches a
member with a weight ≠ ±1. Honest, and it keeps the engine exactly additive. It also means
`AVG` fails on the ragged hierarchies that are the product's distinguishing feature, so I
do not recommend it.

### Decision 3 — COUNTD

**Recommend: do not do it yet; keep `countDistinctExact` honest.**
- `COUNTD` of a dimension member is "distinct values of a column", but storage is a cube:
  it becomes "distinct coordinates of dimension D among the group's non-empty cells",
  which needs a `HashSet` per group — memory proportional to cardinality × groups, the one
  aggregation that is not O(1) per group.
- `capabilities.countDistinctExact` is currently `true`, and since `COUNTD` is not
  advertised it is never exercised. If COUNTD stays unimplemented, that flag should become
  `false`, because a client that reads it today is being told something the engine cannot
  back up.

### Decision 4 — how "no value" travels

`AVG`/`MIN`/`MAX` over an empty group need a representation. Today a measure cell is a
string, and a non-numeric one goes to the client as a JSON string.
- **Recommend:** `null` on the wire (the plan layer already appends `null` for a missing
  joined attribute, so the client can handle it) and `-` in the SQL transcript, matching
  the engine's existing "no data" convention.
- This needs a small wire change: the measure branch currently parses `f64` and falls back
  to `json!(cell)`; it needs an explicit "this cell is absent" marker rather than `""`.

## 5. Implementation sketch

1. **`SliceQuery` gains per-measure aggregation**: `aggregations: HashMap<String, Aggregation>`
   (keyed by measure name), so one query may ask `SUM(Sales), AVG(Cost)`.
2. **`Accumulator` per (group, measure)**:
   ```rust
   struct Acc { sum: f64, count: u64, min: f64, max: f64 }
   ```
   The loop keeps `raw` (before weighting) alongside the weighted value it already computes.
   Four `f64`s per group per measure is the whole extra cost.
3. **Finalise per measure** when the row is emitted: `SUM` → `sum`, `COUNT` → `count`,
   `AVG` → `sum(raw)/count`, `MIN`/`MAX` → the extremes or absent.
4. **`Capabilities`** advertises exactly the implemented set; the existing
   "advertised ⇒ accepted, un-advertised ⇒ refused" test keeps it honest.
5. **Non-additive cubes** (attribute tables) are untouched: no math there today.
6. **String cells** under an aggregation stay an error rather than being coerced —
   `MIN` of a text column is a separate feature, not a free one.

## 6. Tests and the honest failure modes

- SQL goldens for `AVG`, `MIN`, `MAX`, `COUNT` on the seeded cube, including a sparse cell
  set so the empty-cell rule is pinned rather than assumed.
- A ragged-hierarchy test asserting `AVG ≠ SUM/COUNT` and *saying why*, so the difference
  cannot later be "fixed" into the weighted version by accident.
- `MIN` on a group whose cells are all absent → `null`, not `0`.
- Capabilities/acceptance round trip, both directions.
- A `SUM` regression test proving the accumulator refactor did not change additive results
  — every existing golden is that test, if they stay byte-identical.

## 7. What this does not include

`MEDIAN`, `STDEV`, `VAR` (need all values per group, or a second pass), `COUNT(*)`
(needs a name), and `COUNTD` (decision 3). All four stay unadvertised, which keeps the
capability list honest rather than aspirational.

## 8. Deferred: averages that COUNT THE EMPTY CELLS (dense / period average)

Raised by the user while accepting decision 1: *"some values, like monthly average, will
always count the empty cells"*. That is correct, and it is a DIFFERENT FUNCTION — not a
change to `AVG`.

- `AVG` answers "what is the mean of the values that exist?". Denominator = non-empty
  cells. A year with data in 9 of 12 months divides by 9.
- A monthly average asks "what is the mean across the periods?". Denominator = the
  periods, so those same 9 months divide by 12, and an empty month contributes 0.

Do not "fix" `AVG` to be the second one: for a sparse cube the first is what a user means
by "average sales per order", and silently switching denominators would turn a measured
number into a fabricated one. They need separate names.

**Cost: none.** The denominator is arithmetic on metadata the engine already has — the
product of `get_leaf_descendants_in(hier, member).len()` over the group's members, per the
dimensions designated as "period" axes. No scan, no per-cell memory. The accumulator
already holds the numerator (`weighted` for SUM).

**Open questions before implementing:**

1. **Name.** `AVG_DENSE`, `AVG_INCLUDE_EMPTY`, `PERIOD_AVG`? The protocol's aggregation
   vocabulary is fixed by ENGINE-PROTOCOL.md §3 (`SUM | AVG | MEDIAN | ...`), and this is
   not in it — so it needs either a protocol addition or a `dense: true` flag on the
   measure.
2. **Which dimensions supply the denominator.** With one period axis (Year -> 12 months)
   the answer is clear. With several dimensions the cross-product counts cells, not
   periods: is a monthly average over `Year x Product` divided by 12, or by
   12 x number-of-products? Almost certainly the former, which means the denominator is
   per-dimension and the flag has to say which dimension is the period.
3. **Empty vs absent.** A period with no cells at all contributes 0 to the sum and 1 to the
   count. A period that does not EXIST (not a member of the dimension) surely does not
   count. That distinction is the whole reason this needs its own name.

Until those are settled it stays unadvertised, and `AVG` keeps the meaning pinned by
`tests/scripts/aggregation_functions.sql`.
### 8b. The denominator is a product of PER-DIMENSION count rules (user's refinement)

Follow-up from the user: the numerator is one sum, but the denominator may need a
*different count method per dimension*. So:

    dense_average = SUM / PRODUCT over dimensions d of count(d)

where each dimension picks how it counts, and the choices compose:

| count rule | meaning | cost |
|---|---|---|
| `all` (dense) | every leaf member under the group's member, data or not | **free** — `get_leaf_descendants_in(...).len()` |
| `present` (sparse) | distinct leaf members that actually hold a cell | a set per (group, dimension) → memory O(cells in the group) |
| `domain` | a declared count (e.g. 12, from a calendar attribute) | depends on where the number comes from |

Worked example — "average monthly sales per active region" over `Calendar x Region`:

    denominator = all(Calendar) x present(Region) = 12 x (regions with data)

That is a defensible metric, and notably one that neither plain `AVG` (divides by non-empty
cells) nor a single-flag dense average can express.

**Two consequences worth deciding before building it:**

1. **The cost asymmetry is real.** `all` is arithmetic on metadata; `present` needs a
   distinct-member set per group per dimension, i.e. memory proportional to the cells in
   that group. The rule that makes the metric interesting is the one that is not free, so
   this belongs in the same measured-gate discipline as Phase 1b rather than being assumed
   cheap.
2. **Presence must stay binary.** Under a fractional weight (`.rollup Half England 0.5`),
   does England count as present once, or 0.5 times? `AVG` already ignores weights for the
   base values, so presence should be binary too — a cell either exists or it does not.
   Consistent, and it avoids a denominator that is itself a weighted sum.

**Protocol shape (not yet decided).** The aggregation vocabulary in ENGINE-PROTOCOL.md §3 is
a flat list of names, and none of these is in it. Either a new name plus a per-dimension
spec, e.g.

    "measures": [{ "name": "Sales", "aggregation": "AVG",
                   "denominator": { "Calendar": "all", "Region": "present" } }]

or a `dense: true` flag (which cannot express the per-dimension part). This is a
Plateau-facing decision before it is an engine implementation.