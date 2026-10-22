//! System cubes: read-only, automatically maintained views of the database itself.
//!
//! WHAT THEY ARE FOR
//!   A front end needs to *list* what exists — cubes, dimensions, hierarchies, members —
//!   without MDX and without a second metadata API that can drift from the catalogue.
//!   These are ordinary cubes, so a client lists the database by sending a normal query.
//!   The security design (`SECURITY-DESIGN.md` §0) also hangs its `Role`/`Dimension`/
//!   `Member` dictionaries on the same mechanism.
//!
//! NAMING
//!   `#` prefix — the Infor convention (the user's choice; TM1 would use `}`). Any name
//!   beginning with `#` is system-owned: read-only to users, and excluded from the
//!   listings below.
//!
//! WHY THEY ARE STORED RATHER THAN COMPUTED
//!   Stored cubes inherit grouping, aggregation and joins for free. A virtual
//!   implementation would have to reimplement all three for metadata, and could not be
//!   joined to anything.
//!
//! WHY THEY EXCLUDE THEMSELVES
//!   `#Members` lists members; the members of `#Members` include the names it is writing.
//!   Listing itself is self-referential (the list would have to contain its own contents),
//!   so system cubes describe USER content only. Their dictionary dimensions (`#Cube`,
//!   `#Member`, …) are system-owned too and are skipped for the same reason.
//!
//! WHEN THEY REBUILD
//!   On load, and after every command in the CLI (`main.rs`). The HTTP server never
//!   writes, so its system cubes cannot go stale between load and shutdown.

use crate::catalog::catalog::Catalog;
use crate::cube::node::CellValue;

pub const CUBES: &str = "#Cubes";
pub const DIMENSIONS: &str = "#Dimensions";
pub const HIERARCHIES: &str = "#Hierarchies";
pub const MEMBERS: &str = "#Members";
pub const EDGES: &str = "#Edges";

/// True when a name is system-owned (`#` prefix).
pub fn is_system(name: &str) -> bool {
    name.starts_with('#')
}

/// The system cubes this module maintains.
pub fn names() -> [&'static str; 5] {
    [CUBES, DIMENSIONS, HIERARCHIES, MEMBERS, EDGES]
}

/// One hierarchy's members and parent/child edges, captured before the catalogue is touched.
struct HierarchyFacts {
    name: String,
    members: Vec<String>,
    /// (child, parent, weight)
    edges: Vec<(String, String, f64)>,
}

struct DimensionFacts {
    name: String,
    hierarchies: Vec<HierarchyFacts>,
}

/// Rebuilds every system cube from the current catalogue state.
///
/// Cheap enough to call after each command: it copies names, not data.
pub fn rebuild(catalog: &mut Catalog) {
    // --- 1. Snapshot USER content before touching the system cubes. ---
    let mut cubes: Vec<(String, Option<String>, bool)> = Vec::new();
    let mut dimensions: Vec<DimensionFacts> = Vec::new();

    for cube in catalog.cubes.values() {
        if is_system(&cube.name) {
            continue;
        }
        cubes.push((
            cube.name.clone(),
            cube.measure_dimension.clone(),
            cube.is_aggregating,
        ));
    }

    for (name, dimension) in catalog.dimensions.iter() {
        if is_system(name) {
            continue;
        }
        let mut dim = dimension.write().unwrap_or_else(|e| e.into_inner());
        // `registered_ids()` lists only members the hierarchy has a NAME for; a
        // consolidated parent can live in the consolidation map alone, so iterating it
        // skipped every parent and produced an empty edge table. `display_order()` covers
        // the whole tree — parents included — once it has been built.
        dim.ensure_display_order();
        let mut facts = DimensionFacts {
            name: name.clone(),
            hierarchies: Vec::new(),
        };
        // EVERY hierarchy, not just the default: a multi-hierarchy dimension keeps
        // consolidated members (and their children) in the others, and listing only the
        // default made them invisible to SQL.
        for hierarchy_name in dim.hierarchy_names() {
            let Some(hierarchy) = dim.hierarchy(&hierarchy_name) else {
                continue;
            };
            let mut facts_for_hierarchy = HierarchyFacts {
                name: hierarchy_name.clone(),
                members: Vec::new(),
                edges: Vec::new(),
            };
            for id in hierarchy.display_order().to_vec() {
                // The DIMENSION owns the canonical id -> name map. A hierarchy keeps its own
                // keys, which come back lowercased for members it did not itself create,
                // making a listing read "bookcases" for "Bookcases".
                let member = dim.get_name(id);
                facts_for_hierarchy.members.push(member.clone());
                for (child, weight) in hierarchy.children_of(id) {
                    facts_for_hierarchy.edges.push((
                        dim.get_name(*child),
                        member.clone(),
                        *weight,
                    ));
                }
            }
            facts.hierarchies.push(facts_for_hierarchy);
        }
        dimensions.push(facts);
    }

    // --- 2. (Re)create them. `add_cube` overwrites, which is the rebuild semantic. ---
    catalog.add_cube(CUBES, &["#Cube", "#Property"], None, true);
    catalog.add_cube(DIMENSIONS, &["#Dimension", "#Property"], None, true);
    catalog.add_cube(HIERARCHIES, &["#Dimension", "#Hierarchy"], None, true);
    catalog.add_cube(MEMBERS, &["#Dimension", "#Hierarchy", "#Member"], None, true);
    catalog.add_cube(
        EDGES,
        &["#Dimension", "#Hierarchy", "#Member", "#Parent"],
        None,
        true,
    );

    // --- 3. Populate. ---
    for (name, measure_dimension, is_aggregating) in &cubes {
        put(catalog, CUBES, &[name, "measureDimension"], measure_dimension.as_deref().unwrap_or(""));
        put(
            catalog,
            CUBES,
            &[name, "isAttributeTable"],
            if *is_aggregating { "false" } else { "true" },
        );
    }
    for dimension in &dimensions {
        let count = dimension.hierarchies.len().to_string();
        put(catalog, DIMENSIONS, &[&dimension.name, "hierarchyCount"], &count);
    }
    for dimension in &dimensions {
        for hierarchy in &dimension.hierarchies {
            put(catalog, HIERARCHIES, &[&dimension.name, &hierarchy.name], "1");
            for member in &hierarchy.members {
                put(catalog, MEMBERS, &[&dimension.name, &hierarchy.name, member], "1");
            }
            // A ROOT is simply a member with no edge row; the client folds the edges into a
            // tree and takes the members it never saw as a child. That is why the weight is
            // the cell: the same edge carries the rollup share.
            for (child, parent, weight) in &hierarchy.edges {
                put_number(
                    catalog,
                    EDGES,
                    &[&dimension.name, &hierarchy.name, child, parent],
                    *weight,
                );
            }
        }
    }
}

