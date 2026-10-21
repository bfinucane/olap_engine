# Calculated fields — design proposal

**Status:** proposal, for decisions. No code written.
**Today:** the plan layer accepts `expressions` and `Measure.expr`, then refuses them:
`calculated field 'X' (evaluator not implemented yet)`. `capabilities.expressionFunctions`
is empty and `supportsLod` is false — both honest, and both stay honest here.

---

## 1. The boundary is already agreed

This is the most important fact, and it is why this is a smaller job than it looks:

| | Engine (`src/engine/plan.rs`) | Plateau (`src/core/expression.ts`) |
|---|---|---|
| tags | `number, string, boolean, null, field, unary, binary, call, if, lod` | identical |
| shapes | `{type:'binary', op, left, right}`, `{type:'call', name, args}` | identical |

Plateau already parses the formula language (`[Sales] * 1.1`, `IF … THEN … END`,
`SUM([Sales])`, `{ FIXED [Region] : … }`) into that AST. The engine's `AstNode` is the
same shape, field for field.

So: **Plateau parses, the engine evaluates.** No second grammar in Patricia, no grammar
duplication to drift. A text box for typing formulas belongs to Plateau, not here — that
was the user's call and it is the right one.

## 2. What v1 evaluates

Ordered by value, and deliberately narrow:

1. **Aggregates** — `SUM/AVG/COUNT/MIN/MAX([Field])`. Each becomes an aggregation slot the
   existing engine computes; the expression combines their per-group values.
2. **Arithmetic** — `+ - * / % ^`, unary `-`, numeric literals.
3. **Comparisons and logic** — `= <> < <= > >=`, `AND OR NOT`, `IF(c, a, b)`.
4. **`ZN` and `IFNULL`** — the two functions that make a sparse cube usable in a ratio.

That is enough for the things people actually build first: margin (`SUM(Profit)/SUM(Sales)`),
variance, cost ratio, any `IF`-driven bucket.

**Not in v1** (each stays `unsupported_expression`, never a silent zero): `CASE`, string
functions, date functions, `ROUND`-family, and LOD. LOD is a separate query, not an
expression — see §6.

## 3. Decisions needed

### Decision 1 — division by zero

`SUM(Profit) / SUM(Sales)` where Sales is empty or zero.

**Recommend: `null`.** The alternatives are worse: `Infinity` is not JSON, `NaN` poisons
every later comparison, and `0` is a confident lie that looks like a real margin of zero.
A `null` cell travels as `null` (the wire already does that for absent measures).

### Decision 2 — an absent input

The aggregation semantics decided earlier are: a measure with no cells in a group is
**absent** (`-` internally, `null` on the wire), not `0`.

**Recommend: absence propagates.** If any operand is absent, the expression is absent —
except for `ZN`/`IFNULL`, which exist precisely to convert absence to `0`. Making `SUM(x)`
silently `0` inside an expression would resurrect the `MIN`-of-a-sparse-cube problem we
just designed out.

### Decision 3 — what `COUNT` means inside an expression

`COUNT([Sales])` in a formula is the same count as `COUNT` as a measure aggregation: the
number of non-empty cells. **Recommend: no special case**, so the two agree.

### Decision 4 — one aggregation per field, or several?

`SUM([Sales]) / AVG([Sales])` needs two slots for the same field. The engine's slice result
names columns by MEASURE, so two aggregations of one measure collide there.

**Recommend: v1 supports one aggregation per field** and refuses the mixed case with a
clear message naming the field. Multi-aggregation is a column-naming problem in
`query_slice` (the same one that makes `SELECT SUM(Sales), AVG(Sales)` collide in SQL), and
it should be fixed once, for both.

## 4. How it runs

1. Walk the AST; collect `(aggregation, field)` from each aggregate `call`. Reject a `call`
   whose name is not one of the five (naming the supported set), and a `field` that is not
   a measure of the cube.
2. Add those slots to the slice alongside the requested measures.
3. `query_slice` computes them per group as it does today — no new scan.
4. Evaluate the expression once per row, substituting each aggregate slot's value.
5. Emit the calculated column under `Measure.name`; the base slots are dropped (they are
   not output unless the user also asked for them).

Cost: one pass for the slots plus O(rows) evaluation. No extra scan, no cache change.

## 5. Honesty in capabilities

`expressionFunctions` currently advertises nothing, which is correct until something runs.
When v1 lands it should list exactly the functions implemented — `ZN`, `IFNULL`, and the
aggregate names already covered — and nothing else. `supportsLod` stays `false` until LOD
is genuinely evaluated; the module docs in `plan.rs` already explain why faking it is worse
than refusing it.

## 6. LOD is not an expression

`{ FIXED [Region] : SUM([Sales]) }` computes at its own grouping and attaches the result to
each row — a second query, then a join back on the fixed dimensions. That is a
different mechanism (and suspiciously close to the conformed-dimension join that already
exists). It stays refused, and a separate design owns it.

## 7. Tests this will need

- Golden: a calculated measure over a sparse cube, checking absence propagates and
  `ZN` converts it.
- Division by zero → `null`, not `0`/`Infinity`/`NaN`.
- An unsupported function → `unsupported_expression` naming it and listing the supported
  set (the failure mode to avoid is a silent zero).
- A field that is not a measure → error, not `null`.
- The same calculated field in SQL (`SELECT SUM(Profit)/SUM(Sales) FROM ...`) once SQL
  routes through the same evaluator — otherwise the two front doors drift, which is the
  exact defect this project keeps finding.

## 8. Open question for the user

Where should the **formula text** live? Plateau owns the parser today. Two coherent
answers:

1. **Plateau keeps parsing** (recommended for now): it sends the AST; the engine evaluates.
   One parser, one evaluator, no duplication. A formula console then belongs in Plateau,
   which is also where users would expect it.
2. **The engine parses** (Phase 3 "replace the front end"): the engine grows a Rust parser
   for the same grammar, SQL gains calculated members, and Plateau's becomes redundant.
   More work, and a grammar maintained in two languages until Plateau drops its copy.
