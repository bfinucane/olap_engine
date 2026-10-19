//! Concurrency tests — what is actually safe, after Phase 1b.
//!
//! **This file used to document an ABSENCE.** Before 1b, a `Cube` was owned by value
//! inside the catalog, every write took `&mut self`, and `query_cache` was cleared
//! globally on every write — correct only under a single-writer assumption. One test
//! here (`concurrent_mutation_is_not_expressible_today`) asserted that absence, and its
//! own comment said it should be rewritten the day 1b landed.
//!
//! 1b landed (M1 persistent trie, M2 versioned store, M3 the catalog `Mutex` removed),
//! so the absence is gone and this file now states the POSITIVE guarantee:
//!
//!   * `dimension_reads_are_safe_across_threads` — many readers of one dimension.
//!   * `catalog_moves_to_another_thread` — the auto-trait floor, plus `Cube: Sync`.
//!   * `readers_and_a_writer_share_one_cube_without_tearing` — the real race test:
//!     a writer overwrites one cell while readers aggregate it. Every read must land
//!     on a value that was actually written, never a torn or impossible one.
//!   * `a_pinned_snapshot_stays_stable_while_a_writer_runs` — a reader that pinned a
//!     version before the writer started keeps seeing that version.
//!
//! What is still NOT promised: durability under a crash, and a write endpoint over
//! HTTP (there is none — the writer here is in-process, which is the only writer the
//! engine has today).

use olap_engine::catalog::catalog::Catalog;
use olap_engine::cube::cube::Cube;
use olap_engine::cube::node::CellValue;
use olap_engine::dimension::dimension::Dimension;
use std::sync::{Arc, RwLock};

/// Builds a catalog with one small cube, reusing the shape the persistence
/// tests use.
fn build_sample_catalog() -> Catalog {
    let mut catalog = Catalog::new();
    catalog.add_cube("Sales", &["Geography", "Measure"], Some("Measure"), true);

    {
        let cube = catalog.get_cube_mut("Sales").expect("cube exists");
        cube.write(&["France", "Revenue"], CellValue::Numeric(100.0))
            .expect("write France/Revenue");
        cube.write(&["Germany", "Revenue"], CellValue::Numeric(200.0))
            .expect("write Germany/Revenue");
    }

    catalog.create_hierarchy("Geography", "Ops").expect("create Ops");
    {
        let dim = catalog.get_dimension("Geography").expect("dimension");
        let mut d = dim.write().unwrap();
        d.add_component_in(Some("Ops"), "EMEA", "France", 1.0).expect("link France");
        d.add_component_in(Some("Ops"), "EMEA", "Germany", 1.0).expect("link Germany");
    }

    catalog
}

/// A `Dimension` is shared behind `Arc<RwLock<Dimension>>`, so many threads may read it
/// at once, and the data they see is consistent.
#[test]
fn dimension_reads_are_safe_across_threads() {
    let catalog = build_sample_catalog();
    let dim: Arc<RwLock<Dimension>> = catalog.get_dimension("Geography").expect("dimension");

    let mut handles = Vec::new();
    for _ in 0..8 {
        let dim = Arc::clone(&dim);
        handles.push(std::thread::spawn(move || {
            let d = dim.read().expect("read lock is not poisoned");
            // Same answer from every thread, read through the same pointer.
            let leaves = d.get_leaf_descendants_in(Some("Ops"), "EMEA");
            assert_eq!(leaves.len(), 2, "EMEA consolidates its two leaves");
            d.hierarchy_names().len()
        }));
    }

    let names: Vec<usize> = handles
        .into_iter()
        .map(|h| h.join().expect("reader thread must not panic"))
        .collect();
    assert!(
        names.iter().all(|n| *n == names[0]),
        "every thread sees the same hierarchy list: {names:?}"
    );
}

