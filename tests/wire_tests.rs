//! Integration tests for the Plateau wire layer (`engine::wire`) — ENGINE-PROTOCOL.md v1.
//!
//! These cover the RESPONSE half of the protocol: the four operations and the JSON
//! shapes a Plateau client parses. The plan -> engine REQUEST half is covered by
//! `tests/plan_tests.rs`; this file is deliberately about what goes back on the wire.
//!
//! Normative source: `ENGINE-PROTOCOL.md` §1-§4 (kept in
//! `<workspace>/tableau-clone/docs/`). Where this file asserts a shape, the section
//! reference is in the test name.
//!
//! TWO TESTS HERE ARE CHARACTERISATIONS OF DEFECTS, NOT CONTRACTS. They are named
//! `*_is_ignored_*` and `*_today_*` and carry a comment saying what the correct
//! behaviour is. They exist so the current behaviour cannot change unnoticed; they
//! should be inverted, not deleted, when the defect is fixed. See the module-level
//! report note in `PLATEAU-WIRE-FINDINGS.md`.

use olap_engine::catalog::catalog::Catalog;
use olap_engine::cube::node::CellValue;
use olap_engine::engine::plan::{QueryRequest, PROTOCOL_VERSION};
use olap_engine::engine::wire::{self, SampleQuery, WireError};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A cube shaped to exercise every response field, with measured totals so the
/// numbers can be asserted exactly.
///
///   Sales(Region, Product, Measure) — 4 leaves x 2 measures = 8 cells
///     East/Widget/Sales = 100, East/Widget/Cost = 40
///     East/Gadget/Sales =  50, East/Gadget/Cost = 30   (East   Sales 150, Cost 70)
///     West/Widget/Sales = 200, West/Widget/Cost = 80
///     West/Gadget/Sales =  25, West/Gadget/Cost = 15   (West   Sales 225, Cost 95)
///                                                       grand total Sales = 375
///
///   Markets(Region) — a second dataset with no measure dimension, for the
///   non-pivoting shape.
fn build_catalog() -> Catalog {
    let mut catalog = Catalog::new();
    catalog.add_cube("Sales", &["Region", "Product", "Measure"], Some("Measure"), true);
    catalog.add_cube("Markets", &["Region"], None, true);

    {
        let cube = catalog.get_cube_mut("Sales").expect("Sales cube");
        let rows: [(&str, &str, &str, f64); 8] = [
            ("East", "Widget", "Sales", 100.0),
            ("East", "Widget", "Cost", 40.0),
            ("East", "Gadget", "Sales", 50.0),
            ("East", "Gadget", "Cost", 30.0),
            ("West", "Widget", "Sales", 200.0),
            ("West", "Widget", "Cost", 80.0),
            ("West", "Gadget", "Sales", 25.0),
            ("West", "Gadget", "Cost", 15.0),
        ];
        for (r, p, m, v) in rows {
            cube.write(&[r, p, m], CellValue::Numeric(v)).expect("write");
        }

        // FIXTURE SELF-CHECK. The data-import path is under active development, and
        // this fixture is built with the same write API the loaders use. If a change
        // there stops a numeric value landing as a readable leaf cell, EVERY
        // expectations-based test in this file would fail — and would look like a
        // wire-layer bug. Verify the fixture itself says what it claims, so the
        // failure points at the right layer.
        for (r, p, m, v) in rows {
            match cube.read_cell(&[r, p, m]) {
                Some(CellValue::Numeric(got)) => assert_eq!(
                    got, v,
                    "fixture self-check: {r}/{p}/{m} reads back {got}, expected {v}"
                ),
                other => panic!(
                    "fixture self-check: {r}/{p}/{m} did not read back as a numeric cell \
                     (got {other:?}). The write/import path changed; fix the fixture \
                     before believing a wire-layer failure."
                ),
            }
        }
    }
    {
        let cube = catalog.get_cube_mut("Markets").expect("Markets cube");
        cube.write(&["East"], CellValue::Numeric(10.0)).expect("write");
        cube.write(&["West"], CellValue::Numeric(20.0)).expect("write");
    }

    catalog
}

/// A minimal valid request; tests add what they need.
fn request(dataset: &str) -> QueryRequest {
    QueryRequest {
        dataset: dataset.to_string(),
        protocol_version: PROTOCOL_VERSION,
        group_by: Vec::new(),
        measures: Vec::new(),
        filters: Vec::new(),
        order_by: None,
        limit: None,
        expressions: Default::default(),
        domain_for: Vec::new(),
    }
}

/// Groups by `Region`.
fn by_region(p: &mut QueryRequest) {
    p.group_by.push(serde_json::from_value(json!({"name": "Region"})).expect("GroupBy"));
}

/// Adds a bare measure with the given aggregation.
fn measure(p: &mut QueryRequest, name: &str, aggregation: &str) {
    p.measures.push(
        serde_json::from_value(json!({"name": name, "aggregation": aggregation}))
            .expect("Measure"),
    );
}

/// The wire JSON of a value, for key-name assertions.
fn wire(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("serialises to JSON")
}

/// The object keys of a JSON object, sorted.
fn keys(value: &Value) -> Vec<String> {
    let mut k: Vec<String> = value
        .as_object()
        .expect("expected a JSON object")
        .keys()
        .cloned()
        .collect();
    k.sort();
    k
}

