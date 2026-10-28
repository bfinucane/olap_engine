//! Aggregation semantics, tested directly against `query_slice` — below the SQL and
//! wire layers, so a failure points at the accumulator rather than at parsing.

use std::collections::HashMap;

use olap_engine::catalog::catalog::Catalog;
use olap_engine::cube::aggregation::Aggregation;
use olap_engine::cube::cube::SliceQuery;
use olap_engine::cube::node::CellValue;

/// Two regions x two products, so a group by Region has two cells to fold:
///   East: 100 + 50 = 150, West: 200 + 25 = 225
fn four_cells() -> Catalog {
    let mut catalog = Catalog::new();
    catalog.add_cube("T", &["Region", "Product", "M"], Some("M"), true);
    {
        let cube = catalog.get_cube_mut("T").expect("T");
        for (r, p, v) in [
            ("East", "A", 100.0),
            ("East", "B", 50.0),
            ("West", "A", 200.0),
            ("West", "B", 25.0),
        ] {
            cube.write(&[r, p, "Sales"], CellValue::Numeric(v)).expect("write");
        }
    }
    catalog
}

fn slice(aggregation: Option<&str>) -> SliceQuery {
    let mut aggregations = HashMap::new();
    if let Some(name) = aggregation {
        aggregations.insert(
            "Sales".to_string(),
            olap_engine::cube::aggregation::Aggregation::parse(name).expect("known aggregation"),
        );
    }
    SliceQuery {
        output_columns: vec!["Region".to_string(), "Sales".to_string()],
        axes: vec!["Region".to_string()],
        requested_measures: vec!["Sales".to_string()],
        aggregations,
        ..Default::default()
    }
}

#[test]
fn sum_over_a_two_cell_group_is_the_total() {
    let catalog = four_cells();
    let cube = catalog.get_cube("T").expect("T");
    let rs = cube.query_slice(&slice(None)).expect("slice");
    assert_eq!(
        rs.rows,
        vec![
            vec!["East".to_string(), "150".to_string()],
            vec!["West".to_string(), "225".to_string()],
        ],
        "SUM must add both cells of the group"
    );
}

#[test]
fn the_non_additive_aggregations_fold_the_two_cells() {
    let catalog = four_cells();
    let cube = catalog.get_cube("T").expect("T");

    for (agg, east, west) in [
        ("SUM", "150", "225"),
        ("AVG", "75", "112.5"),
        ("COUNT", "2", "2"),
        ("MIN", "50", "25"),
        ("MAX", "100", "200"),
        // Both groups hold two DISTINCT values (100/50 and 200/25).
        ("COUNTD", "2", "2"),
    ] {
        let rs = cube.query_slice(&slice(Some(agg))).expect("slice");
        assert_eq!(
            rs.rows[0],
            vec!["East".to_string(), east.to_string()],
            "{agg} for East"
        );
        assert_eq!(
            rs.rows[1],
            vec!["West".to_string(), west.to_string()],
            "{agg} for West"
        );
    }
}

#[test]
fn a_group_with_no_cell_for_the_measure_is_the_absent_marker() {
    let mut catalog = Catalog::new();
    catalog.add_cube("T", &["Region", "M"], Some("M"), true);
    {
        let cube = catalog.get_cube_mut("T").expect("T");
        cube.write(&["East", "Sales"], CellValue::Numeric(100.0)).expect("write");
        cube.write(&["West", "Cost"], CellValue::Numeric(80.0)).expect("write");
    }
    let cube = catalog.get_cube("T").expect("T");

    let mut q = SliceQuery {
        output_columns: vec!["Region".to_string(), "Cost".to_string()],
        axes: vec!["Region".to_string()],
        requested_measures: vec!["Cost".to_string()],
        ..Default::default()
    };
    // "Cost" is requested but the cube's measure dimension members are Sales/Cost.
    let rs = cube.query_slice(&q).expect("slice");
    assert_eq!(
        rs.rows[0],
        vec!["East".to_string(), "-".to_string()],
        "East has no Cost cell"
    );

    q.aggregations.insert(
        "Cost".to_string(),
        olap_engine::cube::aggregation::Aggregation::Avg,
    );
    let rs_avg = cube.query_slice(&q).expect("avg slice");
    assert_eq!(
        rs_avg.rows[0],
        vec!["East".to_string(), "-".to_string()],
        "there is no average of zero cells"
    );

    q.aggregations.insert(
        "Cost".to_string(),
        olap_engine::cube::aggregation::Aggregation::CountDistinct,
    );
    let rs_countd = cube.query_slice(&q).expect("countd slice");
    assert_eq!(
        rs_countd.rows[0],
        vec!["East".to_string(), "-".to_string()],
        "there is no distinct count of zero cells either - absent, never 0"
    );
}

