//! Integration tests for Phase 1a durable persistence.
//!
//! These pin the three guarantees the roadmap lists under Phase 1a's
//! "Definition of done":
//!
//!   1. a `save` -> `load` round-trip preserves the data (numbers, strings,
//!      AND the multi-hierarchy structure);
//!   2. loading a garbage / foreign file yields a clean typed error, not a
//!      panic;
//!   3. saving is non-destructive: the old file is replaced atomically and no
//!      temp debris is left behind.
//!
//! They exercise the REAL engine (the `olap_engine` library), not the REPL, so
//! they are fast and independent of the binary.

use olap_engine::catalog::catalog::{Catalog, PersistError};
use olap_engine::cube::node::CellValue;

/// A unique temp path per test, in the OS temp dir so we never touch the repo's
/// `database.bin`.
fn temp_path(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("olap_persist_{}_{}_{}.bin", tag, std::process::id(), n));
    p
}

/// Builds a small catalog with one multi-hierarchy dimension and one cube.
fn build_sample_catalog() -> Catalog {
    let mut catalog = Catalog::new();

    // Sales(France, Germany) with an ADDITIONAL "Ops" hierarchy over the same
    // shared leaves. Measures live only at the leaves, shared by all hierarchies.
    catalog.add_cube("Sales", &["Geography", "Measure"], Some("Measure"), true);

    {
        let cube = catalog.get_cube_mut("Sales").expect("cube exists");
        cube.write(&["France", "Revenue"], CellValue::Numeric(100.0));
        cube.write(&["Germany", "Revenue"], CellValue::Numeric(200.0));
        // A STRING cell, to prove non-numeric values survive too.
        cube.write(&["France", "Note"], CellValue::String("bonjour".to_string()));
    }

    // Second hierarchy "Ops" over the SAME leaves, plus a consolidation so the
    // hierarchy has real structure to persist.
    catalog.create_hierarchy("Geography", "Ops").expect("create Ops");
    {
        let dim = catalog.get_dimension("Geography").expect("dim exists");
        let mut d = dim.write().unwrap();
        // Create the parent aggregate "EMEA" in the Ops hierarchy and link each
        // existing shared leaf to it. (add_component_in returns the old parent on
        // a re-parent; there is none here.)
        d.add_component_in(Some("Ops"), "EMEA", "France", 1.0).expect("link France");
        d.add_component_in(Some("Ops"), "EMEA", "Germany", 1.0).expect("link Germany");
    }

    catalog
}

#[test]
fn save_then_load_round_trip_preserves_data_and_hierarchies() {
    let path = temp_path("roundtrip");
    let path_str = path.to_str().unwrap();

    let catalog = build_sample_catalog();
    catalog.save_to_disk(path_str).expect("save should succeed");

    // --- Reload and verify it is byte-for-byte the same *data*. ---
    let reloaded = Catalog::load_from_disk(path_str).expect("load should succeed");

    let cube = reloaded.get_cube("Sales").expect("cube survived load");

    // Numeric leaves.
    assert_eq!(
        cube.read_cell(&["France", "Revenue"]),
        Some(CellValue::Numeric(100.0))
    );
    assert_eq!(
        cube.read_cell(&["Germany", "Revenue"]),
        Some(CellValue::Numeric(200.0))
    );
    // String leaf.
    assert_eq!(
        cube.read_cell(&["France", "Note"]),
        Some(CellValue::String("bonjour".to_string()))
    );

    // The additional hierarchy + its consolidation survived.
    let dim = reloaded.get_dimension("Geography").expect("dim survived");
    let d = dim.read().unwrap();
    assert!(d.has_hierarchy("Ops"), "extra hierarchy 'Ops' survived");

    let emea_leaves = d.get_leaf_descendants_in(Some("Ops"), "EMEA");
    assert_eq!(emea_leaves.len(), 2, "EMEA still consolidates 2 leaves");

    // A read THROUGH the hierarchy still aggregates correctly.
    drop(d);
    let mut cube = reloaded.get_cube("Sales").cloned().expect("cube");
    let emea_total = cube.query_consolidated(&["Ops:EMEA", "Revenue"]);
    assert_eq!(emea_total, 300.0, "Ops:EMEA = 100 + 200");

    let _ = std::fs::remove_file(&path);
}

/// Asserts that `Catalog::load_from_disk` fails, and returns the error.
///
/// `Catalog` does NOT implement `Debug`, so a `match` on the whole `Result`
/// cannot be `panic!("{:?}")`-formatted; we must unwrap the error out first.
fn expect_load_err(path: &str) -> PersistError {
    match Catalog::load_from_disk(path) {
        Ok(_) => panic!("expected a load error, but the load succeeded"),
        Err(e) => e,
    }
}

#[test]
fn load_of_garbage_file_is_a_clean_typed_error() {
    let path = temp_path("garbage");
    let path_str = path.to_str().unwrap();

    // Not even the magic bytes.
    std::fs::write(&path, b"this is not a catalog at all").unwrap();
    match expect_load_err(path_str) {
        PersistError::NotACatalog => {}
        other => panic!("expected NotACatalog, got {:?}", other),
    }

    // A file too short to hold the 8-byte header is also "not a catalog"
    // (not an unexpected-Eof panic).
    std::fs::write(&path, b"OLA").unwrap();
    match expect_load_err(path_str) {
        PersistError::NotACatalog => {}
        other => panic!("expected NotACatalog for short file, got {:?}", other),
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn newer_format_version_is_refused_not_misread() {
    let path = temp_path("futurever");
    let path_str = path.to_str().unwrap();

    // Correct magic, but a version far in the future.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"OLAP");
    bytes.extend_from_slice(&999u32.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 64]); // filler, never decoded
    std::fs::write(&path, &bytes).unwrap();

    match expect_load_err(path_str) {
        PersistError::UnsupportedVersion { found, .. } => assert_eq!(found, 999),
        other => panic!("expected UnsupportedVersion, got {:?}", other),
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn save_is_non_destructive_and_leaves_no_temp_debris() {
    let path = temp_path("atomic");
    let path_str = path.to_str().unwrap();
    let dir = path.parent().unwrap().to_path_buf();
    let file_name = path.file_name().unwrap().to_string_lossy().to_string();

    // Save once (creates the file), save again (replaces it).
    let catalog = build_sample_catalog();
    catalog.save_to_disk(path_str).expect("first save");
    catalog.save_to_disk(path_str).expect("second save (replace)");

    // The file exists and still loads.
    assert!(path.exists(), "target file exists after saves");
    Catalog::load_from_disk(path_str).expect("reload after replace");

    // No sibling temp files for THIS target were left behind. (Saves from other
    // concurrent tests use distinct names, so we only look for our own.)
    let prefix = format!(".{}", file_name);
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(&prefix) && n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temp debris left behind: {:?}", leftovers);

    let _ = std::fs::remove_file(&path);
}
