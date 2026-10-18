# Plateau wire layer — test findings

Test pass over the newly added Plateau support (`src/engine/wire.rs`,
`src/bin/serve.rs`, `src/engine/plan.rs`). Written by the test session; the code
under test came from the Continue session and was **not** modified.

## What was added

| File | Tests | Covers |
|---|---|---|
| `tests/wire_tests.rs` | **29 pass, 1 ignored** | All four operations as pure functions: exact wire field names (§1–§4), the `measureLabels` measure-position invariant, `rowCount`-is-total, offset windowing, column subsets, grand totals, `domainFor`, error codes and their HTTP statuses, the error envelope, measure-less cubes (both forms), and request deserialisation of the full §3 body. |
| `tests/serve_http_tests.rs` | **10 pass** | The real `serve` binary over a real socket: all four routes, status codes on the wire, the envelope as an actual body, `--cors` on and off, `OPTIONS` preflight, trailing slashes, unknown params, a >16 KB POST body (Content-Length handling), and `--db` loading a catalogue written by `save_to_disk`. |

Both fixture builders **self-check**: after writing each cell they read it back and
assert it is a numeric cell with the expected value. The data-import path is under
active development and shares that write API, so a loader change that stops numbers
landing should fail as a fixture problem, not masquerade as a wire-protocol bug.

Run them:

```powershell
Set-Location C:\Users\Startklar\Documents\GitHub\olap_engine
cargo test --offline --test wire_tests --test serve_http_tests
```

### Definitions frozen by these tests

- Fixture: `Sales(Region, Product, Measure)` — East/Widget/Sales 100, /Cost 40,
  East/Gadget 50/30, West/Widget 200/80, West/Gadget 25/15 → East Sales 150,
  Cost 70; West 225/95; grand total 375. Plus `Markets(Region)` with two cells.
- Servers are spawned one per test and killed by an RAII guard, so a failing test
  cannot leave a listening process behind. Verified: zero orphans after a run.

## Defects found

### 1. `measure.aggregation` is ignored — every aggregation computes SUM (serious)

`execute_plan` / `build_slice_query` never read `aggregation`. The request is
accepted, the engine computes a SUM, and `measureLabels` echoes the *requested*
aggregation back, so the client renders a confident `AVG(Sales)` pill over a sum:

```
{"measures":[{"name":"Sales","aggregation":"AVG"}]}
  -> measureLabels: ["AVG(Sales)"],  rows: [["East", 150.0], ["West", 225.0]]
     (150 and 225 are SUMs; the true AVGs are 75 and 112.5)
```

`COUNT`, `COUNTD` and `MIN`/`MAX` all return the same sums. `MEDIAN` — which
`capabilities.aggregations` does not even advertise — is answered `200` rather
than refused with `unsupported_aggregation`. So is a string that is not an
aggregation at all (`"NOT_A_REAL_AGGREGATION"`).

This is exactly the failure mode ENGINE-PROTOCOL.md exists to prevent: a silent,
plausible-looking wrong number, with the label confirming the client's mistake
rather than exposing it.

**Pinned by** `query_response_aggregation_field_is_ignored_today` (characterisation
of current behaviour) and `capabilities_aggregations_are_actually_implemented`
(`#[ignore]`d contract test — remove the `#[ignore]` when fixed).

**Fix:** implement the advertised aggregations, or narrow
`capabilities.aggregations` to `["SUM"]` and return `unsupported_aggregation` for
everything else. The narrow version is a small, honest change; the UI already
disables what is not advertised.

### 2. `sample` returns measure member names where §2 shows measure values

`sample` projects `dim.get_name(id)` for every dimension, including the measure
dimension. §2's example is `rows: [["East", 100], ["West", 40]]` — a dimension and
a measure *value*:

```
GET /v1/Sales/sample?limit=3  ->
  columns: ["Region","Product","Measure"]
  rows: [["East","Gadget","Cost"], ...]      <- member names, not 30.0
```

Plateau calls `sample` for type inference and profiling, so the measure column
arrives as strings of member names and will be inferred as categorical.

**Pinned by** `sample_returns_member_names_where_the_spec_shows_measure_values_today`.

### 3. A query that exactly fills `limit` reports `truncated: true` (minor)

`truncated = rows.len() == limit`, because `execute_plan` applies the limit before
the wire layer sees the result and the pre-limit count is gone. A 2-row result
fetched with `limit: 2` claims there is more. §3 says `truncated` is true when the
limit **cut** the result.

