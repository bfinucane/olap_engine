//! Golden-file test harness for SQL scripts.
//!
//! Each script lives in `tests/scripts/<name>.sql` with a companion
//! `tests/scripts/<name>.expected.txt` holding the exact expected output
//! (timing lines stripped by the batch runner).
//!
//! For every script the harness:
//!   1. runs the built binary in batch mode into a temp file, capturing the exit code,
//!   2. compares the captured output against the golden file,
//!   3. asserts the exit code is consistent with the golden content
//!      (a golden containing "Error:" implies a non-zero exit code).
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

/// True when the caller asked us to (re)write golden files instead of comparing.
fn update_mode() -> bool {
    std::env::var("UPDATE_GOLDEN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Collect every `*.sql` fixture, sorted for stable test output.
fn collect_scripts() -> Vec<PathBuf> {
    let mut scripts: Vec<PathBuf> = fs::read_dir(scripts_dir())
        .expect("tests/scripts directory should exist")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "sql").unwrap_or(false))
        .collect();
    scripts.sort();
    scripts
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

/// Runs one script through the binary and returns (output, exit_ok).
fn run_script(script: &Path) -> (String, bool) {
    let out_file = std::env::temp_dir().join(format!(
        "olap_harness_{}.txt",
        script.file_stem().unwrap().to_string_lossy()
    ));

    let status = Command::new(env!("CARGO_BIN_EXE_olap_engine"))
        .arg(script)
        .arg("--out")
        .arg(&out_file)
        .status()
        .expect("failed to launch the olap_engine binary");

    let output = fs::read_to_string(&out_file).unwrap_or_default();
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

        // 2. Exit code must be consistent with the golden content:
        //    a golden containing "Error:" implies the script should fail.
        let expect_failure = expected.contains("Error:") || expected.contains("failed");
        if expect_failure == exit_ok {
            let want = if expect_failure { "non-zero" } else { "zero" };
            failures.push(format!(
                "{}: expected {} exit code, but the binary returned the opposite",
                name, want
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