// ---------------------------------------------------------------------------
// §4 capabilities
// ---------------------------------------------------------------------------

#[test]
fn capabilities_section4_keys_and_honest_declarations() {
    let caps = wire::capabilities();
    let v = wire(&caps);

    // The exact wire keys (§4). A rename here silently breaks the client's
    // feature detection, which then *enables* features the engine lacks.
    assert_eq!(
        keys(&v),
        vec![
            "aggregations",
            "countDistinctExact",
            "expressionFunctions",
            "filterOperators",
            "maxRows",
            "protocolVersion",
            "supportsInlineDomain",
            "supportsLod",
        ],
        "capabilities must use these camelCase keys exactly"
    );

    assert_eq!(v["protocolVersion"], json!(PROTOCOL_VERSION));
    assert_eq!(v["protocolVersion"], json!(1), "protocol v1");
    assert_eq!(v["supportsLod"], json!(false), "LOD must not be claimed");
    assert_eq!(v["supportsInlineDomain"], json!(true));
    assert_eq!(v["countDistinctExact"], json!(true));
    assert_eq!(v["maxRows"], json!(100_000));
    assert!(
        v["expressionFunctions"].as_array().unwrap().is_empty(),
        "no evaluator yet, so no function names may be advertised"
    );
}

/// The advertised aggregations must be the ones that ACTUALLY RUN.
///
/// This is the honesty claim in §4 ("This drives the UI: aggregations absent from
/// `aggregations` are disabled rather than failing at query time"). **It currently
/// FAILS, and that failure is the finding** — see
/// `query_response_aggregation_field_is_ignored_today`, and the recommendation in
/// `PLATEAU-WIRE-FINDINGS.md`: either implement the six advertised aggregations or
/// narrow the list to `["SUM"]`.
///
/// It is written as the contract and marked `#[ignore]` so the suite stays green
/// while the defect is unfixed. **Remove the `#[ignore]` the moment the engine
/// implements them** — that is the signal this test is waiting for, and running it
/// with `cargo test -- --ignored` reproduces the defect on demand.
#[test]
#[ignore = "documented defect: every aggregation currently computes SUM; see PLATEAU-WIRE-FINDINGS.md"]
fn capabilities_aggregations_are_actually_implemented() {
    let mut catalog = build_catalog();
    let caps = wire::capabilities();

    // `Sales` for region West/Widget is a single leaf: 200.
    // Its correct answers per aggregation: SUM 200, AVG 200 (one leaf),
    // MIN 200, MAX 200, COUNT 1, COUNTD 1.
    for agg in &caps.aggregations {
        let mut p = request("Sales");
        by_region(&mut p);
        measure(&mut p, "Sales", agg);
        p.filters.push(
            serde_json::from_value(json!({
                "field": "Product", "scope": "dimension", "operator": "equals", "value": "Widget"
            }))
            .expect("Filter"),
        );

        let resp = wire::query(&mut catalog, &p).unwrap_or_else(|e| panic!("{agg} failed: {e:?}"));
        let west = resp
            .rows
            .iter()
            .find(|r| r.first() == Some(&json!("West")))
            .unwrap_or_else(|| panic!("no West row for {agg}: {:?}", resp.rows));
        let got = west[1].as_f64().expect("numeric cell");

        let want = match agg.as_str() {
            "SUM" | "AVG" | "MIN" | "MAX" => 200.0,
            "COUNT" | "COUNTD" => 1.0,
            other => panic!("capabilities advertises '{other}', which this test does not know"),
        };
        assert_eq!(
            got, want,
            "advertised aggregation {agg} returned {got}, expected {want}. \
             Advertised-but-unimplemented aggregations must be refused, not silently summed."
        );
    }
}

// ---------------------------------------------------------------------------
// §1 describe
// ---------------------------------------------------------------------------

#[test]
fn describe_section1_shape_and_measure_hint() {
    let catalog = build_catalog();
    let resp = wire::describe(&catalog, "Sales").expect("describe Sales");

    // §1 keys, snake -> camel exactly.
    assert_eq!(
        keys(&wire(&resp)),
        vec!["columns", "dataset", "primaryKey", "rowCount"],
        "CatalogResponse field names"
    );
    assert_eq!(resp.dataset, "Sales");
    assert_eq!(resp.row_count, 8, "rowCount is the leaf-cell count");
    assert_eq!(resp.primary_key, Value::Null);

    assert_eq!(resp.columns.len(), 3, "one entry per cube dimension");

    let region = &resp.columns[0];
    assert_eq!(region.name, "Region");
    assert_eq!(region.suggested_kind, "dimension");
    assert_eq!(region.data_type, "string");
    assert_eq!(region.logical_type, "string");
    assert_eq!(region.cardinality, 2, "East and West");
    assert_eq!(region.nullable, true);
    assert_eq!(region.not_null, true);
    assert_eq!(
        keys(&wire(region)),
        vec![
            "cardinality",
            "dataType",
            "logicalType",
            "max",
            "min",
            "name",
            "notNull",
            "nullable",
            "suggestedKind",
        ],
        "CatalogColumn field names"
    );

    // The measure dimension is hinted `measure`; the others `dimension`. A HINT
    // only (§1), but it must at least be self-consistent with dataType.
    let meas = resp
        .columns
        .iter()
        .find(|c| c.name == "Measure")
        .expect("Measure column present");
    assert_eq!(meas.suggested_kind, "measure");
    assert_eq!(meas.data_type, "number");
    assert_eq!(meas.logical_type, "real");
    assert_eq!(meas.cardinality, 2, "Sales and Cost members");

    for c in &resp.columns {
        if c.suggested_kind == "measure" {
            assert_eq!(c.data_type, "number", "a measure hint must be numeric");
        } else {
            assert_eq!(c.data_type, "string", "a dimension hint must be a string");
        }
    }
}

