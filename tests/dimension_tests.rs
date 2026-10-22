//! `Dimension::get_default_member_name` — the member a query falls back to.
//!
//! A dimension can own several hierarchies, and its "first added" member may live in one
//! of the others. The default member is injected as a FILTER when a query leaves the
//! dimension unnamed, and a filter is resolved in the DEFAULT hierarchy — so such a name
//! produced `Member 'Furniture' not found`. That was observed against the seeded
//! catalogue, where `Product` has both a default hierarchy and `Product_Class`.

use olap_engine::dimension::dimension::Dimension;

/// A member that exists only inside a non-default hierarchy must not become the default,
/// even when the dimension's default pointer names it.
///
/// The seeded catalogue had exactly this state on `Product`, which produced
/// `Member 'Furniture' not found` when the name was injected as a filter. The state is
/// constructed directly because it depends on the order members were created in, not on
/// anything a test would normally do.
#[test]
fn the_default_member_must_live_in_the_default_hierarchy() {
    let mut dim = Dimension::new("Product");
    dim.create_hierarchy("Product_Class").expect("create the hierarchy");
    dim.add_component_in(Some("Product_Class"), "Furniture", "Widget", 1.0)
        .expect("link a class");

    // The poisoned state: the default points at a member that only lives in Product_Class.
    let furniture = dim
        .get_id_in(Some("Product_Class"), "Furniture")
        .expect("the class parent exists");
    dim.default_member_id = Some(furniture);
    assert!(
        dim.get_id_in(None, "Furniture").is_none(),
        "precondition: Furniture is not addressable in the default hierarchy"
    );

    let default = dim
        .get_default_member_name()
        .expect("there is a member in the default hierarchy to fall back to");
    assert_ne!(
        default, "Furniture",
        "a member from another hierarchy must not be injected as a filter"
    );
    assert!(
        dim.get_id_in(None, &default).is_some(),
        "whatever is returned must resolve in the DEFAULT hierarchy: {default}"
    );
}

/// The ordinary case is unchanged: the first leaf is the default.
#[test]
fn a_single_hierarchy_dimension_still_defaults_to_its_first_member() {
    let mut dim = Dimension::new("Region");
    dim.add_leaf("East");
    dim.add_leaf("West");

    assert_eq!(dim.get_default_member_name().as_deref(), Some("East"));
}

/// A dimension with no members has no default — the caller must skip injection rather than
/// invent one.
#[test]
fn an_empty_dimension_has_no_default_member() {
    let dim = Dimension::new("Nothing");
    assert_eq!(dim.get_default_member_name(), None);
}