// ---------------------------------------------------------------------------
// COUNTD - exact distinct count of BASE leaf values
// ---------------------------------------------------------------------------

/// A slice grouped by `Region`, folding `measure` with `aggregation`.
fn region_slice(measure: &str, aggregation: &str) -> SliceQuery {
    let mut aggregations = HashMap::new();
    aggregations.insert(
        measure.to_string(),
        Aggregation::parse(aggregation).expect("aggregation"),
    );
    SliceQuery {
        output_columns: vec!["Region".to_string(), measure.to_string()],
        axes: vec!["Region".to_string()],
        requested_measures: vec![measure.to_string()],
        aggregations,
        ..Default::default()
    }
}

/// A grand-total slice (no axes), folding `measure` with `aggregation`.
fn total_slice(measure: &str, aggregation: &str) -> SliceQuery {
    let mut aggregations = HashMap::new();
    aggregations.insert(
        measure.to_string(),
        Aggregation::parse(aggregation).expect("aggregation"),
    );
    SliceQuery {
        output_columns: vec![measure.to_string()],
        requested_measures: vec![measure.to_string()],
        aggregations,
        ..Default::default()
    }
}

/// Two cells in one group holding the SAME value: COUNT counts cells, COUNTD counts
/// values. This is the difference that makes COUNTD worth having.
#[test]
fn countd_counts_distinct_values_not_cells() {
    let mut catalog = Catalog::new();
    catalog.add_cube("D", &["Region", "Product", "M"], Some("M"), true);
    {
        let cube = catalog.get_cube_mut("D").expect("D");
        for (r, p, v) in [
            ("East", "A", 100.0),
            ("East", "B", 100.0), // same VALUE, different cell
            ("West", "A", 200.0),
            ("West", "B", 25.0),
        ] {
            cube.write(&[r, p, "Sales"], CellValue::Numeric(v))
                .expect("write");
        }
    }
    let cube = catalog.get_cube("D").expect("D");

    let countd = cube
        .query_slice(&region_slice("Sales", "COUNTD"))
        .expect("countd slice");
    assert_eq!(
        countd.rows,
        vec![
            vec!["East".to_string(), "1".to_string()],
            vec!["West".to_string(), "2".to_string()],
        ],
        "COUNTD counts distinct base values, not cells"
    );

    let count = cube
        .query_slice(&region_slice("Sales", "COUNT"))
        .expect("count slice");
    assert_eq!(
        count.rows[0],
        vec!["East".to_string(), "2".to_string()],
        "COUNT still counts CELLS: the two East cells are both non-empty"
    );
}

/// Weights are ignored, exactly like AVG/MIN/MAX: a weight says how much a leaf
/// contributes to a rollup, not what value it holds.
#[test]
fn countd_ignores_rollup_weights() {
    let mut catalog = Catalog::new();
    catalog.add_cube("W", &["Geography", "M"], Some("M"), true);
    {
        let cube = catalog.get_cube_mut("W").expect("W");
        cube.write(&["England", "Sales"], CellValue::Numeric(100.0))
            .expect("write England");
        cube.write(&["France", "Sales"], CellValue::Numeric(100.0))
            .expect("write France");
    }
    catalog
        .create_hierarchy("Geography", "Weighted")
        .expect("create Weighted");
    {
        let dim = catalog.get_dimension("Geography").expect("dimension");
        let mut d = dim.write().unwrap();
        // A fractional weight: England contributes HALF to Half's rollup.
        d.add_component_in(Some("Weighted"), "Half", "England", 0.5)
            .expect("link England under Half");
    }
    let cube = catalog.get_cube("W").expect("W");

    let mut filtered = total_slice("Sales", "SUM");
    filtered.filters.insert(
        "Geography".to_string(),
        vec!["Weighted:Half".to_string()],
    );
    let sum = cube.query_slice(&filtered).expect("weighted sum");
    assert_eq!(sum.rows, vec![vec!["50".to_string()]], "SUM applies the 0.5 weight");

    let mut filtered = total_slice("Sales", "COUNTD");
    filtered.filters.insert(
        "Geography".to_string(),
        vec!["Weighted:Half".to_string()],
    );
    let countd = cube.query_slice(&filtered).expect("weighted countd");
    assert_eq!(
        countd.rows,
        vec![vec!["1".to_string()]],
        "COUNTD folds the BASE value (100), so the weight changes nothing"
    );
}