#[test]
fn describe_unknown_dataset_is_404() {
    let catalog = build_catalog();
    let err = wire::describe(&catalog, "Nope").expect_err("unknown dataset");
    assert_eq!(err.code, "unknown_dataset");
    assert_eq!(err.status(), 404);
    assert!(err.message.contains("Nope"), "message should name it: {}", err.message);
}

// ---------------------------------------------------------------------------
// §2 sample
// ---------------------------------------------------------------------------

/// §2: `rowCount` is the TOTAL row count, not `rows.length`. Plateau renders
/// "showing 3 of 8" from exactly this pair.
#[test]
fn sample_row_count_is_the_total_not_the_page_length() {
    let catalog = build_catalog();
    let resp = wire::sample(
        &catalog,
        "Sales",
        &SampleQuery { limit: Some(3), offset: None, columns: Vec::new() },
    )
    .expect("sample");

    assert_eq!(resp.row_count, 8, "rowCount is the TOTAL, not rows.len()");
    assert_eq!(resp.rows.len(), 3, "limit caps the page");
    assert_eq!(resp.columns, vec!["Region", "Product", "Measure"]);
    for row in &resp.rows {
        assert_eq!(row.len(), 3, "each row is as wide as `columns`");
    }

    assert_eq!(
        keys(&wire(&resp)),
        vec!["columns", "rowCount", "rows"],
        "SampleResponse field names"
    );
}

#[test]
fn sample_offset_windows_the_rows() {
    let catalog = build_catalog();
    let all = wire::sample(
        &catalog,
        "Sales",
        &SampleQuery { limit: Some(8), offset: None, columns: Vec::new() },
    )
    .expect("full sample");
    assert_eq!(all.rows.len(), 8, "the cube has 8 leaf cells");

    let window = wire::sample(
        &catalog,
        "Sales",
        &SampleQuery { limit: Some(2), offset: Some(2), columns: Vec::new() },
    )
    .expect("windowed sample");

    assert_eq!(window.row_count, 8, "rowCount stays the total when windowing");
    assert_eq!(window.rows.len(), 2);
    assert_eq!(window.rows[0], all.rows[2], "offset skips whole rows");
    assert_eq!(window.rows[1], all.rows[3]);
}

#[test]
fn sample_offset_past_the_end_is_empty_not_an_error() {
    let catalog = build_catalog();
    let resp = wire::sample(
        &catalog,
        "Sales",
        &SampleQuery { limit: Some(5), offset: Some(500), columns: Vec::new() },
    )
    .expect("an out-of-range offset is a valid query with no rows");
    assert!(resp.rows.is_empty());
    assert_eq!(resp.row_count, 8, "the total is still reported");
}

#[test]
fn sample_column_subset_restricts_and_orders_the_projection() {
    let catalog = build_catalog();

    // A subset, in the order asked for (not cube order).
    let resp = wire::sample(
        &catalog,
        "Sales",
        &SampleQuery {
            limit: Some(2),
            offset: None,
            columns: vec!["Product".into(), "Region".into()],
        },
    )
    .expect("subset sample");
    assert_eq!(resp.columns, vec!["Product", "Region"], "requested order wins");
    for row in &resp.rows {
        assert_eq!(row.len(), 2);
    }

    // Case-insensitive resolution to the cube's casing.
    let ci = wire::sample(
        &catalog,
        "Sales",
        &SampleQuery { limit: Some(1), offset: None, columns: vec!["rEgIoN".into()] },
    )
    .expect("case-insensitive column");
    assert_eq!(ci.columns, vec!["Region"], "the cube's casing is echoed back");

    // An unknown column is a typed client error, never a silent drop.
    let err = wire::sample(
        &catalog,
        "Sales",
        &SampleQuery { limit: Some(1), offset: None, columns: vec!["Nope".into()] },
    )
    .expect_err("unknown column must be refused");
    assert_eq!(err.code, "invalid_argument");
    assert_eq!(err.status(), 400);
    assert!(err.message.contains("Nope"), "message names the column: {}", err.message);
}

#[test]
fn sample_without_limit_is_invalid_argument() {
    let catalog = build_catalog();
    let err = wire::sample(
        &catalog,
        "Sales",
        &SampleQuery { limit: None, offset: None, columns: Vec::new() },
    )
    .expect_err("limit is required by §2");
    assert_eq!(err.code, "invalid_argument");
    assert_eq!(err.status(), 400);
}

#[test]
fn sample_unknown_dataset_is_404() {
    let catalog = build_catalog();
    let err = wire::sample(&catalog, "Nope", &SampleQuery { limit: Some(1), ..Default::default() })
        .expect_err("unknown dataset");
    assert_eq!(err.code, "unknown_dataset");
    assert_eq!(err.status(), 404);
}

