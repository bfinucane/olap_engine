use std::io::{self, BufRead, Write};
use std::fs::File;
use std::path::Path;
use std::time::Instant; // Adds precision timing!

use olap_engine::catalog::catalog::Catalog;
use olap_engine::engine::sql::execute_sql;

fn main() {
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

        // Otherwise, process the command!
        process_command(&mut catalog, line);
    }
}

/// Evaluates a single line of input (either SQL or a .meta command)
fn process_command(catalog: &mut Catalog, line: &str) {
    let start_time = Instant::now(); // Start the stopwatch

    if line.starts_with('.') {
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts[0] {
            ".help" => {
                println!("Available Commands:");
                println!("  .cubes                       - List all cubes in the catalog");
				println!("  .dimensions                  - List all dimensions in the catalog");
                println!("  .import <file.csv> <cube>    - Import data from CSV");
                println!("  .rollup <dim> <p> <c> <wt>   - Create parent/child hierarchy relation");
                println!("  .run <script.sql>            - Run a batch script of commands");
                println!("  .save                        - Save database to disk");
                println!("  .exit / .quit                - Save database and exit");
            }
            ".save" => {
                catalog.save_to_disk("database.bin");
                println!("Database saved.");
            }
            ".cubes" => {
                println!("Cubes in Catalog:");
                for (name, cube) in &catalog.cubes {
                    let m_dim = cube.measure_dimension.clone().unwrap_or_else(|| "None".to_string());
                    let cube_type = if cube.is_aggregating { "Transactional" } else { "Attribute" };
                    println!("  - {} [{}] (Dims: {:?}) [Measure Dim: {}]", name, cube_type, cube.dimension_names, m_dim);
                }
            }
            
            ".cube" => {
                // Usage: .cube Financials
                if parts.len() == 2 {
                    let cube_name = parts[1];
                    if let Some(cube) = catalog.get_cube(cube_name) {
                        println!("======================================");
                        println!("Cube '{}' State Dump", cube.name);
                        println!("======================================");
                        
                        let c_type = if cube.is_aggregating { "Transactional (Aggregates)" } else { "Attribute (No Math)" };
                        println!("Type: {}", c_type);
                        
                        let m_dim = cube.measure_dimension.clone().unwrap_or_else(|| "None".to_string());
                        println!("Measure Dimension: {}", m_dim);
                        
                        println!("Dimensions ({}):", cube.dimension_names.len());
                        for (i, dim_name) in cube.dimension_names.iter().enumerate() {
                            let dim = cube.dimensions[i].read().unwrap();
                            let is_m = if Some(dim_name) == cube.measure_dimension.as_ref() { "[MEASURE]" } else { "" };
                            println!("  [{}] {} (Size: {}) {}", i, dim.name, dim.len(), is_m);
                        }

                        println!("Raw Trie Data:");
                        cube.store.print_tree();
                        
                        println!("======================================");
                    } else {
                        println!("Error: Cube '{}' not found.", cube_name);
                    }
                } else {
                    println!("Usage: .cube <name>");
                }
            }            
			//IMPORT Commands
			".import" => {
                if parts.len() == 3 {
                    if let Some(cube) = catalog.get_cube_mut(parts[2]) {
                        match cube.import_csv(parts[1], true) {
                            Ok(_) => println!("Import successful."),
                            Err(e) => println!("Import failed: {}", e),
                        }
                    } else {
                        println!("Cube not found.");
                    }
                } else {
                    println!("Usage: .import <filepath> <cube>");
                }
            }		
			".import_pc" => {
                // Format: Parent, Child, Weight
                if parts.len() == 3 {
                    let dim_arc = catalog.get_or_create_dimension(parts[2]);
                    let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_path(parts[1]).unwrap();
                    let mut count = 0;
                    
                    for result in rdr.records() {
                        let record = result.unwrap();
                        if record.len() >= 3 {
                            let parent = &record[0];
                            let child = &record[1];
                            let weight: f64 = record[2].parse().unwrap_or(1.0);
                            
                            dim_arc.write().unwrap().add_component(parent, child, weight);
                            count += 1;
                        }
                    }
                    catalog.clear_all_caches();
                    println!("Imported {} parent-child relations into '{}'.", count, parts[2]);
                } else {
                    println!("Usage: .import_pc <file.csv> <dimension>");
                }
            }
            ".import_lvl" => {
                // Format: Level1, Level2, Level3 (e.g., Europe, France, Paris)
                if parts.len() == 3 {
                    let dim_arc = catalog.get_or_create_dimension(parts[2]);
                    let mut rdr = csv::ReaderBuilder::new().has_headers(true).from_path(parts[1]).unwrap();
                    let mut count = 0;

                    for result in rdr.records() {
                        let record = result.unwrap();
                        let mut dim = dim_arc.write().unwrap();
                        
                        // Link each column to the column to its right
                        for i in 0..(record.len() - 1) {
                            let parent = &record[i];
                            let child = &record[i + 1];
                            
                            // Skip empty strings (handles ragged blanks in level-based CSVs)
                            if parent.is_empty() || child.is_empty() { continue; }
                            
                            dim.add_component(parent, child, 1.0);
                            count += 1;
                        }
                    }
                    catalog.clear_all_caches();
                    println!("Imported {} level relations into '{}'.", count, parts[2]);
                } else {
                    println!("Usage: .import_lvl <file.csv> <dimension>");
                }
            }
			
            ".rollup" => {
                if parts.len() == 5 {
                    if let Ok(weight) = parts[4].parse::<f64>() {
                        let dim_arc = catalog.get_or_create_dimension(parts[1]);
                        dim_arc.write().unwrap().add_component(parts[2], parts[3], weight);
                        catalog.clear_all_caches();
                        println!("Rollup added.");
                    }
                }
            }
            
			".dimensions" => {
                println!("Dimensions in Catalog:");
                for (name, dim_arc) in &catalog.dimensions {
                    let dim = dim_arc.read().unwrap();
                    println!("  - {} ({} members)", name, dim.len());
                }
            }
			
            ".tree" => {
                // Usage: .tree Geography Global
                if parts.len() == 3 {
                    let dim_name = parts[1];
                    let member = parts[2];

                    if let Some(dim_arc) = catalog.dimensions.get(dim_name) {
                        println!("Hierarchy for '{}' in {}:", member, dim_name);
                        dim_arc.read().unwrap().print_tree(member, 0, 1.0);
                    } else {
                        println!("Error: Dimension '{}' not found.", dim_name);
                    }
                } else {
                    println!("Usage: .tree <dimension> <root_member>");
                    println!("Example: .tree Geography Global");
                }
            }
			
			".run" => {
                if parts.len() == 2 {
                    run_script(catalog, parts[1]);
                } else {
                    println!("Usage: .run <filepath>");
                }
            }
            _ => println!("Unknown meta-command."),
        }
    } else {
        // It's a SQL Command
        match execute_sql(catalog, line) {
            Ok(msg) => println!("{}", msg),
            Err(e) => println!("Error: {}", e),
        }
    }

    // Stop the stopwatch and print the execution time
    let elapsed = start_time.elapsed();
    println!("(Time: {:?})\n", elapsed);
}

/// Reads a file line-by-line and feeds it into the engine
fn run_script(catalog: &mut Catalog, filepath: &str) {
    let file = match File::open(filepath) {
        Ok(f) => f,
        Err(e) => {
            println!("Could not open script: {}", e);
            return;
        }
    };

    let reader = io::BufReader::new(file);
    let mut line_count = 0;

    println!("Running script '{}'...", filepath);
    let script_start = Instant::now();

    for line_result in reader.lines() {
        let line = line_result.unwrap_or_default();
        let trimmed = line.trim();

        // Skip empty lines and SQL comments
        if trimmed.is_empty() || trimmed.starts_with("--") {
            continue;
        }

        println!("> {}", trimmed);
        process_command(catalog, trimmed);
        line_count += 1;
    }

    println!("Script finished. Executed {} commands in {:?}.", line_count, script_start.elapsed());
}