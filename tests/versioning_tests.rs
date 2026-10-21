//! Phase 1b M2: versioning and version-tagged caching.
//!
//! These tests pin the behaviour that replaced `self.query_cache.clear()` on every
//! write: a write publishes a new version, and the cache is invalidated lazily by the
//! next read rather than eagerly by the writer.

use std::sync::{Arc, RwLock};

use olap_engine::catalog::catalog::Catalog;
use olap_engine::cube::cube::Cube;
use olap_engine::cube::node::CellValue;
use olap_engine::dimension::dimension::Dimension;

/// A one-dimension cube. `Region` has no hierarchy beyond its default, which is all
/// these tests need: a leaf aggregate is enough to exercise the cache.
fn cube() -> Cube {
    let dim = Arc::new(RwLock::new(Dimension::new("Region")));
    Cube::new("Sales", vec!["Region".to_string()], vec![dim], None, true)
}

#[test]
fn a_write_publishes_a_new_version() {
    let cube = cube();
    let before = cube.store.version();

    cube.write(&["East"], CellValue::Numeric(100.0)).expect("write");
    let after_first = cube.store.version();
    assert_eq!(after_first, before + 1, "each write advances the version");

    cube.write(&["West"], CellValue::Numeric(200.0)).expect("write");
    assert_eq!(cube.store.version(), after_first + 1);
}

#[test]
fn a_snapshot_read_is_unaffected_by_a_later_write() {
    let cube = cube();
    cube.write(&["East"], CellValue::Numeric(100.0)).expect("write");

    let id = cube.dimensions[0]
        .read()
        .expect("dim")
        .get_id("East")
        .expect("East exists");

    let snapshot = cube.store.snapshot();
    cube.write(&["East"], CellValue::Numeric(999.0)).expect("overwrite");

    assert_eq!(
        snapshot.query_exact(&[id]),
        Some(CellValue::Numeric(100.0)),
        "the snapshot keeps the value it was taken with"
    );
    assert_eq!(
        cube.store.query_exact(&[id]),
        Some(CellValue::Numeric(999.0)),
        "the live store sees the write"
    );
}

#[test]
fn the_cache_is_reused_within_a_version_and_dropped_after_a_write() {
    let cube = cube();
    cube.write(&["East"], CellValue::Numeric(100.0)).expect("write");

    assert_eq!(cube.query_consolidated(&["East"]), 100.0);
    assert_eq!(cube.store.cached_len(), 1, "the first read populates the cache");

    // A second read in the same version is served from the cache and does not grow it.
    assert_eq!(cube.query_consolidated(&["East"]), 100.0);
    assert_eq!(cube.store.cached_len(), 1);

    // The write does NOT clear the cache (that was the old behaviour)...
    cube.write(&["East"], CellValue::Numeric(250.0)).expect("overwrite");
    assert!(cube.store.version() > 0);

    // ...but the next read must not see the stale 100. Lazy invalidation.
    assert_eq!(
        cube.query_consolidated(&["East"]),
        250.0,
        "a write must invalidate the cached aggregate"
    );
    assert_eq!(cube.store.cached_len(), 1, "the fresh value is cached again");
}

#[test]
fn a_stale_cache_put_is_refused() {
    // The guard that matters once writers are concurrent (M3): a value computed from
    // an older snapshot must not be filed under the newer version.
    let cube = cube();
    let version = cube.store.version();
    cube.store.cache_put(vec![42], 7.0, version + 1);
    assert_eq!(
        cube.store.cached_len(),
        0,
        "a put tagged with a version that is not current must be dropped"
    );
}

#[test]
fn a_cloned_cube_writes_independently() {
    let cube = cube();
    cube.write(&["East"], CellValue::Numeric(100.0)).expect("write");

    let copy = cube.clone();
    copy.write(&["East"], CellValue::Numeric(500.0)).expect("write to the copy");

    assert_eq!(
        cube.query_consolidated(&["East"]),
        100.0,
        "writing to a clone must not change the original"
    );
    assert_eq!(copy.query_consolidated(&["East"]), 500.0);
}

/// A STRUCTURAL change (no cell written) must invalidate cached aggregates.
///
/// Regression test for the defect M2 introduced: the cache became version-tagged and
/// `clear_cache` became a no-op, so a `.rollup` could leave a stale total in place.
/// Nothing is written between the two queries below - only the hierarchy changes - so
/// a version bump is the only thing that can catch it.
#[test]
fn a_structural_change_invalidates_cached_aggregates() {
    let mut catalog = Catalog::new();
    catalog.add_cube("Sales", &["Region", "Measure"], Some("Measure"), true);
    {
        let cube = catalog.get_cube_mut("Sales").expect("Sales");
        for (region, value) in [("East", 10.0), ("West", 20.0), ("North", 5.0)] {
            cube.write(&[region, "Sales"], CellValue::Numeric(value))
                .expect("write");
        }
    }

    catalog.create_hierarchy("Region", "All").expect("hierarchy");
    {
        let dim = catalog.get_dimension("Region").expect("Region");
        let mut d = dim.write().expect("lock");
        d.add_component_in(Some("All"), "All", "East", 1.0).expect("link East");
        d.add_component_in(Some("All"), "All", "West", 1.0).expect("link West");
    }
    catalog.clear_all_caches();

    let first = catalog
        .get_cube("Sales")
        .expect("cube")
        .query_consolidated(&["All:All", "Sales"]);
    assert_eq!(first, 30.0, "East 10 + West 20");

    {
        let dim = catalog.get_dimension("Region").expect("Region");
        let mut d = dim.write().expect("lock");
        d.add_component_in(Some("All"), "All", "North", 1.0).expect("link North");
    }
    catalog.clear_all_caches();

    let second = catalog
        .get_cube("Sales")
        .expect("cube")
        .query_consolidated(&["All:All", "Sales"]);
    assert_eq!(
        second, 35.0,
        "the cached 30 must not survive a rollup that adds North (5)"
    );
}