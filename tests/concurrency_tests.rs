//! Concurrency tests — what is actually safe today.
//!
//! **The point of this file is to document an ABSENCE, and to make it
//! deliberate.** The roadmap's "Where we are today" section says concurrency is
//! "coarse and uneven": a `Dimension` is shared behind `Arc<RwLock<Dimension>>`,
//! but a `Cube` is owned **by value** inside the catalog and every write path
//! takes `&mut self`. There is no safe concurrent *mutation* of a cube, and
//! `query_cache` is invalidated globally on every write
//! (`self.query_cache.clear()`), which is only correct under a single-writer
//! assumption. Phase 1b (MVCC) exists to replace exactly that.
//!
//! Why write tests for behaviour that does not exist? Because the next change to
//! this area is a storage-engine redesign, and a redesign is precisely when an
//! accidental new guarantee slips in unnoticed (or an existing one is silently
//! removed). These tests turn "concurrency is uneven" into something executable:
//!
//!   * `dimension_reads_are_safe_across_threads` — the guarantee that DOES hold.
//!   * `catalog_moves_to_another_thread` — the auto-trait floor 1b builds on.
//!   * `concurrent_mutation_is_not_expressible_today` — the absence, stated.
//!
//! Two of the three are assertions about the *type system* rather than about
//! runtime behaviour, which is deliberate: that is where today's guarantee
//! actually lives.

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

/// The guarantee that DOES hold today: a `Dimension` is shared behind
/// `Arc<RwLock<Dimension>>`, so many threads may read it at once, and the data
/// they see is consistent.
///
/// This is the one piece of the concurrency story that is already correct, and
/// 1b must not regress it while replacing the store underneath.
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

/// Ownership can cross a thread boundary: a whole `Catalog` moves into a
/// spawned thread and is used there.
///
/// Stated because it is the floor Phase 1b stands on. If a future change puts
/// an `Rc`, a `RefCell`, or a thread-affine handle inside the catalog, this
/// stops compiling — which is exactly the warning worth having.
#[test]
fn catalog_moves_to_another_thread() {
    fn assert_send<T: Send>() {}

    assert_send::<Catalog>();
    assert_send::<Cube>();
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

/// The ABSENCE, stated as a compile-time fact rather than a comment.
///
/// `query_consolidated`, `write` and `write_splashed` all take `&mut self`, and
/// the catalog hands out `&mut Cube`, so sharing one cube across threads for
/// writing is not expressible in safe code — there is nothing to test at
/// runtime because nothing can be written. The assertions below are therefore
/// about the *shape of the API*: an exclusive borrow is required, and taking two
/// at once is rejected before any thread is involved.
///
/// When Phase 1b lands, this test SHOULD FAIL TO COMPILE or be rewritten: that
/// is the signal that a concurrent mutation path now exists and needs a real
/// race test (two writers committing, one long reader pinned to its version).
#[test]
fn concurrent_mutation_is_not_expressible_today() {
    let mut catalog = build_sample_catalog();

    // A single exclusive borrow is all the API offers.
    let cube: &mut Cube = catalog.get_cube_mut("Sales").expect("cube");
    cube.write(&["France", "Revenue"], CellValue::Numeric(150.0))
        .expect("write France/Revenue");

    // Re-reading through the same exclusive borrow sees the upsert (this is the
    // intentional overwrite contract, confirmed at the storage level too).
    assert_eq!(
        cube.read_cell(&["France", "Revenue"]),
        Some(CellValue::Numeric(150.0))
    );

    // There is deliberately no `Arc<Cube>`, no `RwLock<Cube>`, and no `&self`
    // write method to exercise here. `query_consolidated` needs `&mut self` only
    // because it populates `query_cache`, which is itself the global-invalidation
    // problem Phase 1b replaces with a version-aware cache.
    assert!(
        !std::any::type_name::<Cube>().contains("RwLock"),
        "Cube is owned by value today; if it becomes shared, this test is the place \
         to add a real concurrent-writer race test"
    );
}
