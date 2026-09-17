use std::io::{self, BufRead, Write};
use std::fmt::Write as _;
use std::fs::File;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant; // Adds precision timing!

use olap_engine::catalog::catalog::Catalog;
use olap_engine::engine::sql::execute_sql;

/// Parsed command-line arguments for the batch (headless) mode.
struct CliArgs {
    /// The script to run non-interactively. `None` means "start the REPL".
    script: Option<String>,
    /// Optional file to write the captured output to. `None` means stdout.
    out: Option<String>,
    /// Optional golden file to diff the output against.
    golden: Option<String>,
    /// If set, (re)write the golden file with the current output instead of comparing.
    bless: bool,
}

/// Minimal hand-rolled argument parser (no external crates).
/// Recognized forms:
///   olap_engine <script.sql> [--out <file>] [--golden <file>] [--bless]
fn parse_args(argv: &[String]) -> CliArgs {
    let mut args = CliArgs {
        script: None,
        out: None,
        golden: None,
        bless: false,
    };

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--out" => {
                if i + 1 < argv.len() { args.out = Some(argv[i + 1].clone()); i += 1; }
            }
            "--golden" => {
                if i + 1 < argv.len() { args.golden = Some(argv[i + 1].clone()); i += 1; }
            }
            "--bless" => { args.bless = true; }
            other => {
                // First bare argument (not starting with '-') is the script path.
                if args.script.is_none() && !other.starts_with('-') {
                    args.script = Some(other.to_string());
                }
            }
        }
        i += 1;
    }

    args
}

/// Strips non-deterministic timing lines so output can be diffed against a golden file.
/// Removes lines like "(Time: 1.23ms)" and the "... finished. Executed N commands in ..." line.
fn strip_timing(text: &str) -> String {
    text.lines()
        .filter(|l| !l.starts_with("(Time:") && !l.contains(". Executed ") )
        .map(|l| format!("{}\n", l))
        .collect()
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&argv);

    match &args.script {
        Some(script) => run_batch(script, &args),
        None => { run_repl(); ExitCode::SUCCESS }
    }
}

/// Interactive REPL (the default mode).
fn run_repl() {
    println!("======================================");
    println!("      Patria OLAP Engine v0.1.0       ");
    println!("======================================");

    let db_file = "database.bin";
    let mut catalog = if Path::new(db_file).exists() {
        println!("Loading existing database from '{}'...", db_file);
        Catalog::load_from_disk(db_file)
    } else {
        println!("Starting fresh in-memory database.");
        Catalog::new()
    };

    println!("Type .help for instructions, or .exit to quit.\n");
    let stdin = io::stdin();
    let mut input = String::new();

    loop {
        print!("olap> ");
        io::stdout().flush().unwrap();

        input.clear();
        if stdin.read_line(&mut input).is_err() {
            break;
        }

        let line = input.trim();
        if line.is_empty() { continue; }

        // If the user types .exit, we handle it here to break the loop
        if line == ".exit" || line == ".quit" {
            println!("Saving database to disk...");
            catalog.save_to_disk(db_file);
            println!("Goodbye!");
            break;
        }

                        // Otherwise, process the command! Output goes to stdout for the CLI.
        let mut out = String::new();
        let _ = process_command(&mut catalog, line, &mut out);
        print!("{}", out);
    }
}

