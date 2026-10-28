//! Integration tests for the native plan entry point (`engine::plan`).
//!
//! These exercise the REAL engine over a small in-memory catalogue (the same
//! shape the persistence tests build), proving the plan -> `SliceQuery`
//! translation returns the right answers, that the wire names match
//! ENGINE-PROTOCOL.md, and that every unimplemented feature is a TYPED error
//! carrying the protocol `code` - never a plausible wrong number.
//!
//! The two properties worth stating up front, because they are the point of the
//! module:
//!   1. A well-formed plan produces the correct result (groupBy, measures,
//!      dimension-scope filters, orderBy, limit, domain queries).
//!   2. A plan that asks for something not yet implemented is REFUSED, with a
//!      distinct error `code`, never quietly answered wrong.

use olap_engine::catalog::catalog::Catalog;
use olap_engine::cube::node::CellValue;
use olap_engine::engine::plan::{



    execute_plan, AstNode, Capabilities, Direction, ExprRef, Filter, FilterOp, FilterScope,
    GroupBy, LodSpec, Measure, OrderBy, PlanError, PlanResult, QueryRequest, PROTOCOL_VERSION,
};

/// A trivial non-LOD AST (a field reference), for calculated-field tests.
fn simple_ast() -> AstNode {
    AstNode::Field { name: "Revenue".into() }
}

/// Builds a small cube shaped like the golden fixtures:
///   Sales(Geography, Product, Measure) with a few leaf facts.
fn build_catalog() -> Catalog {
    let mut catalog = Catalog::new();
    catalog.add_cube(
        "Sales",
        &["Geography", "Product", "Measure"],
        Some("Measure"),
        true,
    );
    {
        let cube = catalog.get_cube_mut("Sales").expect("cube exists");
        let w = |cube: &mut olap_engine::cube::cube::Cube, g: &str, p: &str, m: &str, v: f64| {
            cube.write(&[g, p, m], CellValue::Numeric(v)).expect("write");
        };
        w(cube, "France", "Laptop", "Revenue", 100.0);
        w(cube, "France", "Phone", "Revenue", 50.0);
        w(cube, "Germany", "Laptop", "Revenue", 200.0);
        w(cube, "Germany", "Phone", "Revenue", 25.0);
    }
    catalog
}

fn plan(dataset: &str) -> QueryRequest {
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
        join: None,
        denied_members: std::collections::HashMap::new(),
    }
}

fn bare_measure(name: &str) -> Measure {
    Measure {
        name: name.to_string(),
        aggregation: None,
        expr: None,
    }
}

/// Convenience: the headers and rows of a successful plan.
fn rows(result: PlanResult) -> (Vec<String>, Vec<Vec<String>>) {
    match result {
        PlanResult::Rows(rs) => (rs.headers, rs.rows),
        PlanResult::Domains(_) => panic!("expected rows, got a domain answer"),
    }
}

#[test]
fn group_by_one_dimension_with_one_measure() {
    let mut catalog = build_catalog();
    let mut p = plan("Sales");
    p.group_by.push(GroupBy { name: "Geography".into(), date_part: None });
    p.measures.push(bare_measure("Revenue"));

    let (headers, rows) = rows(execute_plan(&mut catalog, &p).expect("plan should succeed"));

    assert_eq!(headers, vec!["Geography", "Revenue"]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec!["France", "150"]);
    assert_eq!(rows[1], vec!["Germany", "225"]);
}

#[test]
fn group_by_two_dimensions() {
    let mut catalog = build_catalog();
    let mut p = plan("Sales");
    p.group_by.push(GroupBy { name: "Geography".into(), date_part: None });
    p.group_by.push(GroupBy { name: "Product".into(), date_part: None });
    p.measures.push(bare_measure("Revenue"));

    let (headers, rows) = rows(execute_plan(&mut catalog, &p).expect("plan should succeed"));

    assert_eq!(headers, vec!["Geography", "Product", "Revenue"]);
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0], vec!["France", "Laptop", "100"]);
    assert_eq!(rows[1], vec!["France", "Phone", "50"]);
    assert_eq!(rows[2], vec!["Germany", "Laptop", "200"]);
    assert_eq!(rows[3], vec!["Germany", "Phone", "25"]);
}

#[test]
fn dimension_filter_prunes_before_grouping() {
    let mut catalog = build_catalog();
    let mut p = plan("Sales");
    p.group_by.push(GroupBy { name: "Product".into(), date_part: None });
    p.measures.push(bare_measure("Revenue"));
    p.filters.push(Filter {
        id: Some("f1".into()),
        field: "Geography".into(),
        scope: FilterScope::Dimension,
        operator: FilterOp::Equals,
        values: vec![],
        value: Some("Germany".into()),
        min: None,
        max: None,
        limit: None,
        include_nulls: false,
    });

    let (_, rows) = rows(execute_plan(&mut catalog, &p).expect("plan should succeed"));

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec!["Laptop", "200"]);
    assert_eq!(rows[1], vec!["Phone", "25"]);
}