/// §2 shows `rows: [["East", 100], ["West", 40]]` — a dimension and a MEASURE
/// VALUE. The engine returns the measure dimension's member NAME instead, so a
/// client profiling the sample sees a categorical column where the data is
/// numeric.
///
/// **CHARACTERISATION OF A DEFECT.** `sample` projects `dim.get_name(id)` for
/// every dimension including the measure dimension (`wire.rs`, the `selected`
/// projection). For Plateau — whose whole reason for calling `sample` is type
/// inference and profiling — this reports `Measure` as a string of member names.
/// Correct behaviour: a measure dimension's column should carry the stored
/// numeric value, as the §2 example shows. Invert this test when fixed.
#[test]
fn sample_returns_member_names_where_the_spec_shows_measure_values_today() {
    let catalog = build_catalog();
    let resp = wire::sample(
        &catalog,
        "Sales",
        &SampleQuery { limit: Some(8), offset: None, columns: Vec::new() },
    )
    .expect("sample");

    let measure_col = resp.columns.iter().position(|c| c == "Measure").expect("Measure column");

    // What the engine does today: every cell of the measure column is a member
    // name, and no cell is a number.
    for row in &resp.rows {
        let cell = &row[measure_col];
        assert!(
            cell.is_string(),
            "today the measure column carries member names, not values: {cell:?}"
        );
        assert!(
            cell == &json!("Sales") || cell == &json!("Cost"),
            "unexpected measure cell {cell:?}"
        );
    }
    assert!(
        resp.rows.iter().all(|r| r[measure_col].as_f64().is_none()),
        "no numeric measure VALUE appears anywhere in the sample"
    );

    // The contrast that makes the defect concrete: the data IS numeric, as the
    // query path proves for the same cells.
    let mut catalog2 = build_catalog();
    let mut p = request("Sales");
    by_region(&mut p);
    measure(&mut p, "Sales", "SUM");
    let q = wire::query(&mut catalog2, &p).expect("query");
    assert_eq!(q.rows[0][1], json!(150.0), "query sees numbers where sample shows names");
}

// ---------------------------------------------------------------------------
// §3 query
// ---------------------------------------------------------------------------

/// **THE invariant the protocol singles out** (§3: "measureLabels is the one to
/// get right ... indexed by measure position, not column position. Getting this
/// wrong was a real bug in Plateau").
///
/// One dimension plus two measures makes the two indices differ by exactly the
/// dimension count, which is the case that was broken.
#[test]
fn query_response_measure_labels_are_indexed_by_measure_position() {
    let mut catalog = build_catalog();
    let mut p = request("Sales");
    by_region(&mut p);
    measure(&mut p, "Sales", "SUM");
    measure(&mut p, "Cost", "SUM");

    let resp = wire::query(&mut catalog, &p).expect("query");

    assert_eq!(resp.columns, vec!["Region", "Sales", "Cost"]);
    assert_eq!(
        resp.measure_labels,
        vec!["SUM(Sales)", "SUM(Cost)"],
        "labels are pill labels, one per MEASURE"
    );
    assert_eq!(resp.measure_names, vec!["Sales", "Cost"], "output names of measures");
    assert_eq!(resp.dimension_names, vec!["Region"], "output names of dimensions");

    // The load-bearing assertion: labels are NOT a slice of columns, and the two
    // indices differ by the dimension count.
    assert_ne!(
        resp.measure_labels, resp.columns,
        "labels must not be the column names"
    );
    assert_eq!(
        resp.columns.len() - resp.measure_labels.len(),
        resp.dimension_names.len(),
        "with one dimension present the label index is offset by exactly that many columns"
    );
    assert_eq!(resp.measure_labels[0], "SUM(Sales)");
    assert_eq!(resp.columns[1], "Sales", "different name space to the label");

    // columnMeta agrees with the two name spaces, per column.
    assert_eq!(resp.column_meta.len(), resp.columns.len());
    let kinds: Vec<&str> = resp.column_meta.iter().map(|m| m.kind.as_str()).collect();
    assert_eq!(kinds, vec!["dimension", "measure", "measure"]);
    assert_eq!(
        resp.column_meta[0].name, "Region",
        "meta is indexed by COLUMN position"
    );
    assert_eq!(resp.column_meta[1].name, "Sales");
    assert_eq!(resp.column_meta[1].semantic_type, "quantitative");
    assert_eq!(resp.column_meta[1].data_type, "number");
    assert_eq!(resp.column_meta[0].semantic_type, "nominal");
}

#[test]
fn query_response_section3_keys_and_values() {
    let mut catalog = build_catalog();
    let mut p = request("Sales");
    by_region(&mut p);
    measure(&mut p, "Sales", "SUM");

    let resp = wire::query(&mut catalog, &p).expect("query");
    let v = wire(&resp);

    assert_eq!(
        keys(&v),
        vec![
            "columnMeta",
            "columns",
            "dimensionNames",
            "domain",
            "elapsedMs",
            "measureLabels",
            "measureNames",
            "rows",
            "truncated",
        ],
        "QueryResponse field names (§3)"
    );

    // §3 rows are NUMBERS for measures and strings for dimensions. A measure that
    // arrived as the string "150" would break the client's numeric formatting.
    assert_eq!(v["rows"], json!([["East", 150.0], ["West", 225.0]]));
    assert!(v["rows"][0][1].is_f64(), "measures must be JSON numbers");
    assert!(v["rows"][0][0].is_string(), "dimensions stay JSON strings");
    assert_eq!(v["truncated"], json!(false));
    assert!(v["elapsedMs"].is_u64(), "elapsedMs is a number");
    assert_eq!(v["domain"], json!({}), "no domainFor request, so an empty map");

    assert_eq!(
        keys(&v["columnMeta"][0]),
        vec![
            "dataType",
            "defaultAggregation",
            "kind",
            "logicalType",
            "name",
            "semanticType",
        ],
        "ColumnMeta field names"
    );
}