/// Headless mode: run a script against a fresh in-memory catalog, then route
/// the captured output to stdout, a file, and/or a golden-file comparison.
/// Returns a process exit code so CI can detect failures.
fn run_batch(script: &str, args: &CliArgs) -> ExitCode {
    // Always start from a clean, empty catalog so runs are hermetic and
    // independent of any database.bin lying around.
    let mut catalog = Catalog::new();

    let mut output = String::new();
    let script_ok = run_script(&mut catalog, script, &mut output);

    // Determine what to write to the output file (default stdout).
    let file_text = strip_timing(&output);

    match &args.out {
        Some(path) => {
            match File::create(path) {
                Ok(mut f) => { let _ = f.write_all(file_text.as_bytes()); }
                Err(e) => {
                    eprintln!("Could not write output file '{}': {}", path, e);
                    return ExitCode::FAILURE;
                }
            }
        }
        None => {
            print!("{}", output);
        }
    }

    // Optional golden-file handling.
    if let Some(golden_path) = &args.golden {
        if args.bless {
            match File::create(golden_path) {
                Ok(mut f) => {
                    let _ = f.write_all(file_text.as_bytes());
                    eprintln!("Blessed golden file '{}'.", golden_path);
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    eprintln!("Could not write golden file '{}': {}", golden_path, e);
                    return ExitCode::FAILURE;
                }
            }
        }

        let expected = match std::fs::read_to_string(golden_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Could not read golden file '{}': {}", golden_path, e);
                return ExitCode::FAILURE;
            }
        };

        let expected_norm = expected.replace("\r\n", "\n");
        if expected_norm == file_text {
            eprintln!("Golden match: {}", golden_path);
            if script_ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        } else {
            eprintln!("Golden MISMATCH: {}", golden_path);
            eprintln!("--- expected (golden) ---");
            eprint!("{}", expected_norm);
            eprintln!("--- actual ---");
            eprint!("{}", file_text);
            ExitCode::FAILURE
        }
    } else if script_ok {
        ExitCode::SUCCESS
    } else {
        eprintln!("One or more commands failed.");
        ExitCode::FAILURE
    }
}

/// Splits a meta-command line into tokens, honoring single/double quotes so
/// member names may contain spaces. Examples:
///   .rollup Geography "North America" USA 1.0
///   .tree   Geography "North America"
/// Quotes are stripped from the returned tokens.
fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut in_token = false;

    for c in line.chars() {
        match quote {
            Some(q) => {
                if c == q { quote = None; } else { current.push(c); }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    in_token = true;
                } else if c.is_whitespace() {
                    if in_token {
                        tokens.push(std::mem::take(&mut current));
                        in_token = false;
                    }
                } else {
                    current.push(c);
                    in_token = true;
                }
            }
        }
    }
    if in_token {
        tokens.push(current);
    }
    tokens
}