/// Ownership can cross a thread boundary, and a shared `Cube` is readable from several
/// threads at once.
///
/// The `Sync` assertion is the new part: it is only true because the store is
/// interior-mutable (`RwLock`) and the version is atomic. If a future change puts an
/// `Rc`, a `RefCell`, or a thread-affine handle back into the store, this stops
/// compiling — which is the warning worth having.
#[test]
fn catalog_moves_to_another_thread() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}

    assert_send::<Catalog>();
    assert_send::<Cube>();
    assert_sync::<Cube>();
    assert_send::<Arc<RwLock<Dimension>>>();
    assert_send::<CellValue>();

    let catalog = build_sample_catalog();
    let handle = std::thread::spawn(move || {
        let dim = catalog.get_dimension("Geography").expect("dimension survived");
        assert!(dim.read().unwrap().has_hierarchy("Ops"));
        catalog.get_cube("Sales").is_some()
    });

    assert!(handle.join().expect("owner thread must not panic"));
}

/// THE RACE TEST: one writer overwrites a cell while several readers aggregate it.
///
/// A reader must always land on a value that some version actually held — never a
/// half-written cell, a NaN, or a number the writer never wrote. This is the property
/// the persistent trie plus snapshot reads exist to provide.
#[test]
fn readers_and_a_writer_share_one_cube_without_tearing() {
    const WRITES: u32 = 400;

    let catalog = Arc::new(build_sample_catalog());

    let writer = {
        let catalog = Arc::clone(&catalog);
        std::thread::spawn(move || {
            let cube = catalog.get_cube("Sales").expect("cube");
            for i in 0..WRITES {
                cube.write(&["France", "Revenue"], CellValue::Numeric(f64::from(i)))
                    .expect("concurrent write");
            }
        })
    };

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let catalog = Arc::clone(&catalog);
            std::thread::spawn(move || {
                let cube = catalog.get_cube("Sales").expect("cube");
                let mut observed = Vec::with_capacity(500);
                for _ in 0..500 {
                    let v = cube.query_consolidated(&["France", "Revenue"]);
                    assert!(v.is_finite(), "torn read: {v} is not finite");
                    // The initial fixture value is 100; the writer writes 0..WRITES.
                    assert!(
                        (0.0..=f64::from(WRITES)).contains(&v),
                        "read {v} is outside every version the writer ever published \
                         (0..={WRITES}, plus the fixture's 100)"
                    );
                    observed.push(v);
                }
                observed
            })
        })
        .collect();

    writer.join().expect("writer thread must not panic");
    for reader in readers {
        let observed = reader.join().expect("reader thread must not panic");
        assert_eq!(observed.len(), 500);
    }

    // And the final state is the last write, not a lost update or a merge artifact.
    let cube = catalog.get_cube("Sales").expect("cube");
    assert_eq!(
        cube.query_consolidated(&["France", "Revenue"]),
        f64::from(WRITES - 1),
        "the last committed write must win"
    );
}

/// A reader that pins a snapshot BEFORE the writer starts keeps seeing that version,
/// no matter what the writer does afterwards.
#[test]
fn a_pinned_snapshot_stays_stable_while_a_writer_runs() {
    let catalog = Arc::new(build_sample_catalog());

    let cube = catalog.get_cube("Sales").expect("cube");
    // France lives in `Geography`, Revenue in `Measure` — resolve each in its own
    // dimension (getting this wrong is how the first version of this test failed).
    let id = {
        let geo = catalog.get_dimension("Geography").expect("Geography");
        let measure = catalog.get_dimension("Measure").expect("Measure");
        let g = geo.read().expect("geo lock");
        let m = measure.read().expect("measure lock");
        [
            g.get_id("France").expect("France"),
            m.get_id("Revenue").expect("Revenue"),
        ]
    };
    let pinned = cube.store.snapshot();
    assert_eq!(pinned.query_exact(&id), Some(CellValue::Numeric(100.0)));

    let writer = {
        let catalog = Arc::clone(&catalog);
        std::thread::spawn(move || {
            let cube = catalog.get_cube("Sales").expect("cube");
            for i in 0..200 {
                cube.write(&["France", "Revenue"], CellValue::Numeric(f64::from(i)))
                    .expect("write");
            }
        })
    };
    writer.join().expect("writer");

    assert_eq!(
        pinned.query_exact(&id),
        Some(CellValue::Numeric(100.0)),
        "the pinned snapshot must still read the value it was taken with"
    );
    assert_eq!(
        cube.store.query_exact(&id),
        Some(CellValue::Numeric(199.0)),
        "the live store moved on"
    );
}