#[test]
fn in_filter_keeps_listed_members() {
    let mut catalog = build_catalog();
    let mut p = plan("Sales");
    p.group_by.push(GroupBy { name: "Geography".into(), date_part: None });
    p.measures.push(bare_measure("Revenue"));
    p.filters.push(Filter {
        id: None,
        field: "Geography".into(),
        scope: FilterScope::Dimension,
        operator: FilterOp::In,
        values: vec!["France".into()],
        value: None,
        min: None,
        max: None,
        limit: None,
        include_nulls: false,
    });

    let (_, rows) = rows(execute_plan(&mut catalog, &p).expect("plan should succeed"));
    assert_eq!(rows, vec![vec!["France".to_string(), "150".to_string()]]);
}

/// `notEquals` is the NEGATION of `equals`: the slice's member filter is inclusive, so
/// the plan layer feeds it every member but the excluded one (ENGINE-PROTOCOL.md §3
/// "filters"). This is the operator `capabilities.filterOperators` advertises.
#[test]
fn not_equals_filter_keeps_everything_but_the_listed_member() {
    let mut catalog = build_catalog();
    let mut p = plan("Sales");
    p.group_by.push(GroupBy { name: "Geography".into(), date_part: None });
    p.measures.push(bare_measure("Revenue"));
    p.filters.push(Filter {
        id: None,
        field: "Geography".into(),
        scope: FilterScope::Dimension,
        operator: FilterOp::NotEquals,
        values: vec![],
        value: Some("France".into()),
        min: None,
        max: None,
        limit: None,
        include_nulls: false,
    });

    let (_, rows) = rows(execute_plan(&mut catalog, &p).expect("plan should succeed"));

    assert_eq!(
        rows,
        vec![vec!["Germany".to_string(), "225".to_string()]],
        "everything except France"
    );
}

#[test]
fn order_by_desc_then_limit() {
    let mut catalog = build_catalog();
    let mut p = plan("Sales");
    p.group_by.push(GroupBy { name: "Geography".into(), date_part: None });
    p.measures.push(bare_measure("Revenue"));
    p.order_by = Some(OrderBy { output: "Revenue".into(), direction: Direction::Desc });
    p.limit = Some(1);

    let (_, rows) = rows(execute_plan(&mut catalog, &p).expect("plan should succeed"));

    assert_eq!(rows, vec![vec!["Germany".to_string(), "225".to_string()]]);
}

#[test]
fn domain_query_returns_members_in_display_order() {
    let mut catalog = build_catalog();
    let mut p = plan("Sales");
    p.domain_for.push("Geography".into());

    match execute_plan(&mut catalog, &p).expect("domain plan should succeed") {
        PlanResult::Domains(d) => {
            let geos = d.get("Geography").expect("Geography domain present");
            assert_eq!(geos, &vec!["France".to_string(), "Germany".to_string()]);
        }
        PlanResult::Rows(_) => panic!("expected a domain answer"),
    }
}

// --- Refusals: declared-not-implemented must be TYPED ERRORS with codes. ---

#[test]
fn lod_expression_is_refused_with_unsupported_expression() {
    let mut catalog = build_catalog();
    let mut p = plan("Sales");
    p.group_by.push(GroupBy { name: "Geography".into(), date_part: None });
    p.measures.push(Measure {
        name: "FixedSales".into(),
        aggregation: None,
        expr: Some(ExprRef::Ast(Box::new(simple_ast()))),
    });
    p.expressions.insert(
        "e1".into(),
        AstNode::Lod {
            spec: LodSpec::Fixed,
            dimensions: vec!["Geography".into()],
            expression: Box::new(AstNode::Field { name: "Revenue".into() }),
        },
    );

    // Use the named form so the LOD is reached via the expressions map.
    p.measures[0].expr = Some(ExprRef::Named("e1".into()));

    let err = execute_plan(&mut catalog, &p).expect_err("LOD must be refused");
    assert!(matches!(err, PlanError::UnsupportedExpression(_)), "got {err:?}");
    assert_eq!(err.code(), "unsupported_expression");
    assert!(err.to_string().contains("LOD"), "message should name LOD: {err}");
}

#[test]
fn calculated_field_is_refused_not_computed() {
    let mut catalog = build_catalog();
    let mut p = plan("Sales");
    p.group_by.push(GroupBy { name: "Geography".into(), date_part: None });
    // A simple non-LOD calculated field is still unsupported until the
    // evaluator exists.
    p.measures.push(Measure {
        name: "Margin".into(),
        aggregation: None,
        expr: Some(ExprRef::Ast(Box::new(simple_ast()))),
    });

    let err = execute_plan(&mut catalog, &p).expect_err("calculated field must be refused");
    assert!(matches!(err, PlanError::UnsupportedExpression(_)), "got {err:?}");
    assert_eq!(err.code(), "unsupported_expression");
}