/// Evaluates a single line of input (either SQL or a .meta command).
/// All human-readable output is appended to `out` instead of printed directly,
/// so the same command pipeline can drive both the CLI and batch/script runs.
/// Returns `false` if the command reported an error (so batch mode can set an exit code).
fn process_command(catalog: &mut Catalog, line: &str, out: &mut String) -> bool {
    let start_time = Instant::now(); // Start the stopwatch
    let mut ok = true;

    if line.starts_with('.') {
        let parts: Vec<String> = tokenize(line);
        let parts: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();
        match parts[0] {
            ".help" => {
                let _ = writeln!(out, "Available Commands:");
                let _ = writeln!(out, "  .cubes                       - List all cubes in the catalog");
                let _ = writeln!(out, "  .dimensions                  - List all dimensions in the catalog");
                let _ = writeln!(out, "  .import <file.csv> <cube>    - Import data from CSV");
                let _ = writeln!(out, "  .rollup <dim> <p> <c> <wt>   - Create parent/child hierarchy relation");
                let _ = writeln!(out, "  .detach <dim> <p> <c>        - Remove a parent/child hierarchy relation");
                let _ = writeln!(out, "  .delete_member <dim> <member>- Delete a member and purge its data");
                let _ = writeln!(out, "  .splash [ADD] <cube> <val> <m...> - Allocate a value to leaf descendants");
                let _ = writeln!(out, "  .order <dim> [<m> front | <m> before <r>] - Set/show member display order");
                let _ = writeln!(out, "  .run <script.sql>            - Run a batch script of commands");
                let _ = writeln!(out, "  .save                        - Save database to disk");
                let _ = writeln!(out, "  .exit / .quit                - Save database and exit");
            }
            ".save" => {
                catalog.save_to_disk("database.bin");
                let _ = writeln!(out, "Database saved.");
            }
            ".cubes" => {
                let _ = writeln!(out, "Cubes in Catalog:");
                for (name, cube) in &catalog.cubes {
                    let m_dim = cube.measure_dimension.clone().unwrap_or_else(|| "None".to_string());
                    let cube_type = if cube.is_aggregating { "Transactional" } else { "Attribute" };
                    let _ = writeln!(out, "  - {} [{}] (Dims: {:?}) [Measure Dim: {}]", name, cube_type, cube.dimension_names, m_dim);
                }
            }
            
                        ".cube" => {
                // Usage: .cube Financials
                if parts.len() == 2 {
                    let cube_name = parts[1];
                    if let Some(cube) = catalog.get_cube(cube_name) {
                        let _ = writeln!(out, "======================================");
                        let _ = writeln!(out, "Cube '{}' State Dump", cube.name);
                        let _ = writeln!(out, "======================================");

                        let c_type = if cube.is_aggregating { "Transactional (Aggregates)" } else { "Attribute (No Math)" };
                        let _ = writeln!(out, "Type: {}", c_type);

                        let m_dim = cube.measure_dimension.clone().unwrap_or_else(|| "None".to_string());
                        let _ = writeln!(out, "Measure Dimension: {}", m_dim);

                        let _ = writeln!(out, "Dimensions ({}):", cube.dimension_names.len());
                        for (i, dim_name) in cube.dimension_names.iter().enumerate() {
                            let dim = cube.dimensions[i].read().unwrap();
                            let is_m = if Some(dim_name) == cube.measure_dimension.as_ref() { "[MEASURE]" } else { "" };
                            let _ = writeln!(out, "  [{}] {} (Size: {}) {}", i, dim.name, dim.len(), is_m);
                        }

                        let _ = writeln!(out, "Raw Trie Data:");
                        cube.store.print_tree(out);

                                                let _ = writeln!(out, "======================================");
                    } else {
                        let _ = writeln!(out, "Error: Cube '{}' not found.", cube_name);
                        ok = false;
                    }
                } else {
                    let _ = writeln!(out, "Usage: .cube <name>");
                    ok = false;
                }
            }            
			//IMPORT Commands
			".import" => {
                if parts.len() == 3 {
                                        if let Some(cube) = catalog.get_cube_mut(parts[2]) {
                        match cube.import_csv(parts[1], true) {
                            Ok(msg) => { let _ = writeln!(out, "{}", msg); }
                            Err(_) => { let _ = writeln!(out, "Import failed for '{}'.", parts[1]); ok = false; }
                        }
                    } else {
                        let _ = writeln!(out, "Cube not found.");
                        ok = false;
                    }
                } else {
                    let _ = writeln!(out, "Usage: .import <filepath> <cube>");
                    ok = false;
                }
            }		
						".import_pc" => {
                // Format: Parent, Child, Weight
                                if parts.len() == 3 {
                    match csv::ReaderBuilder::new().has_headers(true).flexible(true).from_path(parts[1]) {
                        Ok(mut rdr) => {
                            let dim_arc = catalog.get_or_create_dimension(parts[2]);
                            let mut count = 0;
                            let mut failed = false;

                            for result in rdr.records() {
                                let record = match result {
                                    Ok(r) => r,
                                    Err(e) => {
                                        let _ = writeln!(out, "Import failed: bad CSV row: {}", e);
                                        ok = false;
                                        failed = true;
                                        break;
                                    }
                                };
                                if record.len() >= 3 {
                                    let parent = &record[0];
                                    let child = &record[1];
                                    let weight: f64 = record[2].parse().unwrap_or(1.0);
                                    dim_arc.write().unwrap().add_component(parent, child, weight);
                                    count += 1;
                                }
                            }

                            if !failed {
                                catalog.clear_all_caches();
                                let _ = writeln!(out, "Imported {} parent-child relations into '{}'.", count, parts[2]);
                            }
                        }
                                                Err(_) => {
                            let _ = writeln!(out, "Could not open '{}'.", parts[1]);
                            ok = false;
                        }
                    }
                } else {
                    let _ = writeln!(out, "Usage: .import_pc <file.csv> <dimension>");
                    ok = false;
                }
            }
            ".import_lvl" => {
                // Format: Level1, Level2, Level3 (e.g., Europe, France, Paris)
                                if parts.len() == 3 {
                    match csv::ReaderBuilder::new().has_headers(true).flexible(true).from_path(parts[1]) {
                        Ok(mut rdr) => {
                            let dim_arc = catalog.get_or_create_dimension(parts[2]);
                            let mut count = 0;
                            let mut failed = false;

                            for result in rdr.records() {
                                let record = match result {
                                    Ok(r) => r,
                                    Err(e) => {
                                        let _ = writeln!(out, "Import failed: bad CSV row: {}", e);
                                        ok = false;
                                        failed = true;
                                        break;
                                    }
                                };
                                let mut dim = dim_arc.write().unwrap();

                                // Link each non-empty level to the NEXT non-empty level
                                // to its right, ignoring blanks. This supports ragged
                                // hierarchies where shorter paths simply stop early, and
                                // messy account structures where a value may appear at a
                                // "shallower" column because an intermediate level was blank.
                                //   All,Europe,France,Paris -> All>Europe>France>Paris
                                //   All,,France,Paris       -> All>France>Paris
                                //   Europe,France,,         -> Europe>France (France is a leaf)
                                let non_empty: Vec<&str> = record.iter()
                                    .map(|s| s.trim())
                                    .filter(|s| !s.is_empty())
                                    .collect();

                                for pair in non_empty.windows(2) {
                                    let parent = pair[0];
                                    let child = pair[1];
                                    dim.add_component(parent, child, 1.0);
                                    count += 1;
                                }
                            }

                            if !failed {
                                catalog.clear_all_caches();
                                let _ = writeln!(out, "Imported {} level relations into '{}'.", count, parts[2]);
                            }
                        }
                                                Err(_) => {
                            let _ = writeln!(out, "Could not open '{}'.", parts[1]);
                            ok = false;
                        }
                    }
                } else {
                    let _ = writeln!(out, "Usage: .import_lvl <file.csv> <dimension>");
                    ok = false;
                }
            }
			
                                                ".rollup" => {
                if parts.len() == 5 {
                    if let Ok(weight) = parts[4].parse::<f64>() {
                        let dim_arc = catalog.get_or_create_dimension(parts[1]);
                        let moved = dim_arc.write().unwrap().add_component(parts[2], parts[3], weight);
                        catalog.clear_all_caches();
                        match moved {
                            Some(old_parent) => {
                                // One-parent-per-hierarchy: re-parenting auto-detached it.
                                let _ = writeln!(
                                    out,
                                    "Rollup added. '{}' moved from '{}' to '{}' (one parent per hierarchy).",
                                    parts[3], old_parent, parts[2]
                                );
                            }
                            None => { let _ = writeln!(out, "Rollup added."); }
                        }
                    } else {
                        let _ = writeln!(out, "Error: weight '{}' is not a number.", parts[4]);
                        ok = false;
                    }
                                } else {
                    let _ = writeln!(out, "Usage: .rollup <dimension> <parent> <child> <weight>");
                    let _ = writeln!(out, "       (quote names that contain spaces)");
                    ok = false;
                }
            }

            // Inverse of .rollup: detach a child from its parent. The child
            // stays in the dimension; only the relationship is removed.
            ".detach" => {
                if parts.len() == 4 {
                    let dim_arc = catalog.get_or_create_dimension(parts[1]);
                    let res = dim_arc.write().unwrap().remove_component(parts[2], parts[3]);
                    match res {
                        Ok(()) => { catalog.clear_all_caches(); let _ = writeln!(out, "Detached '{}' from '{}'.", parts[3], parts[2]); }
                        Err(e) => { let _ = writeln!(out, "Error: {}", e); ok = false; }
                    }
                } else {
                    let _ = writeln!(out, "Usage: .detach <dimension> <parent> <child>");
                    ok = false;
                }
            }

            // Delete a member ENTIRELY (a big operation): removed from the
            // dimension and all referencing data purged from every cube.
            ".delete_member" | ".drop_member" => {
                if parts.len() == 3 {
                    match catalog.delete_member(parts[1], parts[2]) {
                        Ok(msg) => { let _ = writeln!(out, "{}", msg); }
                        Err(e) => { let _ = writeln!(out, "Error: {}", e); ok = false; }
                    }
                } else {
                    let _ = writeln!(out, "Usage: .delete_member <dimension> <member>");
                    ok = false;
                }
            }

            ".splash" => {
                // Data allocation: distribute a value to the leaf descendants of
                // any consolidated coordinate.
                //
                // Syntax: .splash [ADD] <cube> <value> <m1> <m2> ... <mN>
                //   * Use '*' for any dimension to fall back to its default member.
                //   * 'ADD' (optional, right after .splash) spreads the value on
                //     top of existing leaves instead of overwriting them.
                //
                // Example:
                //   .splash Financials 1200 Jan Europe Actuals Sales
                //   .splash ADD Financials 100 Jan * Actuals Sales
                let mut rest = &parts[1..];
                let mode = if rest.first() == Some(&"ADD") {
                    rest = &rest[1..];
                    olap_engine::cube::cube::SplashMode::Add
                } else {
                    olap_engine::cube::cube::SplashMode::Replace
                };

                if rest.len() < 3 {
                    let _ = writeln!(out, "Usage: .splash [ADD] <cube> <value> <m1> [<m2> ...]");
                    let _ = writeln!(out, "       Use '*' to select a dimension's default member.");
                    ok = false;
                } else {
                    let cube_name = rest[0];
                    let value: Option<f64> = rest[1].parse().ok();

                    match value {
                        None => {
                            let _ = writeln!(out, "Error: '{}' is not a number.", rest[1]);
                            ok = false;
                        }
                                                Some(val) => {
                            let member_args = &rest[2..];
                            // Resolve '*' (and omitted trailing coordinates) to a
                            // dimension's default member so the caller can omit
                            // "impractical" coordinates concisely.
                            let resolved: Result<Vec<String>, String> = match catalog.get_cube(cube_name) {
                                Some(cube) => {
                                    let mut v = Vec::with_capacity(cube.dimension_names.len());
                                    let mut resolve_err: Option<String> = None;
                                    for (i, dim_name) in cube.dimension_names.iter().enumerate() {
                                        let arg = member_args.get(i).copied().unwrap_or("*");
                                        if arg == "*" {
                                            let dim = cube.dimensions[i].read().unwrap();
                                            match dim.get_default_member_name() {
                                                Some(d) => v.push(d),
                                                None => {
                                                    resolve_err = Some(format!(
                                                        "Dimension '{}' has no default member; specify a value explicitly.",
                                                        dim_name
                                                    ));
                                                    break;
                                                }
                                            }
                                        } else {
                                            v.push(arg.to_string());
                                        }
                                    }
                                    match resolve_err {
                                        Some(e) => Err(e),
                                        None => Ok(v),
                                    }
                                }
                                None => Err(format!("Cube '{}' not found.", cube_name)),
                            };

                            match resolved {
                                Ok(v) => {
                                    let refs: Vec<&str> = v.iter().map(|s| s.as_str()).collect();
                                    if let Some(cube) = catalog.get_cube_mut(cube_name) {
                                        match cube.write_splashed(&refs, val, mode) {
                                            Ok(n) => {
                                                let verb = if mode == olap_engine::cube::cube::SplashMode::Add { "Added" } else { "Splashed" };
                                                let _ = writeln!(out, "{} {} across {} leaf cell(s) in '{}'.", verb, val, n, cube_name);
                                            }
                                            Err(e) => { let _ = writeln!(out, "Splash failed: {}", e); ok = false; }
                                        }
                                    }
                                }
                                Err(e) => { let _ = writeln!(out, "Splash failed: {}", e); ok = false; }
                            }
                        }
                    }
                }
            }

			            // Design-time member ordering. The dimension's display order is
            // persisted and used to order rows in run-time query results.
            ".order" => {
                // .order <dimension>                        -> show current order
                // .order <dimension> <member> front         -> move member to front
                // .order <dimension> <member> before <ref>  -> move member before <ref>
                if parts.len() < 2 {
                    let _ = writeln!(out, "Usage: .order <dimension> [<member> front | <member> before <ref>]");
                    ok = false;
                } else {
                    let dim_name = parts[1];
                    let dim_arc = catalog.dimensions.get(dim_name).cloned();
                    match dim_arc {
                        None => {
                            let _ = writeln!(out, "Error: Dimension '{}' not found.", dim_name);
                            ok = false;
                        }
                                                Some(arc) => {
                            if parts.len() == 2 {
                                let mut dim = arc.write().unwrap();
                                dim.ensure_display_order();
                                let _ = writeln!(out, "Display order for '{}':", dim.name);
                                for (i, name) in dim.member_order_names().iter().enumerate() {
                                    let _ = writeln!(out, "  {}. {}", i + 1, name);
                                }
                            } else if parts.len() == 4 && parts[3] == "front" {
                                let mut dim = arc.write().unwrap();
                                match dim.move_member_to_front(parts[2]) {
                                    Ok(()) => { let _ = writeln!(out, "Moved '{}' to front in '{}'.", parts[2], dim.name); }
                                    Err(e) => { let _ = writeln!(out, "Error: {}", e); ok = false; }
                                }
                                catalog.clear_all_caches();
                            } else if parts.len() == 5 && parts[3] == "before" {
                                let mut dim = arc.write().unwrap();
                                match dim.move_member_before(parts[2], parts[4]) {
                                    Ok(()) => { let _ = writeln!(out, "Moved '{}' before '{}' in '{}'.", parts[2], parts[4], dim.name); }
                                    Err(e) => { let _ = writeln!(out, "Error: {}", e); ok = false; }
                                }
                                catalog.clear_all_caches();
                            } else {
                                let _ = writeln!(out, "Usage: .order <dimension> [<member> front | <member> before <ref>]");
                                ok = false;
                            }
                        }
                    }
                }
            }

			".dimensions" => {
                let _ = writeln!(out, "Dimensions in Catalog:");
                for (name, dim_arc) in &catalog.dimensions {
                    let dim = dim_arc.read().unwrap();
                    let _ = writeln!(out, "  - {} ({} members)", name, dim.len());
                }
            }
			
            ".tree" => {
                // Usage: .tree Geography Global
                if parts.len() == 3 {
                    let dim_name = parts[1];
                    let member = parts[2];

                    if let Some(dim_arc) = catalog.dimensions.get(dim_name) {
                        let _ = writeln!(out, "Hierarchy for '{}' in {}:", member, dim_name);
                        dim_arc.read().unwrap().print_tree(member, 0, 1.0, out);
                    } else {
                        let _ = writeln!(out, "Error: Dimension '{}' not found.", dim_name);
                    }
                } else {
                    let _ = writeln!(out, "Usage: .tree <dimension> <root_member>");
                    let _ = writeln!(out, "Example: .tree Geography Global");
                }
            }
			
									".run" => {
                if parts.len() == 2 {
                    ok = run_script(catalog, parts[1], out);
                } else {
                    let _ = writeln!(out, "Usage: .run <filepath>");
                    ok = false;
                }
            }
            _ => { let _ = writeln!(out, "Unknown meta-command."); ok = false; }
        }
    } else {
        // It's a SQL Command
        match execute_sql(catalog, line) {
            Ok(msg) => { let _ = writeln!(out, "{}", msg); }
            Err(e) => { let _ = writeln!(out, "Error: {}", e); ok = false; }
        }
    }

    // Stop the stopwatch and append the execution time
    let elapsed = start_time.elapsed();
    let _ = writeln!(out, "(Time: {:?})\n", elapsed);

    ok
}