**Pinned by** `query_truncated_is_true_when_the_result_exactly_fills_the_limit_today`.

### 4. The golden exit-code check is a tautology (minor, test infrastructure)

`script_tests.rs` derives `expect_failure` from the golden text it is checking
(`expected.contains("Error:") || expected.contains("failed")`). Since `run_script`
returns false exactly when a command printed an error, the assertion can only
disagree with the transcript over the wording of a message. It adds no protection;
it also hides nothing. Worth making explicit data, not worth prioritising.

### 5. A measure-less cube returns no numbers at all through the plan layer (serious for Plateau)

`examples/Plateau/seed.sql` builds `Sales(Region, Segment, Category)` — a cube with
**no measure dimension**, the shape `.import` produces when the CSV's last column is
the cell value. That shape is the natural Plateau dataset (`cube-region-segment-category.csv`,
grand total 26,763,427.43 — verified independently against the raw CSV).

Through the plan layer, such a cube answers a `groupBy` with no measures like this:

```
groupBy: [{"name":"Region"}], measures: []
  -> columns: ["Region"]
     measureNames: [], measureLabels: []
     rows: [["East"], ["West"], ...]      <- member names only; NO values
```

The stored numbers are not in the response. Nothing warns; the client sees a
dimension and no measures, and can only conclude the cube is empty.

The name `value` **does** work if it is asked for explicitly
(`measures: [{"name":"value","aggregation":"SUM"}]` → `columns ["Region","value"]`,
numeric per-group cells, and a grand total with no `groupBy`). So the fix is small —
either teach the plan layer the same `value` fallback the SQL layer has
(`sql.rs` pushes `value` into `output_columns` when no measures are requested, which
is why `SELECT Region, value FROM Sales` in the seed script returns numbers), or
have Plateau always name `value`. Right now the two front doors disagree, and the
native one is the silent one.

**Pinned by** `query_a_measureless_cube_without_naming_value_returns_only_the_grand_total`
and `query_a_measureless_cube_naming_value_as_a_measure_returns_per_group_numbers`.

**Needs your call:** which convention should Plateau use? If `value` is the
convention, it belongs in the protocol document — it is currently discoverable only
by reading `sql.rs`.

### Not a defect, but worth knowing

- `capabilities.filterOperators` advertises `notEquals`, but `filter_values`
  rejects anything except `equals` and `in` with `unsupported_filter`. A client
  honouring the capability list will send `notEquals` and get a 400. Same class as
  defect 1: advertise only what runs. Not pinned by a test — the right fix depends
  on whether `notEquals` is about to be implemented.

## Environment notes (not engine bugs)

These cost real time during this pass; recording them so the next one does not
repeat it.

1. **The test sandbox forbids writes outside the session workspace.** Test
   binaries and the processes they spawn get `PermissionDenied` (os error 5) on
   anything under `%TEMP%`. That broke four suites in a way that *reads* as engine
   failure: `persistence_tests`, `script_tests` (the harness points the engine's
   `--out` at `%TEMP%`), and initially the new HTTP tests. All four now write under
   the session workspace, overridable with `OLAP_TEST_ARTIFACTS`. Fixing this is
   what makes `cargo test` meaningful at all here.
2. **A stale handle can pin `target\debug\serve.exe`.** While it is held, every
   rebuild fails with `failed to remove file ... serve.exe`. `Get-Process` shows no
   owner. Workaround used: build with `CARGO_TARGET_DIR` pointing elsewhere.
3. **Do not pipe `cargo` output.** `$LASTEXITCODE` is wrong when piped. Redirect to
   a file and read the code separately, as the earlier baseline report also notes.

## Status of the wider suite

`cargo test --offline` is currently **red**, for reasons unrelated to this work:

- `import_csv.sql` and `import_lvl_ragged.sql` golden mismatches — the engine in the
  working tree now emits `Warning: member 'Q1' ... promoted to an aggregate ...`,
  which the goldens do not contain. `dimension.rs` and `main.rs` are both modified
  and uncommitted, so the engine is mid-change. **Do not `UPDATE_GOLDEN=1`:** either
  the new warning is wanted and the goldens should be updated deliberately, or the
  warning is noise that should not be printed per member.
- `probe.sql`, `verify_probe.sql`, `persist_probe.sql` in `tests/scripts/` have no
  `.expected.txt`, so the harness reports them as missing goldens. They look like
  scratch fixtures from the other session — they should be deleted or given goldens.

Everything else passes: 14 storage unit tests, 3 concurrency, 6 persistence,
16 plan, 27 wire, 10 HTTP.
