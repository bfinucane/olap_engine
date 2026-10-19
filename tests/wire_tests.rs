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

/// The advertised aggregations must be exactly the ones that RUN, in both
/// directions: everything advertised is accepted, and nothing un-advertised is
/// quietly accepted either.
///
/// This is the honesty claim in §4 ("This drives the UI: aggregations absent from
/// `aggregations` are disabled rather than failing at query time"). The engine is
/// additive-only today, so this asserts `["SUM"]` — and it is written to keep
/// working when more are added: it never hardcodes the expected list, it only
/// requires the advertisement and the implementation to agree.
#[test]
fn capabilities_aggregations_match_what_actually_runs() {
    let mut catalog = build_catalog();
    let caps = wire::capabilities();

    // `Sales` filtered to Widget has one leaf per region: West = 200.
    // SUM is 200; any correct future non-additive aggregation also has one leaf to
    // work on here, which is why this fixture is used rather than a multi-leaf group.
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

        let resp = wire::query(&mut catalog, &p)
            .unwrap_or_else(|e| panic!("advertised aggregation {agg} was refused: {e:?}"));
        let west = resp
            .rows
            .iter()
            .find(|r| r.first() == Some(&json!("West")))
            .unwrap_or_else(|| panic!("no West row for {agg}: {:?}", resp.rows));
        assert_eq!(
            west[1].as_f64(),
            Some(200.0),
            "advertised aggregation {agg} must compute the single-leaf value 200"
        );
    }

    // The converse: every aggregation the protocol names but this engine does not
    // advertise must be REFUSED, never silently answered.
    for agg in ["AVG", "MEDIAN", "MIN", "MAX", "COUNT", "COUNTD", "STDEV", "VAR", "ATTR"] {
        assert!(
            !caps.aggregations.iter().any(|a| a == agg),
            "{agg} is not implemented, so it must not be advertised"
        );
        let mut p = request("Sales");
        measure(&mut p, "Sales", agg);
        let err = wire::query(&mut catalog, &p)
            .err()
            .unwrap_or_else(|| panic!("{agg} is not advertised, so it must be refused"));
        assert_eq!(err.code, "unsupported_aggregation");
    }

    // And it is additive-only today, which is a deliberate position:
    assert_eq!(
        caps.aggregations,
        vec!["SUM".to_string()],
        "if this list grows, the refusal list in execute_plan must grow with it"
    );
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
    // A dimension coordinate is always present for a returned row, and never null.
    // (The old code claimed `nullable` AND `notNull` at once, which cannot be true.)
    assert_eq!(region.nullable, false, "a dimension member is never null");
    assert_eq!(region.not_null, true);
    assert_eq!(region.min, Value::Null, "bounds are not known for a text column");
    assert_eq!(region.max, Value::Null);
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
    // Sparse cells: a coordinate can have no value, so a measure may be absent.
    assert_eq!(meas.nullable, true);
    assert_eq!(meas.not_null, false);
    // Bounds come from the stored cells: 15 is West/Gadget/Cost, 200 is
    // West/Widget/Sales.
    assert_eq!(meas.min, json!(15.0));
    assert_eq!(meas.max, json!(200.0));

    for c in &resp.columns {
        if c.suggested_kind == "measure" {
            assert_eq!(c.data_type, "number", "a measure hint must be numeric");
            assert_eq!(c.not_null, false, "sparse values may be absent");
        } else {
            assert_eq!(c.data_type, "string", "a dimension hint must be a string");
            assert_eq!(c.not_null, true, "a dimension member is always present");
            assert_eq!(c.nullable, false);
        }
    }
}

/// §1: the measure column's TYPE comes from what it holds, not from the fact that
/// it is the measure dimension.
///
/// An attribute cube (`CREATE ATTRIBUTE TABLE`) has a measure dimension whose cells
/// are STRINGS — `RegionAttr.Attribute` holds `"Ava Thompson"`. The old code
/// reported every measure dimension as `number`/`real`/`measure`, so a client
/// typed a text column as numeric and failed to render it.
#[test]
fn describe_types_an_attribute_measure_dimension_as_text() {
    let mut catalog = Catalog::new();
    catalog.add_cube("RegionAttr", &["Region", "Attribute"], Some("Attribute"), false);
    {
        let cube = catalog.get_cube_mut("RegionAttr").expect("cube");
        cube.write(&["Central", "Manager"], CellValue::String("Ava Thompson".into()))
            .expect("write");
        cube.write(&["West", "Manager"], CellValue::String("Dan Ellis".into()))
            .expect("write");
    }

    let resp = wire::describe(&catalog, "RegionAttr").expect("describe RegionAttr");
    assert_eq!(resp.row_count, 2, "one cell per attribute row");

    let attr = resp
        .columns
        .iter()
        .find(|c| c.name == "Attribute")
        .expect("Attribute column");
    assert_eq!(attr.data_type, "string", "the cells are text");
    assert_eq!(attr.logical_type, "string");
    assert_eq!(
        attr.suggested_kind, "dimension",
        "text is not summable, so it must not be hinted as a measure"
    );
    assert_eq!(attr.min, Value::Null, "no numeric bounds for text");
    assert_eq!(attr.max, Value::Null);
    assert_eq!(attr.cardinality, 1, "the single attribute member 'Manager'");

    // The plain dimension is unchanged.
    let region = &resp.columns[0];
    assert_eq!(region.data_type, "string");
    assert_eq!(region.suggested_kind, "dimension");
}