/// Reads a file line-by-line and feeds it into the engine.
/// Output is appended to `out` (stdout for the CLI, or a buffer/file for tests).
/// Returns `false` if the file could not be read or any command errored.
fn run_script(catalog: &mut Catalog, filepath: &str, out: &mut String) -> bool {
    let file = match File::open(filepath) {
        Ok(f) => f,
        Err(e) => {
            let _ = writeln!(out, "Could not open script: {}", e);
            return false;
        }
    };

        let reader = io::BufReader::new(file);
    let mut line_count = 0;
    let mut all_ok = true;

    let _ = writeln!(out, "Running script '{}'...", filepath);
    let script_start = Instant::now();

    for (idx, line_result) in reader.lines().enumerate() {
        let mut line = line_result.unwrap_or_default();
        // Strip a UTF-8 BOM that Windows editors/tools often prepend, which
        // would otherwise corrupt the very first SQL statement.
        if idx == 0 {
            line = line.trim_start_matches('\u{feff}').to_string();
        }
        let trimmed = line.trim();

        // Skip empty lines and SQL comments
        if trimmed.is_empty() || trimmed.starts_with("--") {
            continue;
        }

        let _ = writeln!(out, "> {}", trimmed);
        if !process_command(catalog, trimmed, out) {
            all_ok = false;
        }
        line_count += 1;
    }

    let _ = writeln!(out, "Script finished. Executed {} commands in {:?}.", line_count, script_start.elapsed());
    all_ok
}
