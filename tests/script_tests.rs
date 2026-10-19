//! Golden-file test harness for SQL scripts.
//!
//! Each script lives in `tests/scripts/<name>.sql` with a companion
//! `tests/scripts/<name>.expected.txt` holding the exact expected output
//! (timing lines stripped by the batch runner).
//!
//! For every script the harness:
//!   1. runs the built binary in batch mode into a capture file, capturing the exit code,
//!   2. compares the captured output against the golden file,
//!   3. asserts the exit code against an EXPLICIT per-fixture expectation
//!      (`EXPECT_FAILURE` below), not against the golden text.
//!
//! Steps 2 and 3 are deliberately independent. The transcript proves what the
//! engine SAID; the exit code proves whether the runner reported success. Inferring
//! the second from the first made it a tautology (see `EXPECT_FAILURE`).
//!
//! Update the golden files in place after an intentional change by running:
//!   UPDATE_GOLDEN=1 cargo test --test script_tests        (bash)
//!   $env:UPDATE_GOLDEN=1; cargo test --test script_tests   (PowerShell)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Directory holding the script fixtures, relative to the crate root.
fn scripts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("scripts")
}

/// Fixtures that are EXPECTED to exit non-zero, identified by file stem.
///
/// This is explicit data on purpose. The previous revision inferred the
/// expectation from the golden transcript (`expected.contains("Error:")`),
/// which made the exit-code assertion a tautology: the exit code is derivable
/// from the transcript, so the check could only ever disagree on error-message
/// wording. Listing the fixtures here turns "which scripts are supposed to
/// fail" into something a reviewer can see in a diff, and makes an accidental
/// new failure (or an accidental pass) fail loudly.
///
/// When adding a fixture meant to end on an error, add its stem here.
const EXPECT_FAILURE: &[&str] = &[
    "create_duplicate",
    "select_no_from",
    "order",
    "hierarchies",
    "delete_member",
    "join",
    "import_missing_file",
];

/// True when the caller asked us to (re)write golden files instead of comparing.
fn update_mode() -> bool {
    std::env::var("UPDATE_GOLDEN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Fixtures that are NOT part of the golden suite, identified by file stem.
///
/// These are ad-hoc probes left in `tests/scripts/` while the CSV import and the
/// leaf-promotion warnings were being built. Nobody claimed them, and they cannot
/// be maintained as fixtures:
///
///   * `persist_probe` ends in `.save`, so its transcript depends on whether the
///     test process may write `database.bin` — it PASSES in an unconfined shell and
///     FAILS under a file sandbox (`Error: could not save database: ... os error 5`).
///     Its golden records `Database saved.`, so it is environment-dependent by
///     construction. Leaving it collected would also let a `cargo test` overwrite
///     the seeded reference `database.bin`.
///   * `probe` writes a hierarchy over a shared dimension, so it depends on which
///     members were seen as leaves first — deterministic today, fragile by design.
///   * `verify_probe` only means anything after `persist_probe` has run, but every
///     fixture gets a fresh catalogue, so it can only ever assert "cube not found".
///
/// Their `.sql` and `.expected.txt` files are deliberately left in place: they
/// document real investigations. They are excluded here rather than deleted, so
/// nobody has to decide who owns them, and rather than listed in `EXPECT_FAILURE`,
/// which would assert they are permanent fixtures that are MEANT to fail.
const SKIP_FIXTURES: &[&str] = &["probe", "verify_probe", "persist_probe"];

/// Collect every `*.sql` fixture, sorted for stable test output.
///
/// Every `.sql` file in `tests/scripts/` is collected automatically, so a stray
/// scratch file silently becomes a test that runs on every build. A file that is
/// not a real fixture has to be named in `SKIP_FIXTURES` to stay out.
fn collect_scripts() -> Vec<PathBuf> {
    let mut scripts: Vec<PathBuf> = fs::read_dir(scripts_dir())
        .expect("tests/scripts directory should exist")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "sql").unwrap_or(false))
        .filter(|p| {
            let stem = p
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            !SKIP_FIXTURES.contains(&stem.as_str())
        })
        .collect();
    scripts.sort();
    scripts
}

