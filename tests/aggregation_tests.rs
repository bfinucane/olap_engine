//! Aggregation semantics, tested directly against `query_slice` — below the SQL and
//! wire layers, so a failure points at the accumulator rather than at parsing.

use std::collections::HashMap;

use olap_engine::catalog::catalog::Catalog;
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
}