/// §3: "An empty `groupBy` with one measure is a **grand total** and must return
/// exactly one row."
#[test]
fn query_grand_total_without_group_by_is_exactly_one_row() {
    let mut catalog = build_catalog();
    let mut p = request("Sales");
    measure(&mut p, "Sales", "SUM");

    let resp = wire::query(&mut catalog, &p).expect("grand total");

    assert_eq!(resp.rows.len(), 1, "grand total is exactly one row");
    assert_eq!(resp.rows[0], vec![json!(375.0)], "100+50+200+25");
    assert!(resp.dimension_names.is_empty());
    assert_eq!(resp.measure_labels, vec!["SUM(Sales)"]);
}

/// A non-pivoting cube (no measure dimension) must still answer, and the label
/// list stays consistent with the measure list.
#[test]
fn query_on_a_cube_with_no_measure_dimension() {
    let mut catalog = build_catalog();
    let mut p = request("Markets");
    by_region(&mut p);

    let resp = wire::query(&mut catalog, &p).expect("Markets query");
    assert_eq!(resp.columns, vec!["Region"]);
    assert!(resp.measure_labels.is_empty(), "no measures requested, so no labels");
    assert_eq!(resp.column_meta[0].kind, "dimension");
}

/// A measure-less cube queried with NO measure named in the plan.
///
/// This is the shape `examples/Plateau/seed.sql` builds for
/// `Sales(Region, Segment, Category)` and then queries with
/// `SELECT Region, value FROM Sales`. Measured behaviour, which is NOT the obvious
/// guess:
///
///   * `groupBy: ["Region"]` with no measures returns a SINGLE `value` column whose
///     one row is the grand total as a STRING (`"30"`), not one row per region. The
///     `groupBy` is ignored because the planner only falls back to `value` when
///     `outputColumns` is empty, and "Region" already fills it.
///   * Naming `value` as an explicit measure gives the useful answer: one row per
///     region with NUMERIC cells.
///
/// So for a measure-less cube, the name `value` is a pseudo-measure and Plateau
/// must ask for it explicitly. Pinned in both directions so the difference cannot
/// drift silently.
#[test]
fn query_a_measureless_cube_without_naming_value_returns_only_the_grand_total() {
    let mut catalog = build_catalog();
    let mut p = request("Markets"); // Markets(Region) — no measure dimension
    by_region(&mut p); // group by Region, name no measures

    let resp = wire::query(&mut catalog, &p).expect("measure-less query");

    assert_eq!(
        resp.columns,
        vec!["Region"],
        "the groupBy is projected and there is NO `value` fallback in the plan layer"
    );
    assert_eq!(
        resp.rows,
        vec![vec![json!("East")], vec![json!("West")]],
        "one row per region, projected as a bare list of member names — the stored \
         numbers are simply not present in the response"
    );
    assert!(
        resp.measure_names.is_empty() && resp.measure_labels.is_empty(),
        "no measure was named, so no labels exist"
    );
    assert_eq!(resp.column_meta[0].kind, "dimension", "untyped, so a dimension");
}

/// The useful form for a measure-less cube: name `value` as a measure.
///
/// Needs no special support — `value` resolves like any other member of the
/// (absent) measure dimension, and the cells come back as JSON numbers.
#[test]
fn query_a_measureless_cube_naming_value_as_a_measure_returns_per_group_numbers() {
    let mut catalog = build_catalog();
    let mut p = request("Markets");
    by_region(&mut p);
    measure(&mut p, "value", "SUM");

    let resp = wire::query(&mut catalog, &p).expect("measure-less query with an explicit value measure");

    assert_eq!(resp.columns, vec!["Region", "value"]);
    assert_eq!(
        resp.rows,
        vec![
            vec![json!("East"), json!(10.0)],
            vec![json!("West"), json!(20.0)],
        ],
        "one row per region, numeric cells"
    );
    assert_eq!(resp.measure_names, vec!["value"]);
    assert_eq!(
        resp.measure_labels,
        vec!["SUM(value)"],
        "the label is built from the name the client used"
    );
    assert_eq!(resp.column_meta[1].kind, "measure", "now typed as a measure");

    // And a grand total is available the same way.
    let mut total = request("Markets");
    measure(&mut total, "value", "SUM");
    let resp = wire::query(&mut catalog, &total).expect("grand total");
    assert_eq!(resp.rows, vec![vec![json!(30.0)]]);
    assert_eq!(resp.columns, vec!["value"]);
}

