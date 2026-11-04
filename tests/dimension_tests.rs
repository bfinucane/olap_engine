//! Default-member semantics.
//!
//! There is NO implicit default member. The first member a dimension receives is not special;
//! a query that omits the dimension aggregates across all its base members. A default exists
//! only when the user sets one explicitly, and deleting it returns the dimension to "no default".

use olap_engine::dimension::dimension::Dimension;

#[test]
fn the_first_member_is_not_a_default() {
    let mut dim = Dimension::new("Region");
    dim.add_leaf("East");
    dim.add_leaf("West");
    assert_eq!(
        dim.get_default_member_name(),
        None,
        "no implicit default: omitted dimension aggregates all base members"
    );
}

#[test]
fn an_explicit_default_is_used() {
    let mut dim = Dimension::new("Region");
    dim.add_leaf("East");
    dim.add_leaf("West");
    dim.set_default_member("West").expect("set the default");
    assert_eq!(dim.get_default_member_name().as_deref(), Some("West"));
}

#[test]
fn clearing_the_default_removes_it() {
    let mut dim = Dimension::new("Region");
    dim.add_leaf("East");
    dim.set_default_member("East").expect("set the default");
    dim.clear_default_member();
    assert_eq!(dim.get_default_member_name(), None);
}

#[test]
fn deleting_the_explicit_default_leaves_no_default() {
    let mut dim = Dimension::new("Region");
    dim.add_leaf("East");
    dim.add_leaf("West");
    dim.set_default_member("East").expect("set the default");
    dim.delete_member("East").expect("delete the member");
    assert_eq!(
        dim.get_default_member_name(),
        None,
        "deleting the default must not promote another member"
    );
}

#[test]
fn a_legacy_unflagged_default_is_ignored() {
    let mut dim = Dimension::new("Region");
    let id = dim.add_leaf("East");
    // What an old file looked like: a bare id for the auto-assigned first member.
    dim.default_member_id = Some(id);
    assert_eq!(
        dim.get_default_member_name(),
        None,
        "legacy auto-defaults are not honoured"
    );
}

#[test]
fn an_explicit_default_must_resolve_in_the_default_hierarchy() {
    let mut dim = Dimension::new("Product");
    dim.create_hierarchy("Product_Class").expect("create the hierarchy");
    dim.add_component_in(Some("Product_Class"), "Furniture", "Widget", 1.0)
        .expect("link a class");
    // Setting a member that only lives in another hierarchy is refused: it could never be
    // injected as a default-hierarchy filter.
    assert!(dim.get_id_in(None, "Furniture").is_none(), "precondition");
    let error = dim
        .set_default_member("Furniture")
        .expect_err("a non-addressable member must be refused");
    assert!(error.contains("default hierarchy"), "{error}");
    assert_eq!(dim.get_default_member_name(), None);
}