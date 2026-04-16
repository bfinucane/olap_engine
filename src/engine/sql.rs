use sqlparser::ast::{Expr, SetExpr, Statement, BinaryOperator};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;
use std::collections::HashMap; 

use crate::catalog::catalog::Catalog;
use crate::cube::cube::{SliceQuery, ResultSet}; 

// Changed return type from f64 to String to support textual success messages
pub fn execute_sql(catalog: &mut Catalog, sql_query: &str) -> Result<String, String> {
    let dialect = GenericDialect {};
    
    let ast = Parser::parse_sql(&dialect, sql_query)
        .map_err(|e| format!("SQL Parse Error: {:?}", e))?;

    let statement = &ast[0];
    
    match statement {
// 1. SELECT (Query)
        Statement::Query(query) => {
            if let SetExpr::Select(select) = &*query.body {
                let cube_name = select.from[0].relation.to_string();
                let cube = catalog.get_cube_mut(&cube_name)
                    .ok_or_else(|| format!("Cube '{}' not found", cube_name))?;

                // Extract the columns the user wants to group by (Axes)
                let mut axes = Vec::new();
                for proj in &select.projection {
                    let col_name = proj.to_string();
                    if col_name != "value" && col_name != "*" { // 'value' is our measure column
                        axes.push(col_name);
                    }
                }

                // Extract the WHERE clause filters
                let mut filters = HashMap::new();
                if let Some(selection) = &select.selection {
                    extract_where_map(selection, &mut filters)?;
                }

                // If they ONLY asked for 'value', we execute the old cell-based query
                if axes.is_empty() {
                    let mut final_query = Vec::new();
                    for dim in &cube.dimension_names {
                        match filters.get(dim) {
                            Some(val) => final_query.push(val.as_str()),
                            None => return Err(format!("Dimension '{}' must be specified if not selected as an axis", dim)),
                        }
                    }
                    let result = cube.query_consolidated(&final_query);
                    return Ok(result.to_string());
                }

                // Otherwise, they asked for a Slice (Table output)!
                let slice_query = SliceQuery { axes, filters };
                match cube.query_slice(&slice_query) {
                    Ok(result_set) => {
                        // Format the ResultSet into a nice ASCII table string
                        return Ok(format_result_set(result_set));
                    }
                    Err(e) => return Err(e),
                }
            }
            Err("Unsupported SELECT format.".to_string())
        }

        // 2. CREATE TABLE
        // Syntax: CREATE TABLE Sales (Geography, Product)
        Statement::CreateTable { name, columns, .. } => {
            let cube_name = name.to_string();
            
            // Extract the column names (which act as our Dimensions)
            let dim_names: Vec<String> = columns.iter().map(|c| c.name.to_string()).collect();
            let dim_refs: Vec<&str> = dim_names.iter().map(|s| s.as_str()).collect();

            // The Catalog creates the dimensions automatically if they don't exist
            catalog.add_cube(&cube_name, &dim_refs);
            
            Ok(format!("Cube '{}' created with dimensions: {:?}", cube_name, dim_names))
        }

        // 3. INSERT INTO (Upsert)
        // Syntax: INSERT INTO Sales VALUES ('France', 'Laptop', 100.5)
        Statement::Insert { table_name, source, .. } => {
            let cube_name = table_name.to_string();
            let cube = catalog.get_cube_mut(&cube_name)
                .ok_or_else(|| format!("Cube '{}' not found", cube_name))?;

            if let Some(query) = source {
                if let SetExpr::Values(values) = &*query.body {
                    let mut row_count = 0;

                    for row in &values.rows {
                        // Ensure we have exactly Dim_Count + 1 (the measure)
                        if row.len() != cube.dimension_names.len() + 1 {
                            return Err(format!("Expected {} values, got {}", cube.dimension_names.len() + 1, row.len()));
                        }

                        // Extract strings for the dimensions
                        let mut members = Vec::new();
                        for i in 0..cube.dimension_names.len() {
                            let val = row[i].to_string().replace("'", "");
                            members.push(val);
                        }

                        // Extract the final numeric value
                        let measure_str = row.last().unwrap().to_string();
                        let measure_val: f64 = measure_str.parse().unwrap_or(0.0);

                        // Write to the Cube (This acts as an UPSERT)
                        let member_refs: Vec<&str> = members.iter().map(|s| s.as_str()).collect();
                        cube.write(&member_refs, measure_val);
                        row_count += 1;
                    }
                    return Ok(format!("Upserted {} row(s) into '{}'.", row_count, cube_name));
                }
            }
            Err("Invalid INSERT format. Use: INSERT INTO cube VALUES ('dim1', 'dim2', 100)".to_string())
        }

        _ => Err("Unsupported SQL command.".to_string()),
    }
}

// Keep the existing extract_where_conditions helper exactly the same!
fn extract_where_conditions(
    expr: &Expr, 
    query_params: &mut Vec<Option<String>>, 
    dim_names: &[String]
) -> Result<(), String> {
    match expr {
        Expr::BinaryOp { left, op: BinaryOperator::And, right } => {
            extract_where_conditions(left, query_params, dim_names)?;
            extract_where_conditions(right, query_params, dim_names)?;
        }
        Expr::BinaryOp { left, op: BinaryOperator::Eq, right } => {
            let dim_target = left.to_string();
            let val_target = right.to_string().replace("'", "");

            if let Some(index) = dim_names.iter().position(|name| name == &dim_target) {
                query_params[index] = Some(val_target);
            } else {
                return Err(format!("Dimension '{}' does not exist in cube", dim_target));
            }
        }
        _ => return Err("Unsupported WHERE clause format".to_string()),
    }
    Ok(())
}

// Helper: Extracts WHERE Geography = 'Europe' into a HashMap{"Geography": "Europe"}
fn extract_where_map(expr: &Expr, filters: &mut HashMap<String, String>) -> Result<(), String> {
    match expr {
        Expr::BinaryOp { left, op: BinaryOperator::And, right } => {
            extract_where_map(left, filters)?;
            extract_where_map(right, filters)?;
        }
        Expr::BinaryOp { left, op: BinaryOperator::Eq, right } => {
            let dim_target = left.to_string();
            let val_target = right.to_string().replace("'", "");
            filters.insert(dim_target, val_target);
        }
        _ => return Err("Unsupported WHERE clause format. Use Dim = 'Value'".to_string()),
    }
    Ok(())
}

// Helper: Formats the ResultSet into an ASCII table
fn format_result_set(rs: ResultSet) -> String {
    if rs.rows.is_empty() {
        return "0 rows returned.".to_string();
    }

    let mut output = String::new();
    
    // Header Row
    output.push_str(&rs.headers.join(" | "));
    output.push_str(" | Value\n");
    
    // Separator line
    output.push_str(&"-".repeat(output.len() - 1));
    output.push('\n');

    // Data Rows
    for (row, value) in rs.rows {
        output.push_str(&row.join(" | "));
        output.push_str(&format!(" | {}\n", value));
    }

    output
}