/// With no stored cells there is nothing to inspect, so the type falls back to the
/// cube's declared kind: a transactional cube's measure is numeric, an attribute
/// table's is text. Better than guessing `number` for everything.
#[test]
fn describe_types_an_empty_measure_dimension_from_the_declared_kind() {
    let mut catalog = Catalog::new();
    catalog.add_cube("EmptyNum", &["Region", "Amount"], Some("Amount"), true);
    catalog.add_cube("EmptyAttr", &["Region", "Prop"], Some("Prop"), false);

    let num = wire::describe(&catalog, "EmptyNum").expect("describe");
    let amount = num.columns.iter().find(|c| c.name == "Amount").expect("Amount");
    assert_eq!(amount.data_type, "number", "a transactional measure defaults numeric");
    assert_eq!(amount.suggested_kind, "measure");
    assert_eq!(amount.min, Value::Null, "no cells, so no bounds");
    assert_eq!(num.row_count, 0);

    let attrs = wire::describe(&catalog, "EmptyAttr").expect("describe");
    let prop = attrs.columns.iter().find(|c| c.name == "Prop").expect("Prop");
    assert_eq!(prop.data_type, "string", "an attribute measure defaults textual");
    assert_eq!(prop.suggested_kind, "dimension");
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
/// VALUE. A measure dimension's coordinate is a member NAME; the value is the
/// stored cell, so the measure column must carry the cell.
///
/// This matters beyond tidiness: Plateau's reason for calling `sample` is type
/// inference and profiling, so a measure column full of member names is inferred
/// as categorical and the numeric data disappears from its view.
#[test]
fn sample_measure_column_carries_values_not_member_names() {
    let catalog = build_catalog();
    let resp = wire::sample(
        &catalog,
        "Sales",
        &SampleQuery { limit: Some(8), offset: None, columns: Vec::new() },
    )
    .expect("sample");

    let measure_col = resp.columns.iter().position(|c| c == "Measure").expect("Measure column");

    // Every measure cell is a JSON number, and each is one of the eight stored
    // values — never the strings "Sales" / "Cost".
    let mut values = Vec::new();
    for row in &resp.rows {
        let cell = &row[measure_col];
        let n = cell
            .as_f64()
            .unwrap_or_else(|| panic!("measure cells must be numeric, got {cell:?}"));
        assert!(
            cell != &json!("Sales") && cell != &json!("Cost"),
            "member names must not appear in a measure column: {cell:?}"
        );
        values.push(n);
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(
        values,
        vec![15.0, 25.0, 30.0, 40.0, 50.0, 80.0, 100.0, 200.0],
        "the eight stored values, in order"
    );

    // The real dimensions still carry member names.
    let region_col = resp.columns.iter().position(|c| c == "Region").expect("Region column");
    for row in &resp.rows {
        assert!(row[region_col].is_string(), "a dimension stays a name");
    }

    // And it agrees with the query path for the same cells.
    let mut catalog2 = build_catalog();
    let mut p = request("Sales");
    by_region(&mut p);
    measure(&mut p, "Sales", "SUM");
    let q = wire::query(&mut catalog2, &p).expect("query");
    assert_eq!(q.rows[0][1], json!(150.0), "query and sample now agree these are numbers");
}

// ---------------------------------------------------------------------------
// §3 query
// ---------------------------------------------------------------------------

/// Measure values are reported at bounded precision (15 significant digits).
///
/// f64 summation accumulates noise in the last digits — the seeded catalogue's
/// grand total came out as `26763427.430000003` — and that is what the client
/// would render. Rounding to 15 significant digits removes the noise while keeping
/// every digit a double can meaningfully carry.
#[test]
fn query_measure_values_are_reported_at_bounded_precision() {
    // The motivating case, in miniature: 0.1 + 0.2 != 0.3 in binary floating point.
    assert_ne!(0.1_f64 + 0.2_f64, 0.3_f64, "the noise this guards against is real");

    let mut catalog = Catalog::new();
    catalog.add_cube("N", &["K", "Measure"], Some("Measure"), true);
    {
        let cube = catalog.get_cube_mut("N").expect("cube");
        cube.write(&["a", "V"], CellValue::Numeric(0.1)).expect("write");
        cube.write(&["b", "V"], CellValue::Numeric(0.2)).expect("write");
    }

    let mut p = request("N");
    measure(&mut p, "V", "SUM");
    let resp = wire::query(&mut catalog, &p).expect("query");
    assert_eq!(
        resp.rows[0][0],
        json!(0.3),
        "0.1 + 0.2 must serialize as 0.3, not 0.30000000000000004"
    );

    // A large magnitude, where the artifact appeared in the seeded data.
    let mut catalog2 = Catalog::new();
    catalog2.add_cube("Big", &["K", "Measure"], Some("Measure"), true);
    {
        let cube = catalog2.get_cube_mut("Big").expect("cube");
        cube.write(&["a", "V"], CellValue::Numeric(26_763_427.43)).expect("write");
    }
    let mut p2 = request("Big");
    measure(&mut p2, "V", "SUM");
    let resp2 = wire::query(&mut catalog2, &p2).expect("query");
    assert_eq!(
        resp2.rows[0][0],
        json!(26_763_427.43),
        "the seeded grand total must not surface f64 noise"
    );
}

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

/// A non-pivoting cube (no measure dimension) still answers, and now answers with
/// its one implicit measure rather than a bare list of member names.
#[test]
fn query_on_a_cube_with_no_measure_dimension() {
    let mut catalog = build_catalog();
    let mut p = request("Markets");
    by_region(&mut p);

    let resp = wire::query(&mut catalog, &p).expect("Markets query");
    assert_eq!(resp.columns, vec!["Region", "value"], "the implicit value measure");
    assert_eq!(resp.column_meta[0].kind, "dimension");
    assert_eq!(resp.column_meta[1].kind, "measure", "value is typed as a measure");
    assert_eq!(resp.measure_labels, vec!["SUM(value)"]);
}

/// A measure-less cube queried with NO measure named in the plan gets the implicit
/// `value` measure automatically.
///
/// This is the shape `examples/Plateau/seed.sql` builds for
/// `Sales(Region, Segment, Category)`, and the case that used to return
/// `[["East"],["West"]]` — member names with no numbers, which a client can only
/// read as an empty dataset. `sql.rs` already answered `SELECT Region, value ...`
/// with a `value` column; the plan layer did not, so the two front doors disagreed.
/// Now both produce a numeric `value` measure.
#[test]
fn query_a_measureless_cube_auto_adds_a_value_measure() {
    let mut catalog = build_catalog();
    let mut p = request("Markets"); // Markets(Region) — no measure dimension
    by_region(&mut p); // group by Region, name no measures

    let resp = wire::query(&mut catalog, &p).expect("measure-less query");

    assert_eq!(resp.columns, vec!["Region", "value"], "the implicit measure appears");
    assert_eq!(
        resp.rows,
        vec![
            vec![json!("East"), json!(10.0)],
            vec![json!("West"), json!(20.0)],
        ],
        "one row per region with NUMERIC cells"
    );
    assert_eq!(resp.measure_names, vec!["value"], "so the client knows it is a measure");
    assert_eq!(resp.measure_labels, vec!["SUM(value)"]);
    assert_eq!(resp.column_meta[1].kind, "measure");
    assert_eq!(resp.column_meta[1].data_type, "number");
    assert!(resp.rows[0][1].is_f64(), "a measure cell is a JSON number");

    // And a grand total is the same shape with no groupBy.
    let total = request("Markets");
    let resp = wire::query(&mut catalog, &total).expect("grand total");
    assert_eq!(resp.columns, vec!["value"]);
    assert_eq!(resp.rows, vec![vec![json!(30.0)]]);
}

/// The same cube addressed the other way round: naming `value` explicitly is
/// unchanged, so the two spellings cannot drift apart.
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

/// §3 `truncated` is "true when `limit` cut the result". A result exactly as long
/// as the limit was NOT cut if the data happens to end there — otherwise a pager
/// is told there is another page that does not exist.
///
/// The pre-limit row count is threaded out of `apply_order_and_limit` via
/// `ResultSet::truncated`, because that is the only place that knows it.
#[test]
fn query_truncated_is_true_only_when_rows_were_actually_cut() {
    let mut catalog = build_catalog();

    // Exactly two groups (East, West), limit 2: nothing was cut.
    let mut p = request("Sales");
    by_region(&mut p);
    measure(&mut p, "Sales", "SUM");
    p.limit = Some(2);
    let resp = wire::query(&mut catalog, &p).expect("query");
    assert_eq!(resp.rows.len(), 2, "both groups fit");
    assert!(
        !resp.truncated,
        "the result ended exactly at the limit; nothing was cut"
    );

    // limit 1: one row really was removed.
    let mut p1 = request("Sales");
    by_region(&mut p1);
    measure(&mut p1, "Sales", "SUM");
    p1.limit = Some(1);
    let resp1 = wire::query(&mut catalog, &p1).expect("query");
    assert_eq!(resp1.rows.len(), 1);
    assert!(resp1.truncated, "one row was dropped, so this IS truncated");

    // limit 3: plainly did not bind.
    let mut p3 = request("Sales");
    by_region(&mut p3);
    measure(&mut p3, "Sales", "SUM");
    p3.limit = Some(3);
    let resp3 = wire::query(&mut catalog, &p3).expect("query");
    assert_eq!(resp3.rows.len(), 2);
    assert!(!resp3.truncated);

    // No limit at all: false, regardless of row count.
    let mut p0 = request("Sales");
    by_region(&mut p0);
    measure(&mut p0, "Sales", "SUM");
    let resp0 = wire::query(&mut catalog, &p0).expect("query");
    assert_eq!(resp0.rows.len(), 2);
    assert!(!resp0.truncated);
}

/// §3 lists `SUM | AVG | MEDIAN | MIN | MAX | COUNT | COUNTD | STDEV | VAR | ATTR`
/// and tells the engine to disable the rest via `capabilities.aggregations`.
///
/// The engine is ADDITIVE-ONLY by design (Infor-OLAP storage: a measure is a
/// weighted sum at each leaf, and a consolidation is a weighted sum of leaves).
/// So the contract is:
///
///   * `SUM` (or an absent aggregation, the client default) computes the sum;
///   * anything else is refused with `unsupported_aggregation` BEFORE any work,
///     because returning a sum under an `AVG(Sales)` label is a plausible wrong
///     number the client cannot detect.
///
/// This test used to characterise the opposite behaviour — every aggregation
/// computed SUM silently. Non-additive aggregations are wanted; they need a
/// decision on whether empty cells count, so they are refused until then.
#[test]
fn query_refuses_non_additive_aggregations() {
    let mut catalog = build_catalog();

    // SUM, and the absent-aggregation default, both compute the real total:
    // East 150 + West 225 = 375.
    for agg in [Some("SUM"), Some("sum"), None] {
        let mut p = request("Sales");
        p.measures.push(
            serde_json::from_value(match agg {
                Some(a) => json!({ "name": "Sales", "aggregation": a }),
                None => json!({ "name": "Sales" }),
            })
            .expect("Measure"),
        );
        let resp = wire::query(&mut catalog, &p)
            .unwrap_or_else(|e| panic!("{agg:?}: {e:?}"));
        assert_eq!(resp.rows[0][0], json!(375.0), "{agg:?} is additive: real sum");
    }

    // Everything else is a typed client error, never a silent sum.
    for agg in ["AVG", "MIN", "MAX", "COUNT", "COUNTD", "MEDIAN", "STDEV", "VAR", "ATTR"] {
        let mut p = request("Sales");
        measure(&mut p, "Sales", agg);
        let err = wire::query(&mut catalog, &p)
            .err()
            .unwrap_or_else(|| panic!("{agg} must be refused, not answered"));
        assert_eq!(err.code, "unsupported_aggregation", "{agg}: wrong code");
        assert_eq!(err.status(), 400, "{agg}: a bad request is the client's fault");
        assert!(
            err.message.contains(agg),
            "{agg}: the message should name the aggregation: {}",
            err.message
        );
    }

    // An aggregation the protocol does not define is refused identically.
    let mut p = request("Sales");
    measure(&mut p, "Sales", "NOT_A_REAL_AGGREGATION");
    assert_eq!(
        wire::query(&mut catalog, &p).err().map(|e| e.code),
        Some("unsupported_aggregation")
    );

    // And the advertisement matches: SUM only.
    assert_eq!(wire::capabilities().aggregations, vec!["SUM".to_string()]);
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