/// Writes one system-cube cell straight into the store.
///
/// Deliberately NOT `Cube::write`: that refuses writes to system cubes, because they are
/// read-only to users. This is the one place allowed to populate them.
fn put(catalog: &mut Catalog, cube_name: &str, coords: &[&str], value: &str) {
    let Some(cube) = catalog.get_cube_mut(cube_name) else {
        return;
    };
    let ids: Vec<u32> = coords
        .iter()
        .enumerate()
        .map(|(i, member)| {
            cube.dimensions[i]
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .add_leaf(member)
        })
        .collect();
    cube.store.write(&ids, CellValue::String(value.to_string()));
}

#[cfg(test)]
mod system_cube_tests {
    //! S0: `#` cubes are derived views of the catalogue, read-only, and refreshed.

    use super::*;
    use crate::catalog::catalog::Catalog;
    use crate::cube::node::CellValue;

    fn catalog_with_sales() -> Catalog {
        let mut catalog = Catalog::new();
        catalog.add_cube("Sales", &["Region", "Measure"], Some("Measure"), true);
        {
            let cube = catalog.get_cube_mut("Sales").expect("Sales");
            cube.write(&["East", "Revenue"], CellValue::Numeric(10.0))
                .expect("write");
        }
        rebuild(&mut catalog);
        catalog
    }

    /// Reads every cell of a system cube as (dimension values..., string value).
    fn rows(catalog: &Catalog, cube_name: &str) -> Vec<Vec<String>> {
        let cube = catalog.get_cube(cube_name).expect("system cube");
        let mut result = cube
            .store
            .scan_subcube(&vec![None; cube.dimensions.len()])
            .into_iter()
            .map(|(coords, value)| {
                let mut row: Vec<String> = coords
                    .iter()
                    .enumerate()
                    .map(|(i, id)| cube.dimensions[i].read().unwrap().get_name(*id))
                    .collect();
                row.push(value.to_string());
                row
            })
            .collect::<Vec<_>>();
        result.sort();
        result
    }

    #[test]
    fn the_system_cubes_describe_the_catalogue() {
        let catalog = catalog_with_sales();

        let cubes = rows(&catalog, CUBES);
        assert!(
            cubes.iter().any(|r| r[0] == "Sales" && r[1] == "measureDimension" && r[2] == "Measure"),
            "Sales and its measure dimension: {cubes:?}"
        );
        assert!(
            cubes.iter().any(|r| r[0] == "Sales" && r[1] == "isAttributeTable" && r[2] == "false"),
            "Sales is a measure cube: {cubes:?}"
        );

        let dimensions = rows(&catalog, DIMENSIONS);
        assert!(dimensions.iter().any(|r| r[0] == "Region"), "{dimensions:?}");

        let hierarchies = rows(&catalog, HIERARCHIES);
        assert!(
            hierarchies.iter().any(|r| r[0] == "Region" && r[1] == "Region"),
            "every dimension owns a default hierarchy: {hierarchies:?}"
        );

        // #Members is (dimension, hierarchy, member) since members are listed per
        // hierarchy: consolidated members exist only in the hierarchies that define them.
        let members = rows(&catalog, MEMBERS);
        assert!(
            members.iter().any(|r| r[0] == "Region" && r[2] == "East"),
            "{members:?}"
        );
        assert!(
            members.iter().any(|r| r[0] == "Measure" && r[2] == "Revenue"),
            "{members:?}"
        );
    }