#[test]
fn skip_list_entries_exist_on_disk() {
    // A typo in SKIP_FIXTURES would silently re-enable a scratch fixture instead of
    // failing, so check the names against the directory.
    for stem in SKIP_FIXTURES {
        let path = scripts_dir().join(format!("{stem}.sql"));
        assert!(
            path.exists(),
            "SKIP_FIXTURES names '{stem}', but {} does not exist. Remove the entry \
             or fix the spelling.",
            path.display()
        );
    }
}

/// Normalizes output so comparisons are platform- and path-independent:
///   * newlines are unified to `\n`,
///   * the absolute script path is reduced to its file name, so the golden
///     does not embed the machine-specific fixture location.
fn normalize(s: &str, script: &Path) -> String {
    let abs = script.to_string_lossy();
    let base = script.file_name().unwrap().to_string_lossy();
    s.replace("\r\n", "\n").replace(abs.as_ref(), base.as_ref())
}

/// Directory for this suite's scratch output files.
///
/// NOT `std::env::temp_dir()`. Some harnesses run the test binary under a file
/// sandbox that permits writes only under the session workspace, so a `%TEMP%`
/// write fails with `PermissionDenied`. Because `run_script` used to read the
/// capture file with `unwrap_or_default()`, such a failure degraded to an EMPTY
/// string and every golden mismatched at once - a total, silent suite failure
/// with no diagnostic. This mirrors `serve_http_tests::artifact_dir`.
fn artifact_dir() -> PathBuf {
    let base = match std::env::var_os("OLAP_TEST_ARTIFACTS") {
        Some(v) => PathBuf::from(v),
        None => {
            let home = std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            home.join("Documents")
                .join("deepseek-harness")
                .join("default-workspace")
                .join("olap-test-artifacts")
        }
    };
    let dir = base.join(std::process::id().to_string());
    fs::create_dir_all(&dir).unwrap_or_else(|e| {
        panic!(
            "could not create the artifact directory {}: {e}. \
             Set OLAP_TEST_ARTIFACTS to a writable path.",
            dir.display()
        )
    });
    dir
}

/// Runs one script through the binary and returns (output, exit_ok).
fn run_script(script: &Path) -> (String, bool) {
    let out_file = artifact_dir().join(format!(
        "olap_harness_{}.txt",
        script.file_stem().unwrap().to_string_lossy()
    ));

    let status = Command::new(env!("CARGO_BIN_EXE_olap_engine"))
        // Pin the working directory to the crate root.
        //
        // Fixtures reference their CSVs relatively (`tests/scripts/import_data.csv`,
        // `examples/Plateau/...`), and the engine resolves those against its OWN
        // working directory. Cargo makes no promise about the cwd it gives an
        // integration test - it is usually the crate root, but NOT reliably so once
        // the crate sits at a different depth. Moving this repo one level deeper was
        // enough to break every CSV import with "Could not open ...", which reads
        // like an import regression and is not one. Pinning it here fixes the class
        // of failure once instead of rewriting 13 fixtures.
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .arg(script)
        .arg("--out")
        .arg(&out_file)
        .status()
        .expect("failed to launch the olap_engine binary");

    // Do NOT swallow the read error: a missing capture file means the sandbox
    // blocked the write (or --out is broken), which must fail loudly instead of
    // masquerading as an empty golden mismatch.
    let output = fs::read_to_string(&out_file).unwrap_or_else(|e| {
        panic!(
            "could not read the script capture file {} ({e}). \
             The batch binary was run with --out; if this path is unreadable the \
             test sandbox is blocking it. Set OLAP_TEST_ARTIFACTS to a writable path.",
            out_file.display()
        )
    });
    let _ = fs::remove_file(&out_file);

    (output, status.success())
}