/// A partial group: Europe holds England (has a Cost cell) and France (has none).
/// The absent cell must be skipped, not counted as a value.
#[test]
fn countd_skips_absent_cells_within_a_partial_group() {
    let mut catalog = Catalog::new();
    catalog.add_cube("P", &["Geography", "M"], Some("M"), true);
    {
        let cube = catalog.get_cube_mut("P").expect("P");
        cube.write(&["England", "Sales"], CellValue::Numeric(100.0))
            .expect("write");
        cube.write(&["England", "Cost"], CellValue::Numeric(40.0))
            .expect("write");
        cube.write(&["France", "Sales"], CellValue::Numeric(50.0))
            .expect("write"); // France has NO Cost cell
    }
    {
        let dim = catalog.get_dimension("Geography").expect("dimension");
        let mut d = dim.write().unwrap();
        d.add_component_in(None, "Europe", "England", 1.0)
            .expect("link England");
        d.add_component_in(None, "Europe", "France", 1.0)
            .expect("link France");
    }
    let cube = catalog.get_cube("P").expect("P");

    let mut q = total_slice("Cost", "COUNTD");
    q.filters
        .insert("Geography".to_string(), vec!["Europe".to_string()]);
    let countd = cube.query_slice(&q).expect("countd over Europe");
    assert_eq!(
        countd.rows,
        vec![vec!["1".to_string()]],
        "one non-empty Cost cell (40), so one distinct value - France's absence is skipped"
    );
}

/// Numeric equality is BY VALUE: `0.0 == -0.0`, and every NaN is the same value
/// (otherwise a NaN cell would count once per cell).
#[test]
fn countd_numeric_equality_folds_zero_and_nan() {
    let mut catalog = Catalog::new();
    catalog.add_cube("N", &["Region", "Product", "M"], Some("M"), true);
    {
        let cube = catalog.get_cube_mut("N").expect("N");
        cube.write(&["East", "A", "Sales"], CellValue::Numeric(0.0))
            .expect("write");
        cube.write(&["East", "B", "Sales"], CellValue::Numeric(-0.0))
            .expect("write");
        cube.write(&["West", "A", "Sales"], CellValue::Numeric(f64::NAN))
            .expect("write");
        cube.write(&["West", "B", "Sales"], CellValue::Numeric(f64::NAN))
            .expect("write");
    }
    let cube = catalog.get_cube("N").expect("N");

    let rs = cube
        .query_slice(&region_slice("Sales", "COUNTD"))
        .expect("countd slice");
    assert_eq!(
        rs.rows,
        vec![
            vec!["East".to_string(), "1".to_string()],
            vec!["West".to_string(), "1".to_string()],
        ],
        "0.0 and -0.0 are one value; two NaNs are one value"
    );
}

/// Attribute cubes hold TEXT. Distinct text compares EXACTLY and case-sensitively.
#[test]
fn countd_text_values_are_exact_and_case_sensitive() {
    let mut catalog = Catalog::new();
    catalog.add_cube(
        "Attr",
        &["Region", "Attribute"],
        Some("Attribute"),
        false, // an attribute table: cells are strings
    );
    {
        let cube = catalog.get_cube_mut("Attr").expect("Attr");
        for (r, v) in [
            ("East", "Ava"),
            ("West", "Ava"),   // duplicate value across cells -> one distinct value
            ("Central", "Dan"), // and "dan" is a DIFFERENT value: case-sensitive
            ("South", "dan"),
        ] {
            cube.write(&[r, "Manager"], CellValue::String(v.to_string()))
                .expect("write");
        }
    }
    let cube = catalog.get_cube("Attr").expect("Attr");

    let countd = cube
        .query_slice(&total_slice("Manager", "COUNTD"))
        .expect("countd slice");
    assert_eq!(
        countd.rows,
        vec![vec!["3".to_string()]],
        "'Ava' twice, 'Dan' and 'dan' -> three distinct values"
    );

    // NOTE (pre-existing, out of COUNTD's scope): COUNT over TEXT cells is not a cell
    // count in this engine - a text cell replaces the numeric accumulator, so COUNT
    // returns a string. The numeric COUNT-vs-COUNTD contrast is pinned in
    // `countd_counts_distinct_values_not_cells`; COUNTD above is the text aggregation.
}