    /// A system cube must NOT list itself: the member list would have to contain the
    /// names it is in the middle of writing.
    #[test]
    fn system_cubes_do_not_list_themselves() {
        let catalog = catalog_with_sales();
        let cubes = rows(&catalog, CUBES);
        assert!(
            !cubes.iter().any(|r| r[0].starts_with('#')),
            "no # cube may appear in #Cubes: {cubes:?}"
        );
        let dimensions = rows(&catalog, DIMENSIONS);
        assert!(
            !dimensions.iter().any(|r| r[0].starts_with('#')),
            "no # dictionary dimension may appear in #Dimensions: {dimensions:?}"
        );
    }

    #[test]
    fn a_rebuild_picks_up_new_content() {
        let mut catalog = catalog_with_sales();
        assert!(!rows(&catalog, CUBES).iter().any(|r| r[0] == "Later"));

        catalog.add_cube("Later", &["Region"], None, true);
        rebuild(&mut catalog);
        assert!(
            rows(&catalog, CUBES).iter().any(|r| r[0] == "Later"),
            "a cube created after the last rebuild must appear"
        );
    }

    #[test]
    fn writes_to_a_system_cube_are_refused() {
        let catalog = catalog_with_sales();

        let err = catalog
            .get_cube(CUBES)
            .expect("cube")
            .write(&["Bogus", "x"], CellValue::String("y".to_string()))
            .expect_err("system cubes are read-only");
        assert!(err.contains("read-only"), "{err}");

        // The splashing path is refused too, or it would be a way around the guard.
        let err = catalog
            .get_cube(CUBES)
            .expect("cube")
            .write_splashed(&["Bogus", "x"], 1.0, crate::cube::cube::SplashMode::Replace)
            .expect_err("system cubes are read-only");
        assert!(err.contains("read-only"), "{err}");
    }
}
/// Writes a NUMERIC system-cube cell (the edge weight) straight into the store.
fn put_number(catalog: &mut Catalog, cube_name: &str, coords: &[&str], value: f64) {
    let Some(cube) = catalog.get_cube_mut(cube_name) else {
        return;
    };
    let ids: Vec<u32> = coords
        .iter()
        .enumerate()
        .map(|(i, member)| {
            cube.dimensions[i]
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .add_leaf(member)
        })
        .collect();
    cube.store.write(&ids, CellValue::Numeric(value));
}

#[cfg(test)]
mod edge_tests {
    //! `#Edges` and per-hierarchy `#Members`, which the CLI probe suggested were empty.

    use super::*;
    use crate::catalog::catalog::Catalog;

    /// (dimension values..., value) for every cell, sorted for determinism.
    fn rows(catalog: &Catalog, cube_name: &str) -> Vec<Vec<String>> {
        let cube = catalog.get_cube(cube_name).expect("system cube");
        let mut result: Vec<Vec<String>> = cube
            .store
            .scan_subcube(&vec![None; cube.dimensions.len()])
            .into_iter()
            .map(|(coords, value)| {
                let mut row: Vec<String> = coords
                    .iter()
                    .enumerate()
                    .map(|(i, id)| cube.dimensions[i].read().unwrap().get_name(*id))
                    .collect();
                row.push(value.to_string());
                result_push_value(&mut row, value);
                row
            })
            .collect();
        result.sort();
        result
    }

    fn result_push_value(row: &mut Vec<String>, value: crate::cube::node::CellValue) {
        // `CellValue`'s Display is what the scan gives us; keep it as the last column.
        let _ = (row, value);
    }

    #[test]
    fn edges_capture_the_hierarchy_structure() {
        let mut catalog = Catalog::new();
        catalog.add_cube("Sales", &["Region", "Measure"], Some("Measure"), true);
        catalog.create_hierarchy("Region", "Rollup").expect("hierarchy");
        {
            let dim = catalog.get_dimension("Region").expect("Region");
            let mut d = dim.write().expect("lock");
            d.add_component_in(Some("Rollup"), "All", "East", 1.0)
                .expect("East under All");
            d.add_component_in(Some("Rollup"), "All", "West", 0.5)
                .expect("West under All");
        }
        rebuild(&mut catalog);

        let edges = rows(&catalog, EDGES);
        assert!(
            edges.iter().any(|r| r[2].eq_ignore_ascii_case("East") && r[3] == "All"),
            "East -> All must be an edge: {edges:?}"
        );
        assert!(
            edges.iter().any(|r| r[2].eq_ignore_ascii_case("West") && r[3] == "All" && r[4] == "0.5"),
            "the weight travels as the cell: {edges:?}"
        );

        let members = rows(&catalog, MEMBERS);
        assert!(
            members.iter().any(|r| r[1] == "Rollup" && r[2] == "All"),
            "the consolidated parent must be listed under Rollup: {members:?}"
        );
    }
}