/// §3 `domainFor`: the distinct values of a column arrive in the `domain` map.
#[test]
fn query_domain_for_returns_the_domain_map() {
    let mut catalog = build_catalog();
    let mut p = request("Sales");
    p.domain_for.push("Region".into());

    let resp = wire::query(&mut catalog, &p).expect("domain request");

    assert_eq!(
        resp.domain.get("Region"),
        Some(&vec!["East".to_string(), "West".to_string()]),
        "display order, one entry per member"
    );
    assert!(resp.rows.is_empty(), "a pure domain request computes no rows");
    assert!(resp.columns.is_empty());
    assert!(!resp.truncated);

    // An unknown column is a typed error, not an empty domain.
    let mut p2 = request("Sales");
    p2.domain_for.push("Nope".into());
    let err = wire::query(&mut catalog, &p2).expect_err("unknown domainFor column");
    assert_eq!(err.code, "invalid_argument");
}

#[test]
fn query_unknown_dataset_is_404_and_version_mismatch_is_400() {
    let mut catalog = build_catalog();

    let mut p = request("Nope");
    by_region(&mut p);
    let err = wire::query(&mut catalog, &p).expect_err("unknown dataset");
    assert_eq!(err.code, "unknown_dataset");
    assert_eq!(err.status(), 404);

    let mut p2 = request("Sales");
    p2.protocol_version = PROTOCOL_VERSION + 1;
    let err = wire::query(&mut catalog, &p2).expect_err("version mismatch");
    assert_eq!(err.code, "version_mismatch");
    assert_eq!(err.status(), 400, "a version mismatch is the client's fault");
    assert!(err.message.contains("v1"), "message states what we speak: {}", err.message);
}

/// §3 `truncated` is "true when `limit` cut the result". A result that is
/// *exactly* as long as the limit was NOT cut if the data happens to end there.
///
/// **CHARACTERISATION OF A DEFECT.** `build_query_response` derives
/// `truncated = rows.len() == limit`, because `execute_plan` applies the limit
/// before the wire layer sees the result and the pre-limit count is gone. So a
/// client paging a 2-row result with `limit: 2` is told there is more to fetch.
/// Correct behaviour needs the pre-limit row count threaded through from
/// `apply_order_and_limit`. Invert this test when fixed.
#[test]
fn query_truncated_is_true_when_the_result_exactly_fills_the_limit_today() {
    let mut catalog = build_catalog();
    let mut p = request("Sales");
    by_region(&mut p); // exactly two groups: East, West
    measure(&mut p, "Sales", "SUM");
    p.limit = Some(2);

    let resp = wire::query(&mut catalog, &p).expect("query");
    assert_eq!(resp.rows.len(), 2, "both groups fit");
    assert!(
        resp.truncated,
        "today an exactly-filled limit reports truncated = true even though \
         nothing was cut (the pre-limit count is not available here)"
    );

    // The control: a limit larger than the result proves it can report false.
    let mut p2 = request("Sales");
    by_region(&mut p2);
    measure(&mut p2, "Sales", "SUM");
    p2.limit = Some(3);
    let resp2 = wire::query(&mut catalog, &p2).expect("query");
    assert_eq!(resp2.rows.len(), 2);
    assert!(!resp2.truncated, "a limit that plainly did not bind reports false");
}

/// §3 lists `SUM | AVG | MEDIAN | MIN | MAX | COUNT | COUNTD | STDEV | VAR | ATTR`
/// and tells the engine to disable the rest via `capabilities.aggregations`.
///
/// **CHARACTERISATION OF A DEFECT, and the most serious one in this file.**
/// `execute_plan`/`build_slice_query` never reads `measure.aggregation`: every
/// aggregation computes SUM. The response then echoes the REQUESTED aggregation in
/// `measureLabels`, so the client renders a confident `AVG(Sales)` pill over a SUM.
/// MEDIAN is not even advertised, and it is still answered with a 200 and a SUM.
///
/// This is precisely the failure mode ENGINE-PROTOCOL.md exists to prevent: a
/// silent, plausible-looking wrong number. The correct behaviour is a typed
/// `unsupported_aggregation` (400) for anything not implemented — and an honest
/// `capabilities.aggregations`. Invert this test when fixed.
#[test]
fn query_response_aggregation_field_is_ignored_today() {
    let mut catalog = build_catalog();

    // Sales for East + West: SUM 375, AVG 187.5, COUNT 4 (leaves), COUNTD 2.
    let run = |catalog: &mut Catalog, agg: &str| {
        let mut p = request("Sales");
        measure(&mut p, "Sales", agg);
        wire::query(catalog, &p).unwrap_or_else(|e| panic!("{agg}: {e:?}"))
    };

    let sum = run(&mut catalog, "SUM");
    assert_eq!(sum.rows[0][0], json!(375.0), "SUM is the real total");

    for agg in ["AVG", "COUNT", "COUNTD", "MEDIAN"] {
        let resp = run(&mut catalog, agg);
        assert_eq!(
            resp.rows[0][0],
            json!(375.0),
            "{agg} returned the SUM: the aggregation is ignored, not applied"
        );
        assert_eq!(
            resp.measure_labels,
            vec![format!("{agg}(Sales)")],
            "{agg} is echoed back as the label, so the client cannot detect the wrong value"
        );
    }

    // MEDIAN is not in capabilities.aggregations at all, yet it is answered 200
    // instead of being refused with unsupported_aggregation.
    let advertised = wire::capabilities().aggregations;
    assert!(!advertised.contains(&"MEDIAN".to_string()));
    assert_eq!(run(&mut catalog, "MEDIAN").rows[0][0], json!(375.0));

    // And an aggregation the protocol does not even define is accepted too.
    let nonsense = run(&mut catalog, "NOT_A_REAL_AGGREGATION");
    assert_eq!(nonsense.rows[0][0], json!(375.0));
}