#[test]
fn script_golden_tests() {
    let scripts = collect_scripts();
    assert!(
        !scripts.is_empty(),
        "no .sql fixtures found in {}",
        scripts_dir().display()
    );

    let mut failures: Vec<String> = Vec::new();

    for script in &scripts {
        let name = script.file_name().unwrap().to_string_lossy().to_string();
        let golden_path = script.with_extension("expected.txt");

        let (output, exit_ok) = run_script(script);
        let actual = normalize(&output, script);

        // ---- UPDATE MODE: write the golden and move on ----
        if update_mode() {
            fs::write(&golden_path, &actual)
                .unwrap_or_else(|e| panic!("could not write {}: {}", golden_path.display(), e));
            eprintln!("updated golden: {}", golden_path.display());
            continue;
        }

        // ---- COMPARE MODE ----
        let expected = match fs::read_to_string(&golden_path) {
            Ok(s) => normalize(&s, script),
            Err(_) => {
                failures.push(format!(
                    "{}: missing golden file {} (run with UPDATE_GOLDEN=1 to create it)",
                    name,
                    golden_path.display()
                ));
                continue;
            }
        };

        // 1. Output must match the golden exactly.
        if actual != expected {
            failures.push(format!(
                "{}: output mismatch\n{}",
                name,
                diff(&expected, &actual)
            ));
        }

        // 2. The exit code must match the fixture's DECLARED expectation, not an
        //    inference from the golden text it is being compared against.
        //
        //    This replaced `expected.contains("Error:") || expected.contains("failed")`.
        //    That inference was a tautology: `run_script` fails exactly when a command
        //    printed an error, and an error line is what the substring test detects, so
        //    the assertion could only ever disagree on the WORDING of a message. It
        //    also made the passing/failing split an emergent property of blessed text
        //    rather than reviewable data. `EXPECT_FAILURE` above is that data.
        let stem = script.file_stem().unwrap().to_string_lossy();
        let expect_failure = EXPECT_FAILURE.contains(&stem.as_ref());
        if expect_failure == exit_ok {
            let want = if expect_failure { "non-zero" } else { "zero" };
            let hint = if expect_failure {
                " (is this fixture no longer intended to fail? then remove it from \
                 EXPECT_FAILURE)"
            } else {
                " (if it is meant to end on an error, add its stem to EXPECT_FAILURE)"
            };
            failures.push(format!(
                "{}: expected {} exit code, but the binary returned the opposite{}",
                name, want, hint
            ));
        }
    }

    // 3. A fixture added to the golden suite but forgotten in EXPECT_FAILURE would
    //    otherwise only be caught when it next runs and fails. Name it up front.
    //    (A stale entry - declared but with no fixture - is the other half of the
    //    same mistake, and is equally silent.)
    let stems: Vec<String> = scripts
        .iter()
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
        .collect();
    for declared in EXPECT_FAILURE {
        if !stems.iter().any(|s| s == declared) {
            failures.push(format!(
                "EXPECT_FAILURE lists '{}', which is not a fixture in {}",
                declared,
                scripts_dir().display()
            ));
        }
    }

    if !failures.is_empty() {
        panic!(
            "\n{} script test(s) failed:\n\n{}\n",
            failures.len(),
            failures.join("\n\n")
        );
    }
}

/// Produces a compact line-by-line diff of expected vs. actual.
fn diff(expected: &str, actual: &str) -> String {
    let exp: Vec<&str> = expected.lines().collect();
    let act: Vec<&str> = actual.lines().collect();
    let mut out = String::new();
    let max = exp.len().max(act.len());

    out.push_str("    --- expected / + actual ---\n");
    for i in 0..max {
        match (exp.get(i), act.get(i)) {
            (Some(e), Some(a)) if e == a => {}
            (Some(e), Some(a)) => {
                out.push_str(&format!("    line {}:\n", i + 1));
                out.push_str(&format!("      - {}\n", e));
                out.push_str(&format!("      + {}\n", a));
            }
            (Some(e), None) => {
                out.push_str(&format!("      - {} (missing in actual)\n", e));
            }
            (None, Some(a)) => {
                out.push_str(&format!("      + {} (extra in actual)\n", a));
            }
            (None, None) => {}
        }
    }
    out
}
