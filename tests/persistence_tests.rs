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
//! It also pins the two boundaries that "non-destructive" implies but did not
//! previously cover:
//!
//!   4. an INTERRUPTED save (the target cannot be replaced) leaves the previous
//!      database exactly as it was;
//!   5. `Cube::write` PANICS on more coordinates than the cube has dimensions,
//!      while `write_splashed` on the same input is a typed error. See the
//!      comment on that test: it documents a defect rather than endorsing it.
//!
//! They exercise the REAL engine (the `olap_engine` library), not the REPL, so
//! they are fast and independent of the binary.

use olap_engine::catalog::catalog::{Catalog, PersistError};
use olap_engine::cube::cube::{Cube, SplashMode};
use olap_engine::cube::node::CellValue;
use olap_engine::dimension::dimension::Dimension;
use std::sync::{Arc, RwLock};

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

/// Lists the temp siblings a save may leave in `dir`: names beginning with the
/// target's `.<file>.` prefix and ending in `.tmp`. The exact name is an
/// implementation detail of `temp_sibling_path`, so only the shape is relied on.
fn temp_siblings(dir: &std::path::Path, prefix: &str) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.starts_with(prefix) && n.ends_with(".tmp"))
                .collect()
        })
        .unwrap_or_default()
}

/// Builds a small catalog with one multi-hierarchy dimension and one cube.
fn build_sample_catalog() -> Catalog {
    let mut catalog = Catalog::new();

    // Sales(France, Germany) with an ADDITIONAL "Ops" hierarchy over the same
    // shared leaves. Measures live only at the leaves, shared by all hierarchies.
    catalog.add_cube("Sales", &["Geography", "Measure"], Some("Measure"), true);

    {
        let cube = catalog.get_cube_mut("Sales").expect("cube exists");
        cube.write(&["France", "Revenue"], CellValue::Numeric(100.0))
            .expect("write France/Revenue");
        cube.write(&["Germany", "Revenue"], CellValue::Numeric(200.0))
            .expect("write Germany/Revenue");
        // A STRING cell, to prove non-numeric values survive too.
        cube.write(&["France", "Note"], CellValue::String("bonjour".to_string()))
            .expect("write France/Note");
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

/// An INTERRUPTED save must not damage the previous database.
///
/// This closes the gap between what Phase 1a's "Definition of done" claims
/// ("killing the process mid-save leaves the old complete file or the new
/// complete file - never truncated") and what was actually asserted: the old
/// suite only ever exercised two *successful* saves in a row.
///
/// The interruption is simulated the way a real one manifests - the target
/// cannot be replaced while the save runs. The target is held open with
/// `FILE_SHARE_READ` only, so on Windows the temp file is written and fsynced
/// and then the rename over the target is denied.
///
/// Either outcome is a valid pass:
///   * the rename is denied (the interesting branch) -> the old bytes must
///     still be on disk, and the temp file written just before the failure
///     must be cleaned up (a refused save leaves no debris);
///   * the rename somehow succeeds despite the exclusive share -> the target
///     must then be the complete NEW file, never a truncated one.
///
/// What is a failure in both cases is a zero-length target: that is exactly the
/// `File::create`-truncates-first hazard the atomic-write design exists to
/// prevent.
#[test]
fn interrupt_a_save_leaves_the_previous_database_intact() {
    use std::fs::OpenOptions;

    let path = temp_path("interrupted");
    let path_str = path.to_str().unwrap();

    // 1. A first, complete save: this is the "previous" database.
    let first = build_sample_catalog();
    first.save_to_disk(path_str).expect("initial save");
    let before = std::fs::read(&path).expect("read the saved catalogue");

    // 2. Hold the target open, denying replace/delete by other handles.
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.share_mode(0x0000_0001); // FILE_SHARE_READ only
    }
    let lock = opts.open(&path).expect("open target exclusively");

    // 3. Attempt a save the engine may well fail to publish.
    // Snapshot any temp siblings that exist BEFORE the attempt, so leftovers
    // from this save can be told apart from anything else in the directory.
    let dir = path.parent().expect("temp dir").to_path_buf();
    let file_name = path.file_name().unwrap().to_string_lossy().to_string();
    let tmp_prefix = format!(".{}", file_name);
    let tmp_before = temp_siblings(&dir, &tmp_prefix);

    let second = build_sample_catalog();
    let result = second.save_to_disk(path_str);
    let tmp_after = temp_siblings(&dir, &tmp_prefix);

    // The lock is deliberately NOT dropped before the assertions: the whole
    // point is that the target stays unavailable for the duration of the save.
    match &result {
        Err(e) => {
            // The branch this test exists for: publication was refused.
            eprintln!("[interrupted-save] rename was REFUSED ({e}); checking the old file survived");
            let after = std::fs::read(&path).expect("old file still present after the failed save");
            assert_eq!(
                after, before,
                "a failed save must leave the previous database byte-identical"
            );
            assert!(!after.is_empty(), "the database must never be truncated to zero bytes");

            // A refused save must clean up the fsynced temp file it wrote.
            // (Previously it leaked a complete, loadable temp file that the
            // caller could not even name; this now has to be empty.)
            let leaked: Vec<&String> = tmp_after
                .iter()
                .filter(|n| !tmp_before.contains(n))
                .collect();
            assert!(
                leaked.is_empty(),
                "a refused save must not leave temp debris behind: {leaked:?}"
            );
        }
        Ok(()) => {
            // Publication succeeded anyway; then it must be the complete NEW
            // file, and the old bytes must be gone rather than truncated.
            eprintln!("[interrupted-save] rename SUCCEEDED despite the exclusive lock; checking the new file is complete");
            Catalog::load_from_disk(path_str).expect("a published file must load");
            let after = std::fs::read(&path).expect("read after the successful save");
            assert!(!after.is_empty(), "a published file must never be empty");
            assert_ne!(after, before, "a published save must have replaced the old bytes");
        }
    }

    drop(lock);

    // No truncation in any branch.
    assert!(
        std::fs::metadata(&path).unwrap().len() > 0,
        "database.bin must never be left at zero bytes"
    );

    // Clean up: the target, plus any temp sibling this run left behind. The
    // cleanup is reported so the leak is visible even though the file is gone.
    let _ = std::fs::remove_file(&path);
    let leftovers = temp_siblings(&dir, &tmp_prefix);
    for n in &leftovers {
        let _ = std::fs::remove_file(dir.join(n));
    }
    if !leftovers.is_empty() {
        eprintln!("[interrupted-save] cleaned up leftover temp file(s): {leftovers:?}");
    }
}

/// `Cube::write` returns a typed error - never a panic - when given fewer or
/// more coordinates than the cube has dimensions, and `write_splashed` agrees.
///
/// This used to be a *characterisation* of a defect: `write` indexed
/// `self.dimensions[i]` and panicked with an index-out-of-bounds abort, while
/// `write_splashed` rejected the identical input with a typed error. That
/// divergence mattered because `engine/sql.rs` calls `write` on the INSERT
/// fall-through path, reachable from caller-supplied input (a script now, a
/// query plan over the wire soon). `write` now mirrors `write_splashed`.
///
/// The assertions below are the ones that make the fix meaningful:
///   * a malformed write is an `Err`, and the message names the mismatch;
///   * nothing is stored by the refused write (the guard fires before the
///     store is touched);
///   * a legal write still works, so the guard is specific to bad input;
///   * both writers agree on the error text for the same mismatch.
#[test]
fn write_rejects_a_coordinate_dimension_mismatch_instead_of_panicking() {
    // A one-dimensional cube, built directly so no SQL layer normalises the
    // coordinate count for us.
    let dim = Arc::new(RwLock::new(Dimension::new("Region")));
    let mut cube = Cube::new("Sales", vec!["Region".to_string()], vec![dim], None, true);

    // Two coordinates into a one-dimension cube: must be a typed error, not a
    // panic. (No `catch_unwind`: a panic here would fail the test outright,
    // which is the point.)
    let err = cube
        .write(&["France", "Ghost"], CellValue::Numeric(123.0))
        .expect_err("write rejects a coordinate/dimension mismatch");
    assert!(
        err.contains("Expected 1 coordinate(s), got 2"),
        "unexpected error text: {err}"
    );

    // Too FEW coordinates is the same class of input and must also be refused.
    let err_few = cube
        .write(&[], CellValue::Numeric(1.0))
        .expect_err("write rejects too few coordinates");
    assert!(
        err_few.contains("Expected 1 coordinate(s), got 0"),
        "unexpected error text: {err_few}"
    );

    // The refused writes stored nothing, and the cube is still usable.
    assert_eq!(cube.read_cell(&["France"]), None, "no value was stored");

    cube.write(&["France"], CellValue::Numeric(7.0))
        .expect("a legal write succeeds");
    assert_eq!(
        cube.read_cell(&["France"]),
        Some(CellValue::Numeric(7.0)),
        "the cube still works for legal coordinates"
    );

    // The other writer agrees on the same mismatch.
    let err_splashed = cube
        .write_splashed(&["France", "Ghost"], 123.0, SplashMode::Replace)
        .expect_err("write_splashed rejects a coordinate/dimension mismatch");
    assert!(
        err_splashed.contains("Expected 1 coordinate(s), got 2"),
        "unexpected error text: {err_splashed}"
    );
}