/// §3 `orderBy.output` is an OUTPUT column name, and an unknown one must be
/// `invalid_argument` rather than sorting by something else.
#[test]
fn query_order_by_uses_output_column_names_and_rejects_unknown_ones() {
    let mut catalog = build_catalog();

    let mut p = request("Sales");
    by_region(&mut p);
    measure(&mut p, "Sales", "SUM");
    p.order_by = Some(
        serde_json::from_value(json!({"output": "Sales", "direction": "desc"})).expect("OrderBy"),
    );
    let resp = wire::query(&mut catalog, &p).expect("ordered query");
    assert_eq!(
        resp.rows,
        vec![vec![json!("West"), json!(225.0)], vec![json!("East"), json!(150.0)]],
        "descending by the measure column"
    );

    // An unknown output column is the client's bug, and refused as such.
    let mut p2 = request("Sales");
    by_region(&mut p2);
    measure(&mut p2, "Sales", "SUM");
    p2.order_by = Some(
        serde_json::from_value(json!({"output": "Sum of Sales", "direction": "desc"}))
            .expect("OrderBy"),
    );
    let err = wire::query(&mut catalog, &p2).expect_err("unknown output column");
    assert_eq!(err.code, "invalid_argument");
}

// ---------------------------------------------------------------------------
// Error envelope and status mapping (§ "Errors")
// ---------------------------------------------------------------------------

/// Every code the protocol lists must map to the status the reference client
/// expects: client errors 400, a missing dataset 404, and `engine_error` alone
/// reserved for the engine's own faults.
#[test]
fn error_codes_map_to_the_documented_http_statuses() {
    let cases: [(&str, u16); 7] = [
        ("invalid_argument", 400),
        ("unsupported_aggregation", 400),
        ("unsupported_filter", 400),
        ("unsupported_expression", 400),
        ("version_mismatch", 400),
        ("unknown_dataset", 404),
        ("engine_error", 500),
    ];
    for (code, want) in cases {
        let e = WireError { code, message: "x".into() };
        assert_eq!(e.status(), want, "code '{code}' must map to {want}");
    }

    // An UNKNOWN code must not masquerade as a client error: anything
    // unrecognised falls through to the engine's fault, which is the safe
    // direction (a client retries/reports rather than blaming itself).
    let unknown = WireError { code: "something_new", message: "x".into() };
    assert_eq!(unknown.status(), 500);
}

#[test]
fn error_envelope_has_exactly_the_error_and_code_keys() {
    let e = WireError { code: "invalid_argument", message: "bad input".into() };

    // NOTE: there is no `From<&WireError> for ErrorResponse` (the transport builds
    // the envelope by hand in `serve.rs`). Asserting the shape here keeps both
    // producers honest.
    let env = wire::ErrorResponse { error: e.message.clone(), code: e.code.to_string() };

    let v = wire(&env);
    assert_eq!(keys(&v), vec!["code", "error"], "§Errors envelope keys");
    assert_eq!(v["error"], json!("bad input"));
    assert_eq!(v["code"], json!("invalid_argument"));
    assert_eq!(format!("{e}"), "bad input (invalid_argument)");
}

/// A `PlanError` must reach the client with both the protocol code and a human
/// message — the client distinguishes "my bug" from "engine fault" by `code`.
#[test]
fn plan_errors_convert_to_the_wire_envelope_with_their_code() {
    use olap_engine::engine::plan::PlanError;

    let pairs: [(PlanError, &str); 6] = [
        (PlanError::UnknownDataset("X".into()), "unknown_dataset"),
        (PlanError::UnsupportedFilter("f".into()), "unsupported_filter"),
        (PlanError::UnsupportedAggregation("AVG".into()), "unsupported_aggregation"),
        (PlanError::UnsupportedExpression("e".into()), "unsupported_expression"),
        (PlanError::InvalidArgument("bad".into()), "invalid_argument"),
        (
            PlanError::VersionMismatch { found: 9, supported: 1 },
            "version_mismatch",
        ),
    ];

    for (plan_err, want_code) in pairs {
        let env: wire::ErrorResponse = (&plan_err).into();
        assert_eq!(env.code, want_code);
        assert!(!env.error.is_empty(), "the message must not be empty");
        assert!(
            env.error.contains(&plan_err.to_string()) || env.error == plan_err.to_string(),
            "the envelope message should be the PlanError's text"
        );
        // And via WireError, the status follows the code.
        let we: WireError = plan_err.clone().into();
        assert_eq!(we.code, want_code);
        assert_eq!(we.status(), if want_code == "unknown_dataset" { 404 } else { 400 });
    }
}

// ---------------------------------------------------------------------------
// Request deserialisation (§3 request shape)
// ---------------------------------------------------------------------------

