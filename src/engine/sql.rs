use sqlparser::ast::{Expr, SetExpr, Statement, BinaryOperator};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;
use std::collections::HashMap; 

use crate::cube::node::CellValue;

use crate::catalog::catalog::Catalog;
use crate::cube::cube::{SliceQuery, ResultSet}; 

// Changed return type from f64 to String to support textual success messages
pub fn execute_sql(catalog: &mut Catalog, sql_query: &str) -> Result<String, String> {
	
    let mut parseable_sql = sql_query.trim().to_string();
    let upper_sql = parseable_sql.to_uppercase();
	
	// The following two interceptions allow us to use the standard SQL interpreter without modifications. 
    // 1. Intercept CREATE DIMENSION
    if upper_sql.starts_with("CREATE DIMENSION") {
        let dim_name = sql_query[16..].trim().trim_end_matches(';');
        catalog.get_or_create_dimension(dim_name);
        return Ok(format!("Dimension '{}' created successfully.", dim_name));
    }

	
	// 2. Intercept custom "ATTRIBUTE" keyword
    let mut is_aggregating = true;



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
        Statement::Query(query) => {
            if let SetExpr::Select(select) = &*query.body {
                
                                // --- 1. IDENTIFY PRIMARY AND JOINED CUBES ---
                // Get Primary Cube (e.g., Transactions)
                let relation = select.from.first()
                    .ok_or_else(|| "SELECT must include a FROM clause naming a cube.".to_string())?;
                let primary_cube_name = match &relation.relation {
                    sqlparser::ast::TableFactor::Table { name, .. } => name.to_string(),
                    _ => return Err("Unsupported FROM clause.".to_string()),
                };
                let primary_cube = catalog.get_cube(&primary_cube_name)
                    .ok_or_else(|| format!("Cube '{}' not found", primary_cube_name))?;

                // Check for JOINs (e.g., Attribute Cubes)
                let mut joined_cube_name = None;
                if !relation.joins.is_empty() {
                    if let sqlparser::ast::TableFactor::Table { name, .. } = &relation.joins[0].relation {
                        joined_cube_name = Some(name.to_string());
                    }
                }

                // --- 2. CATEGORIZE COLUMNS ---
                let mut output_columns = Vec::new();
                let mut axes = Vec::new();
                let mut requested_measures = Vec::new();
                let mut requested_attributes = Vec::new(); // NEW: Track attribute columns
                let mut measure_dim_requested = false;

                                let p_m_dim = primary_cube.measure_dimension.clone();

                for proj in &select.projection {
                    let col_raw = proj.to_string();
                    let col_name = col_raw.split('.').last().unwrap().trim().to_string();

                    output_columns.push(col_name.clone());
                    
                    let mut found = false; // Track if we successfully categorized the column

                    // 1. Is it a primary dimension?
                    if primary_cube.dimension_names.contains(&col_name) {
                        axes.push(col_name.clone());
                        if Some(&col_name) == p_m_dim.as_ref() { measure_dim_requested = true; }
                        found = true;
                    } 
                    // 2. Is it a primary measure?
                    else if let Some(m_dim) = &p_m_dim {
                                                let dim_idx = primary_cube.dimension_names.iter().position(|n| n == m_dim).unwrap();

                        let target_dim = primary_cube.dimensions[dim_idx].read().unwrap();

                        if let Some(id) = target_dim.get_id(&col_name) {
                            // FETCH THE OFFICIAL CASING FROM THE DICTIONARY!
                            requested_measures.push(target_dim.get_name(id)); 
                            found = true;
                        }
                    }
                    
                    // 3. Is it a joined attribute?
                    if !found {
                        if let Some(j_name) = &joined_cube_name {
                            if let Some(j_cube) = catalog.get_cube(j_name) {
                                if let Some(jm_dim) = &j_cube.measure_dimension {
                                    let j_dim_idx = j_cube.dimension_names.iter().position(|n| n == jm_dim).unwrap();
                                    if j_cube.dimensions[j_dim_idx].read().unwrap().get_id(&col_name).is_some() {
                                        requested_attributes.push(col_name.clone());
                                        found = true;
                                    }
                                }
                            }
                        }
                    }

                    // 4. If it wasn't found anywhere, throw the error!
                    if !found && col_name != "value" {
                        return Err(format!("Column '{}' not found in primary or joined cube.", col_name));
                    }
                }

                // --- 3. EXECUTE MAIN QUERY ---
                if measure_dim_requested && !requested_measures.is_empty() {
                    return Err("Cannot select both measure dimension and specific measures.".to_string());
                }

				let mut filters = HashMap::new();
                if let Some(selection) = &select.selection {
                    extract_where_map(selection, &mut filters)?;
                }

                // Inject Default Members for omitted dimensions
                for dim_name in &primary_cube.dimension_names {
                    // We skip the Measure Dimension (it has special pivot rules)
                    if Some(dim_name) == p_m_dim.as_ref() { continue; }
                    
                    // If it's not an Axis and not Filtered, we must default it
                    if !axes.contains(dim_name) && !filters.contains_key(dim_name) {
                        let dim_idx = primary_cube.dimension_names.iter().position(|n| n == dim_name).unwrap();
                        let dim = primary_cube.dimensions[dim_idx].read().unwrap();
                        
                        if let Some(default_name) = dim.get_default_member_name() {
                            // Inject it into the WHERE clause dynamically
                            // We use a vector because we are about to upgrade filters to handle IN (...)
                            filters.insert(dim_name.clone(), vec![default_name]);
                        }
                    }
                }

                // Fallback for single cell query
                if !requested_measures.is_empty() {

                    if let Some(m_dim) = &p_m_dim {
                        if filters.contains_key(m_dim) {
                            return Err(format!("Cannot filter on '{}' when pivoting specific measures.", m_dim));
                        }
                    }
                } else if !measure_dim_requested && axes.is_empty() {
                    if !output_columns.contains(&"value".to_string()) {
                        output_columns.push("value".to_string());
                    }
                }

                let slice_query = SliceQuery { 
                    output_columns: output_columns.clone(), 
                    axes, 
                    requested_measures, 
                    filters 
                };
                
                let mut result_set = primary_cube.query_slice(&slice_query)?;

 
                // --- 4. ENRICH WITH JOINED ATTRIBUTES (BULK METHOD) ---
                if let Some(j_name) = &joined_cube_name {
                    if !requested_attributes.is_empty() {
                        let j_cube = catalog.get_cube(j_name).unwrap();
                        
                        // Enforce Rule 1: Must share exactly the same dimension object
                        let shared_dim = primary_cube.dimension_names.iter()
                            .find(|d| j_cube.dimension_names.contains(d))
                            .ok_or_else(|| "JOIN strictly requires a Conformed Dimension (shared name) between cubes.".to_string())?;

                        let shared_col_idx = result_set.headers.iter().position(|h| h == shared_dim)
                            .ok_or_else(|| format!("Shared dimension '{}' must be in SELECT to join attributes.", shared_dim))?;

                        // Rule 3: Bulk Request! 
                        // We query the Attribute Cube ONCE for all requested attributes.
                        // Combine the shared dimension and the requested attributes into the output
                        let mut attr_outputs = vec![shared_dim.clone()];
                        attr_outputs.extend(requested_attributes.clone());

                        let attr_slice = SliceQuery {
                            output_columns: attr_outputs, // Include the attributes!
                            axes: vec![shared_dim.clone()],
                            requested_measures: requested_attributes.clone(),
                            filters: HashMap::new(), 
                        };
                        
                        let attr_results = j_cube.query_slice(&attr_slice)?;
                        
                        // Build an O(1) Hash Index in memory: Map<SharedKey, Map<AttrName, Value>>
                        let mut attr_index = HashMap::new();
                        for row in attr_results.rows {
                            let key = row[0].clone(); // Shared Dim Value
                            let mut attrs = HashMap::new();
                            for (i, attr_name) in requested_attributes.iter().enumerate() {
                                 // Measures start at index 1
                                attrs.insert(attr_name.clone(), row[i + 1].clone()); 
                            }
                            attr_index.insert(key, attrs);
                        }

                        // Stitch the bulk attributes into the primary result set
                        for row in &mut result_set.rows {
                            let shared_val = &row[shared_col_idx];
                            let attrs_for_row = attr_index.get(shared_val);

                            for attr_name in &requested_attributes {
                                let display_val = match attrs_for_row.and_then(|m| m.get(attr_name)) {
                                    Some(val) => val.clone(),
                                    None => "-".to_string(), // Null handling
                                };

                                // Inject at the requested SELECT column index
                                let out_idx = result_set.headers.iter().position(|h| h == attr_name).unwrap();
                                if out_idx < row.len() {
                                    row[out_idx] = display_val;
                                } else {
                                    row.push(display_val);
                                }
                            }
                        }
                    }
                }

                return Ok(format_result_set(result_set));
            }
            Err("Unsupported SELECT format.".to_string())
        }

                // 2. CREATE TABLE
        // Syntax: CREATE TABLE Sales (Geography, Product)
        Statement::CreateTable { name, columns, .. } => {
            let cube_name = name.to_string();

            // Refuse to overwrite an existing cube/table.
            if catalog.get_cube(&cube_name).is_some() {
                return Err(format!("Cube '{}' already exists.", cube_name));
            }

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

// Helper: Extracts WHERE Geography = 'Europe' into a HashMap{"Geography": "Europe"}
// Supports IN
// Takes a HashMap<String, Vec<String>> to support multiple filter targets
fn extract_where_map(expr: &Expr, filters: &mut HashMap<String, Vec<String>>) -> Result<(), String> {
    match expr {
        Expr::BinaryOp { left, op: BinaryOperator::And, right } => {
            extract_where_map(left, filters)?;
            extract_where_map(right, filters)?;
        }
        Expr::BinaryOp { left, op: BinaryOperator::Eq, right } => {
            let dim_target = left.to_string();
            let val_target = right.to_string().replace("'", "");
            filters.insert(dim_target, vec![val_target]);
        }
        Expr::InList { expr, list, negated } => {
            if *negated { return Err("NOT IN is currently unsupported.".to_string()); }
            let dim_target = expr.to_string();
            let mut values = Vec::new();
            for item in list {
                values.push(item.to_string().replace("'", ""));
            }
            filters.insert(dim_target, values);
        }
        _ => return Err("Unsupported WHERE format. Use Dim = 'Val' or Dim IN ('Val1', 'Val2')".to_string()),
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