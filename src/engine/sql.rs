use sqlparser::ast::{Expr, SetExpr, Statement, BinaryOperator};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;
use std::collections::HashMap; 

use crate::cube::node::CellValue;

use crate::catalog::catalog::Catalog;
use crate::cube::cube::{SliceQuery, ResultSet}; 

// Changed return type from f64 to String to support textual success messages
pub fn execute_sql(catalog: &mut Catalog, sql_query: &str) -> Result<String, String> {
	    // 1. Intercept custom "ATTRIBUTE" keyword
    let mut is_aggregating = true;
    let mut parseable_sql = sql_query.trim().to_string();
    let upper_sql = parseable_sql.to_uppercase();

    if upper_sql.starts_with("CREATE ATTRIBUTE TABLE") {
        is_aggregating = false;
        // Slice off "CREATE ATTRIBUTE TABLE" (22 chars) and prepend standard SQL
        parseable_sql = format!("CREATE TABLE {}", &sql_query.trim()[22..]);
	}
    
    let dialect = GenericDialect {};
    
    let ast = Parser::parse_sql(&dialect, &parseable_sql)
        .map_err(|e| format!("SQL Parse Error: {:?}", e))?;

    let statement = &ast[0];
    
    match statement {
// 1. SELECT (Query)
// 1. SELECT (Query)
        Statement::Query(query) => {
            if let SetExpr::Select(select) = &*query.body {
                let cube_name = select.from[0].relation.to_string();
                let cube = catalog.get_cube_mut(&cube_name)
                    .ok_or_else(|| format!("Cube '{}' not found", cube_name))?;

                let mut output_columns = Vec::new();
                let mut axes = Vec::new();
                let mut requested_measures = Vec::new();
                let mut measure_dim_requested = false;

                let m_dim_name = cube.measure_dimension.clone();

                // Pass 1: Categorize columns
                for proj in &select.projection {
                    let col_name = proj.to_string();
                    output_columns.push(col_name.clone());

                    if cube.dimension_names.contains(&col_name) {
                        axes.push(col_name.clone());
                        if Some(&col_name) == m_dim_name.as_ref() {
                            measure_dim_requested = true;
                        }
                    } else if let Some(m_dim) = &m_dim_name {
                        // Check if it's a member of the measure dimension
                        let dim_idx = cube.dimension_names.iter().position(|n| n == m_dim).unwrap();
                        let dim = cube.dimensions[dim_idx].read().unwrap();
                        if dim.get_id(&col_name).is_some() {
                            requested_measures.push(col_name);
                        } else if col_name != "value" {
                            return Err(format!("Column '{}' not found.", col_name));
                        }
                    } else if col_name != "value" {
                        return Err(format!("Column '{}' not found.", col_name));
                    }
                }

                // Pass 2: Conflict Validation
                if measure_dim_requested && !requested_measures.is_empty() {
                    return Err("Cannot select both the measure dimension and specific measures.".to_string());
                }

                // Parse WHERE clause
                let mut filters = HashMap::new();
                if let Some(selection) = &select.selection {
                    extract_where_map(selection, &mut filters)?;
                }

                if !requested_measures.is_empty() {
                    if let Some(m_dim) = &m_dim_name {
                        if filters.contains_key(m_dim) {
                            return Err(format!("Cannot filter on '{}' when pivoting specific measures.", m_dim));
                        }
                    }
                } else if !measure_dim_requested && axes.is_empty() {
                    // Fallback for single cell query
                    if !output_columns.contains(&"value".to_string()) {
                        output_columns.push("value".to_string());
                    }
                }

                // Execute!
                let slice_query = SliceQuery { output_columns, axes, requested_measures, filters };
                match cube.query_slice(&slice_query) {
                    Ok(result_set) => return Ok(format_result_set(result_set)),
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
            let mut dim_names = Vec::new(); 
			
			// No longer use this to get dims: columns.iter().map(|c| c.name.to_string()).collect();

			// Store the measure dimension name here if we find it
            let mut measure_dim: Option<String> = None;
			
			for col in columns {
                let col_name = col.name.to_string();
                let data_type = col.data_type.to_string().to_uppercase();

                // If the user specified MEASURE in the SQL, we save it
                if data_type == "MEASURE" {
                    measure_dim = Some(col_name.clone());
                }
                
                dim_names.push(col_name);
            }

            let dim_refs: Vec<&str> = dim_names.iter().map(|s| s.as_str()).collect();
			


            // The Catalog creates the dimensions automatically if they don't exist
            catalog.add_cube(&cube_name, &dim_refs, measure_dim.as_deref(), is_aggregating);
			
			let type_str = if is_aggregating { "Transactional" } else { "Attribute" };
            
			// Return a nice message to the shell
            let msg = match measure_dim {
                Some(m) => format!("Cube '{}' created. Measure dimension: {}", cube_name, m),
                None => format!("Cube '{}' created with no explicit measure dimension.", cube_name),
            };
            
            Ok(msg)
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
                        let measure_val = match measure_str.parse::<f64>() {
                            Ok(n) => CellValue::Numeric(n),
                            Err(_) => CellValue::String(measure_str.replace("'", "")),
                        };

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
    if rs.rows.is_empty() { return "0 rows returned.".to_string(); }
    let mut output = String::new();
    
    output.push_str(&rs.headers.join(" | "));
    output.push('\n');
    output.push_str(&"-".repeat(rs.headers.len() * 10));
    output.push('\n');

    for row in rs.rows {
        output.push_str(&row.join(" | "));
        output.push('\n');
    }
    output
}