/// The request field names are as normative as the response ones: a client sends
/// `groupBy`, `domainFor`, `datePart`, `includeNulls`. A typo here would silently
/// drop a filter, which is far worse than an error.
#[test]
fn query_request_deserialises_the_spec_request_shape() {
    // The exact body shape from ENGINE-PROTOCOL.md §3.
    let body = json!({
        "dataset": "sales",
        "protocolVersion": 1,
        "groupBy": [{ "name": "Order Date", "datePart": "month" }],
        "measures": [
            { "name": "Sales", "aggregation": "SUM" },
            { "name": "Margin", "expr": { "type": "field", "name": "Margin" } }
        ],
        "filters": [
            { "id": "f1", "field": "Region", "scope": "dimension",
              "operator": "in", "values": ["East", "West"] }
        ],
        "orderBy": { "output": "Sum of Sales", "direction": "desc" },
        "limit": 25,
        "expressions": {},
        "domainFor": ["Region"]
    });

    let p: QueryRequest = serde_json::from_value(body).expect("spec request shape must parse");

    assert_eq!(p.dataset, "sales");
    assert_eq!(p.protocol_version, 1);
    assert_eq!(p.group_by.len(), 1);
    assert_eq!(p.group_by[0].name, "Order Date");
    assert_eq!(p.group_by[0].date_part.as_deref(), Some("month"));
    assert_eq!(p.measures.len(), 2);
    assert_eq!(p.measures[0].aggregation.as_deref(), Some("SUM"));
    assert!(p.measures[1].expr.is_some(), "the `expr` form must parse");
    assert_eq!(p.filters.len(), 1, "a filter must never be silently dropped");
    assert_eq!(p.filters[0].id.as_deref(), Some("f1"));
    assert_eq!(p.filters[0].values, vec!["East", "West"]);
    assert_eq!(p.order_by.as_ref().map(|o| o.output.as_str()), Some("Sum of Sales"));
    assert_eq!(p.limit, Some(25));
    assert_eq!(p.domain_for, vec!["Region"]);
}

/// Optional fields may be absent; `protocolVersion` defaults so an old client
/// still gets a defined answer (a *wrong* version is still refused, above).
#[test]
fn query_request_minimal_body_and_protocol_version_default() {
    let p: QueryRequest =
        serde_json::from_value(json!({ "dataset": "Sales" })).expect("minimal body parses");
    assert_eq!(p.protocol_version, PROTOCOL_VERSION, "protocolVersion defaults to v1");
    assert!(p.group_by.is_empty());
    assert!(p.measures.is_empty());
    assert!(p.filters.is_empty());
    assert!(p.order_by.is_none());
    assert!(p.limit.is_none());
    assert!(p.domain_for.is_empty());
}

/// An unknown `scope` or `operator` must be a parse error, not a default. A
/// mis-scoped filter returning 200 is the "Top 10" class of silent wrong answer.
#[test]
fn unknown_filter_scope_or_operator_fails_to_parse() {
    let bad_scope = json!({
        "dataset": "Sales",
        "filters": [{ "field": "Region", "scope": "row", "operator": "in", "values": ["East"] }]
    });
    assert!(
        serde_json::from_value::<QueryRequest>(bad_scope).is_err(),
        "an unknown scope must not silently default to `dimension`"
    );

    let bad_op = json!({
        "dataset": "Sales",
        "filters": [{ "field": "Region", "scope": "dimension", "operator": "regex", "value": "E" }]
    });
    assert!(
        serde_json::from_value::<QueryRequest>(bad_op).is_err(),
        "an unknown operator must not be silently ignored"
    );

    // And a filter that omits `scope` entirely DOES default to dimension, which
    // is the documented default (`default_filter_scope`).
    let defaulted: QueryRequest = serde_json::from_value(json!({
        "dataset": "Sales",
        "filters": [{ "field": "Region", "operator": "in", "values": ["East"] }]
    }))
    .expect("scope defaults");
    assert_eq!(
        format!("{:?}", defaulted.filters[0].scope),
        "Dimension",
        "absent scope defaults to the pre-aggregation scope"
    );
}

// ---------------------------------------------------------------------------
// Non-JSON helpers
// ---------------------------------------------------------------------------

#[test]
fn format_query_response_renders_a_table_or_zero_rows() {
    let mut catalog = build_catalog();

    let empty = wire::QueryResponse {
        columns: Vec::new(),
        column_meta: Vec::new(),
        rows: Vec::new(),
        dimension_names: Vec::new(),
        measure_names: Vec::new(),
        measure_labels: Vec::new(),
        domain: Default::default(),
        elapsed_ms: 0,
        truncated: false,
    };
    assert_eq!(wire::format_query_response(&empty), "0 rows returned.");

    let mut p = request("Sales");
    by_region(&mut p);
    measure(&mut p, "Sales", "SUM");
    let resp = wire::query(&mut catalog, &p).expect("query");
    let text = wire::format_query_response(&resp);

    assert!(text.starts_with("Region | Sales\n"), "header row first: {text:?}");
    assert!(text.contains("East | 150"), "rows rendered: {text:?}");
    assert!(text.contains("West | 225"));
}

#[test]
fn cell_to_value_keeps_numbers_numeric_and_strings_textual() {
    assert_eq!(wire::cell_to_value(&CellValue::Numeric(1.5)), json!(1.5));
    assert_eq!(
        wire::cell_to_value(&CellValue::String("Cash".into())),
        json!("Cash")
    );
    assert_eq!(wire::protocol_version(), PROTOCOL_VERSION);
}