#[test]
fn top_filter_is_refused_with_unsupported_filter() {
    let mut catalog = build_catalog();
    let mut p = plan("Sales");
    p.group_by.push(GroupBy { name: "Geography".into(), date_part: None });
    p.measures.push(bare_measure("Revenue"));
    p.filters.push(Filter {
        id: None,
        field: "Revenue".into(),
        scope: FilterScope::Measure,
        operator: FilterOp::Top,
        values: vec![],
        value: None,
        min: None,
        max: None,
        limit: Some(10),
        include_nulls: false,
    });

    let err = execute_plan(&mut catalog, &p).expect_err("top must be refused");
    assert!(matches!(err, PlanError::UnsupportedFilter(_)), "got {err:?}");
    assert_eq!(err.code(), "unsupported_filter");
    assert!(err.to_string().contains("top"), "message should name top: {err}");
}

#[test]
fn measure_scope_filter_is_refused_for_now() {
    let mut catalog = build_catalog();
    let mut p = plan("Sales");
    p.group_by.push(GroupBy { name: "Geography".into(), date_part: None });
    p.measures.push(bare_measure("Revenue"));
    p.filters.push(Filter {
        id: None,
        field: "Revenue".into(),
        scope: FilterScope::Measure,
        operator: FilterOp::Gt,
        values: vec![],
        value: Some("100".into()),
        min: None,
        max: None,
        limit: None,
        include_nulls: false,
    });

    let err = execute_plan(&mut catalog, &p).expect_err("measure-scope filter must be refused");
    assert!(matches!(err, PlanError::UnsupportedFilter(_)), "got {err:?}");
}

#[test]
fn unsupported_operator_is_refused() {
    let mut catalog = build_catalog();
    let mut p = plan("Sales");
    p.group_by.push(GroupBy { name: "Geography".into(), date_part: None });
    p.measures.push(bare_measure("Revenue"));
    p.filters.push(Filter {
        id: None,
        field: "Product".into(),
        scope: FilterScope::Dimension,
        operator: FilterOp::StartsWith,
        values: vec![],
        value: Some("L".into()),
        min: None,
        max: None,
        limit: None,
        include_nulls: false,
    });

    let err = execute_plan(&mut catalog, &p).expect_err("startsWith must be refused");
    assert!(matches!(err, PlanError::UnsupportedFilter(_)), "got {err:?}");
    assert_eq!(err.code(), "unsupported_filter");
}

#[test]
fn unknown_dataset_is_unknown_dataset_code() {
    let mut catalog = build_catalog();
    let p = plan("NoSuchCube");
    let err = execute_plan(&mut catalog, &p).expect_err("unknown dataset must fail");
    assert!(matches!(err, PlanError::UnknownDataset(_)), "got {err:?}");
    assert_eq!(err.code(), "unknown_dataset");
}

#[test]
fn unknown_group_column_is_invalid_argument() {
    let mut catalog = build_catalog();
    let mut p = plan("Sales");
    p.group_by.push(GroupBy { name: "Nonsense".into(), date_part: None });
    p.measures.push(bare_measure("Revenue"));
    let err = execute_plan(&mut catalog, &p).expect_err("unknown column must fail");
    assert!(matches!(err, PlanError::InvalidArgument(_)), "got {err:?}");
    assert_eq!(err.code(), "invalid_argument");
}

#[test]
fn order_by_unknown_output_is_invalid_argument() {
    let mut catalog = build_catalog();
    let mut p = plan("Sales");
    p.group_by.push(GroupBy { name: "Geography".into(), date_part: None });
    p.measures.push(bare_measure("Revenue"));
    p.order_by = Some(OrderBy { output: "Sum of Sales".into(), direction: Direction::Desc });
    let err = execute_plan(&mut catalog, &p).expect_err("unknown output must fail");
    assert!(matches!(err, PlanError::InvalidArgument(_)), "got {err:?}");
}

#[test]
fn protocol_mismatch_is_version_mismatch_code() {
    let mut catalog = build_catalog();
    let mut p = plan("Sales");
    p.protocol_version = PROTOCOL_VERSION + 1;
    let err = execute_plan(&mut catalog, &p).expect_err("protocol mismatch must fail");
    assert!(matches!(err, PlanError::VersionMismatch { .. }), "got {err:?}");
    assert_eq!(err.code(), "version_mismatch");
}

// --- Capabilities: the honest self-declaration. ---

#[test]
fn capabilities_declare_lod_unsupported_and_no_expression_functions() {
    let caps = Capabilities::default();
    assert_eq!(caps.protocol_version, PROTOCOL_VERSION);
    // The load-bearing honesty bits: Plateau degrades on these.
    assert!(!caps.supports_lod, "LOD must NOT be claimed until implemented");
    // A calculated-field evaluator exists now, but it implements exactly two
    // non-aggregate functions. The list must be the implementation, not an aspiration.
    assert_eq!(
        caps.expression_functions,
        vec!["ZN".to_string(), "IFNULL".to_string()],
        "advertise exactly the functions that run"
    );
    // Only what is actually honoured today.
    assert!(caps.filter_operators.contains(&"equals".to_string()));
    assert!(caps.filter_operators.contains(&"in".to_string()));
    assert!(caps.aggregations.contains(&"SUM".to_string()));
    assert!(caps.supports_inline_domain);
}
