use sqlparser::ast::{Expr, SetExpr, Statement, BinaryOperator, JoinConstraint, JoinOperator};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;
use std::collections::HashMap; 

use crate::cube::node::CellValue;

use crate::catalog::catalog::Catalog;
use crate::cube::cube::{SliceQuery, ResultSet, SplashMode};  

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

    // 1b. Intercept CREATE HIERARCHY <hierarchy> IN <dimension>
    //
    // A dimension owns one or more hierarchies over the SAME base (leaf)
    // members. Every dimension is born with a default hierarchy named after
    // itself; this creates an ADDITIONAL one. Hierarchy names are unique per
    // database, so a hierarchy name can stand in for a dimension in queries.
    if upper_sql.starts_with("CREATE HIERARCHY") {
        let rest = sql_query[16..].trim().trim_end_matches(';').trim();
        // Accept "Hier IN Dim" (IN is case-insensitive).
        let (hier_name, dim_name) = match rest.to_uppercase().split_once(" IN ") {
            Some(_) => {
                let idx = rest.to_uppercase().find(" IN ").unwrap();
                (rest[..idx].trim().to_string(), rest[idx + 4..].trim().to_string())
            }
            None => {
                return Err(
                    "Usage: CREATE HIERARCHY <hierarchy> IN <dimension>".to_string()
                );
            }
        };
        if hier_name.is_empty() || dim_name.is_empty() {
            return Err("Usage: CREATE HIERARCHY <hierarchy> IN <dimension>".to_string());
        }
        let owner = catalog.create_hierarchy(&dim_name, &hier_name)?;
        return Ok(format!(
            "Hierarchy '{}' created in dimension '{}'.",
            hier_name, owner
        ));
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

                                // Check for JOINs (e.g., Attribute Cubes).
                //
                // The join KEY is always a CONFORMED DIMENSION: a dimension of
                // the same name that both cubes share. Because that is the only
                // thing two cubes can possibly join on, the standard `USING (col)`
                // clause is exactly sufficient - the column is named once.
                //
                //   * `USING (Col)`  -> join on conformed dimension `Col`.
                //   * no constraint  -> infer the conformed dimension, but only
                //                       when it is unambiguous.
                //   * `ON (...)`, `NATURAL`, `CROSS` -> rejected with guidance.
                let mut joined_cube_name = None;
                let mut join_using: Option<Vec<String>> = None;
                if !relation.joins.is_empty() {
                    let join = &relation.joins[0];
                    if let sqlparser::ast::TableFactor::Table { name, .. } = &join.relation {
                        joined_cube_name = Some(name.to_string());
                    }
                    match &join.join_operator {
                        JoinOperator::Inner(c)
                        | JoinOperator::LeftOuter(c)
                        | JoinOperator::RightOuter(c)
                        | JoinOperator::FullOuter(c) => match c {
                            JoinConstraint::Using(cols) => {
                                join_using = Some(
                                    cols.iter().map(|i| i.value.clone()).collect(),
                                );
                            }
                            JoinConstraint::None | JoinConstraint::Natural => {
                                join_using = None; // Infer the conformed dimension.
                            }
                            JoinConstraint::On(_) => {
                                return Err(
                                    "JOIN ... ON is not supported. Since the only thing two cubes \
                                     can join on is a shared (conformed) dimension, use \
                                     USING (<dimension>)."
                                        .to_string(),
                                );
                            }
                        },
                        JoinOperator::CrossJoin => {
                            return Err(
                                "CROSS JOIN is not supported; these cubes are joined through a \
                                 shared (conformed) dimension via USING (<dimension>)."
                                    .to_string(),
                            );
                        }
                        _ => {
                            return Err(
                                "Only INNER / LEFT / RIGHT / FULL [USING (...)] joins are \
                                 supported."
                                    .to_string(),
                            );
                        }
                    }
                }

                                // Resolve the join KEY up front, so that a malformed JOIN is
                // reported even when no attribute columns are selected. The key
                // is a CONFORMED DIMENSION: a dimension with the same name that
                // both cubes share.
                //
                //   * `USING (Col)`  -> use `Col` (must be conformed).
                //   * no constraint  -> infer, but only if exactly ONE dimension
                //                       conforms (otherwise it is ambiguous).
                let resolved_join: Option<(String, String)> = match &joined_cube_name {
                    None => None,
                    Some(j_name) => {
                        let j_cube = catalog.get_cube(j_name).ok_or_else(|| {
                            format!("Joined cube '{}' not found", j_name)
                        })?;
                        let shared_dim: String = if let Some(using_cols) = &join_using {
                            if using_cols.len() != 1 {
                                return Err(format!(
                                    "USING currently accepts a single conformed dimension, got {}.",
                                    using_cols.len()
                                ));
                            }
                            let col = &using_cols[0];
                            let in_primary = primary_cube.dimension_names.iter()
                                .any(|d| d.eq_ignore_ascii_case(col));
                            let in_joined = j_cube.dimension_names.iter()
                                .any(|d| d.eq_ignore_ascii_case(col));
                            if !in_primary || !in_joined {
                                return Err(format!(
                                    "Cannot JOIN '{}' with '{}' USING '{}': it must be a conformed \
                                     dimension (present in both cubes).",
                                    primary_cube_name, j_name, col
                                ));
                            }
                            // Normalize to the primary cube's exact casing.
                            primary_cube.dimension_names.iter()
                                .find(|d| d.eq_ignore_ascii_case(col))
                                .cloned()
                                .unwrap()
                        } else {
                            let shared: Vec<String> = primary_cube.dimension_names.iter()
                                .filter(|d| j_cube.dimension_names.contains(*d))
                                .cloned()
                                .collect();
                            match shared.len() {
                                0 => return Err(format!(
                                    "Cannot JOIN '{}' with '{}': they share no conformed dimension. \
                                     A JOIN keys off a dimension with the same name in both cubes; \
                                     write JOIN ... USING (<dimension>).",
                                    primary_cube_name, j_name
                                )),
                                1 => shared.into_iter().next().unwrap(),
                                _ => return Err(format!(
                                    "Cannot infer how to JOIN '{}' with '{}': they share {} conformed \
                                     dimensions ({}). Specify one with USING (<dimension>).",
                                    primary_cube_name, j_name, shared.len(), shared.join(", ")
                                )),
                            }
                        };
                        Some((j_name.clone(), shared_dim))
                    }
                };

                                // --- 2. CATEGORIZE COLUMNS ---
                let mut output_columns = Vec::new();
                let mut axes = Vec::new();
                let mut requested_measures = Vec::new();
                let mut requested_attributes = Vec::new(); // NEW: Track attribute columns
                let mut measure_dim_requested = false;
                // Output column -> the cube dimension it reads its member from.
                // Usually the dimension of the same name; a HIERARCHY name maps to
                // its OWNING dimension.
                let mut column_dim_index: HashMap<String, usize> = HashMap::new();
                // Dimension name -> hierarchy name to resolve that axis in.
                let mut axis_hierarchies: HashMap<String, String> = HashMap::new();

                                let p_m_dim = primary_cube.measure_dimension.clone();

                for proj in &select.projection {
                    let col_raw = proj.to_string();
                    let col_name = col_raw.split('.').next_back().unwrap().trim().to_string();

                    output_columns.push(col_name.clone());
                    
                    let mut found = false; // Track if we successfully categorized the column

                    // 1. Is it a primary dimension?
                    if primary_cube.dimension_names.contains(&col_name) {
                        if !axes.contains(&col_name) { axes.push(col_name.clone()); }
                        if Some(&col_name) == p_m_dim.as_ref() { measure_dim_requested = true; }
                        let idx = primary_cube.dimension_names.iter().position(|n| n == &col_name).unwrap();
                        column_dim_index.insert(col_name.clone(), idx);
                        found = true;
                    }
                    // 1b. Is it a HIERARCHY owned by one of this cube's
                    //     dimensions? A hierarchy name stands for itself and is a
                    //     valid query axis (e.g. `SELECT Ops, value`).
                    else if let Some((owner_dim, hier_name)) = catalog.dimension_of_reference(&col_name)
                        && let Some(hier_name) = hier_name
                        && let Some(idx) = primary_cube.dimension_names.iter().position(|n| n == &owner_dim)
                    {
                        if Some(&owner_dim) == p_m_dim.as_ref() {
                            // The measure dimension with a non-default hierarchy is
                            // not a supported axis.
                            return Err(format!(
                                "Hierarchy '{}' belongs to the measure dimension and cannot be used as an axis.",
                                col_name
                            ));
                        }
                        if !axes.contains(&owner_dim) { axes.push(owner_dim.clone()); }
                        axis_hierarchies.insert(owner_dim.clone(), hier_name);
                        column_dim_index.insert(col_name.clone(), idx);
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
                    
                                        // 3. Is it a joined attribute? (An attribute lives in the
                                        //    joined cube's measure dimension.)
                                        if !found
                                            && let Some(j_name) = &joined_cube_name
                                            && let Some(j_cube) = catalog.get_cube(j_name)
                                            && let Some(jm_dim) = &j_cube.measure_dimension
                                            && let Some(j_dim_idx) =
                                                j_cube.dimension_names.iter().position(|n| n == jm_dim)
                                            && j_cube.dimensions[j_dim_idx]
                                                .read()
                                                .unwrap()
                                                .get_id(&col_name)
                                                .is_some()
                                        {
                                            requested_attributes.push(col_name.clone());
                                            found = true;
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

                // A WHERE key may be a HIERARCHY name rather than a dimension
                // name (e.g. `WHERE Ops = 'Warehouse_A'`). Rewrite such keys to
                // the OWNING dimension and QUALIFY the value as `Hierarchy:Member`
                // so the cube resolves it in the right hierarchy.
                let mut normalized_filters: HashMap<String, Vec<String>> = HashMap::new();
                for (key, vals) in filters.into_iter() {
                    match catalog.dimension_of_reference(&key) {
                        Some((owner_dim, Some(hier))) => {
                            if Some(&owner_dim) == p_m_dim.as_ref() {
                                return Err(format!(
                                    "Hierarchy '{}' belongs to the measure dimension and cannot be filtered on.",
                                    key
                                ));
                            }
                            let qualified: Vec<String> = vals.into_iter()
                                .map(|v| format!("{}:{}", hier, v))
                                .collect();
                            normalized_filters.entry(owner_dim).or_default().extend(qualified);
                        }
                        Some((owner_dim, None)) => {
                            normalized_filters.entry(owner_dim).or_default().extend(vals);
                        }
                        None => {
                            normalized_filters.entry(key).or_default().extend(vals);
                        }
                    }
                }
                let mut filters = normalized_filters;

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
                    if let Some(m_dim) = &p_m_dim
                        && filters.contains_key(m_dim)
                    {
                        return Err(format!(
                            "Cannot filter on '{}' when pivoting specific measures.",
                            m_dim
                        ));
                    }
                } else if !measure_dim_requested
                    && axes.is_empty()
                    && !output_columns.contains(&"value".to_string())
                {
                    output_columns.push("value".to_string());
                }

                                let slice_query = SliceQuery { 
                    output_columns: output_columns.clone(), 
                    axes, 
                    requested_measures, 
                    filters,
                    axis_hierarchies,
                    column_dim_index,
                };
                
                let mut result_set = primary_cube.query_slice(&slice_query)?;

 
                                                                                // --- 4. ENRICH WITH JOINED ATTRIBUTES (BULK METHOD) ---
                // `resolved_join` was validated above; here we only actually
                // stitch attributes in when the query asked for some.
                if let Some((j_name, shared_dim)) = &resolved_join
                    && !requested_attributes.is_empty()
                {
                    let j_cube = catalog.get_cube(j_name).unwrap();

                        let shared_col_idx = result_set.headers.iter().position(|h| h == shared_dim)
                            .ok_or_else(|| format!(
                                "Cannot JOIN '{}' with '{}': the conformed dimension '{}' is not \
                                 projected, so there is no key to join the attributes onto.",
                                primary_cube_name, j_name, shared_dim
                            ))?;

                        // Bulk request: query the attribute cube ONCE for all
                        // requested attributes, keyed by the conformed dimension.
                        let mut attr_outputs = vec![shared_dim.clone()];
                        attr_outputs.extend(requested_attributes.clone());

                                                let attr_slice = SliceQuery {
                            output_columns: attr_outputs, // Include the attributes!
                            axes: vec![shared_dim.clone()],
                            requested_measures: requested_attributes.clone(),
                            filters: HashMap::new(),
                            ..Default::default()
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

                                    if let Some(query) = source
                && let SetExpr::Values(values) = &*query.body
            {
                    let mut row_count = 0;
                    let mut splashed_count = 0;

                    for row in &values.rows {
                        // Ensure we have exactly Dim_Count + 1 (the measure)
                        if row.len() != cube.dimension_names.len() + 1 {
                            return Err(format!("Expected {} values, got {}", cube.dimension_names.len() + 1, row.len()));
                        }

                                                // Extract strings for the dimensions (one per dimension;
                        // the trailing value is the measure).
                        let members: Vec<String> = row.iter()
                            .take(cube.dimension_names.len())
                            .map(|v| v.to_string().replace("'", ""))
                            .collect();

                        // Extract the final numeric value
                        let measure_str = row.last().unwrap().to_string();
                        let measure_val = match measure_str.parse::<f64>() {
                            Ok(n) => CellValue::Numeric(n),
                            Err(_) => CellValue::String(measure_str.replace("'", "")),
                        };

                                                                        // Write to the Cube (This acts as an UPSERT)
                        let member_refs: Vec<&str> = members.iter().map(|s| s.as_str()).collect();

                        // Does any coordinate name a CONSOLIDATED (aggregated)
                        // member? If so AND the value is numeric, splash the value
                        // down to the leaf descendants instead of storing it at
                        // the aggregate node.
                        if let CellValue::Numeric(n) = &measure_val
                            && cube.has_consolidated_coordinate(&member_refs)
                        {
                            let leaf_count =
                                cube.write_splashed(&member_refs, *n, SplashMode::Replace)?;
                            splashed_count += leaf_count;
                            row_count += 1;
                            continue;
                        }

                        cube.write(&member_refs, measure_val);
                        row_count += 1;
                    }
                    if splashed_count > 0 {
                        return Ok(format!(
                            "Upserted {} row(s) into '{}' (splashed across {} leaf cell(s)).",
                            row_count, cube_name, splashed_count
                        ));
                    }
                    return Ok(format!("Upserted {} row(s) into '{}'.", row_count, cube_name));
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