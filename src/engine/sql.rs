use sqlparser::ast::{Expr, SetExpr, Statement, BinaryOperator, JoinConstraint, JoinOperator};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;
use std::collections::HashMap; 

use crate::cube::node::CellValue;

use crate::catalog::catalog::Catalog;
use crate::cube::cube::{SliceQuery, ResultSet, SplashMode};  
use crate::engine::attribute_join;

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
        // Accept "Hier IN Dim" (IN is case-insensitive). `split_once` on the
        // uppercased copy finds the keyword; we slice the ORIGINAL string at the
        // same byte offset (IN is ASCII, so offsets line up). No `.unwrap()`:
        // the `None` arm is the only thing that can happen when IN is absent.
        let (hier_name, dim_name) = match rest.to_uppercase().split_once(" IN ") {
            Some((before, _)) => {
                let idx = before.len();
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
                // V1 implements ONE LEFT join. A second join used to be silently
                // dropped (`relation.joins[0]` only), which returned columns of dashes
                // with no error — the misleading kind of wrong answer.
                if relation.joins.len() > 1 {
                    return Err(format!(
                        "Only one JOIN is supported (found {}). Join one attribute cube at a \
                         time, or issue separate queries.",
                        relation.joins.len()
                    ));
                }
                if !relation.joins.is_empty() {
                    let join = &relation.joins[0];
                    if let sqlparser::ast::TableFactor::Table { name, .. } = &join.relation {
                        joined_cube_name = Some(name.to_string());
                    }
                    match &join.join_operator {
                        // A bare `JOIN` parses as `Inner`, so both arms are accepted and
                        // both behave as a LEFT join — the one join semantic this engine
                        // implements. (`INNER JOIN` written explicitly therefore does NOT
                        // drop unmatched rows; it is an alias for the default here.)
                        JoinOperator::Inner(c) | JoinOperator::LeftOuter(c) => match c {
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
                        // Refused rather than approximated: a left-stitch cannot honour
                        // them, and pretending otherwise is a wrong answer.
                        JoinOperator::RightOuter(_) | JoinOperator::FullOuter(_) => {
                            return Err(
                                "RIGHT and FULL joins are not supported. This engine implements \
                                 a LEFT join: every row of the primary cube is kept and \
                                 attributes are attached where the conformed dimension matches. \
                                 Use a plain JOIN or LEFT JOIN."
                                    .to_string(),
                            );
                        }
                        JoinOperator::CrossJoin => {
                            return Err(
                                "CROSS JOIN is not supported; these cubes are joined through a \
                                 shared (conformed) dimension via USING (<dimension>)."
                                    .to_string(),
                            );
                        }
                        _ => {
                            return Err(
                                "Only a single LEFT [USING (...)] join is supported."
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
                // The single validated join, if the query has one. Validation lives in
                // `engine::attribute_join` so the plan layer cannot drift from SQL: the
                // KEY must be a CONFORMED DIMENSION (the same name in both cubes), given
                // by USING (<dimension>) or inferred when exactly one dimension conforms.
                let resolved_join: Option<attribute_join::ResolvedJoin> = match &joined_cube_name {
                    None => None,
                    Some(j_name) => {
                        // SQL syntax check, kept here: USING names exactly one dimension.
                        if let Some(cols) = &join_using
                            && cols.len() != 1
                        {
                            return Err(format!(
                                "USING currently accepts a single conformed dimension, got {}.",
                                cols.len()
                            ));
                        }
                        Some(attribute_join::resolve(
                            catalog,
                            primary_cube,
                            &primary_cube_name,
                            &attribute_join::JoinSpec {
                                cube: j_name.clone(),
                                using: join_using.as_ref().and_then(|c| c.first().cloned()),
                            },
                        )?)
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
                                        if let Some(idx) = primary_cube.dimension_names.iter().position(|n| n == &col_name) {
                        if !axes.contains(&col_name) { axes.push(col_name.clone()); }
                        if Some(&col_name) == p_m_dim.as_ref() { measure_dim_requested = true; }
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
                                        else if let Some(m_dim) = &p_m_dim
                        && let Some(dim_idx) = primary_cube.dimension_names.iter().position(|n| n == m_dim)
                    {
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

                                // Inject Default Members for omitted dimensions.
                //
                // Iterating by INDEX (rather than looking the name back up with
                // `.position(...).unwrap()`) removes a panic site entirely: the
                // index equals `dim_idx` by construction, so there is nothing
                // that can fail even if `dimension_names` and `dimensions` were
                // ever inconsistent.
                for dim_idx in 0..primary_cube.dimension_names.len() {
                    let dim_name = &primary_cube.dimension_names[dim_idx];
                    // We skip the Measure Dimension (it has special pivot rules)
                    if Some(dim_name) == p_m_dim.as_ref() { continue; }

                    // If it's not an Axis and not Filtered, we must default it
                    if !axes.contains(dim_name) && !filters.contains_key(dim_name) {
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

 
                // --- 4. ENRICH WITH JOINED ATTRIBUTES ---
                // The bulk lookup + stitch lives in `engine::attribute_join`, shared with
                // the native plan layer so the two front doors cannot disagree. SQL keeps
                // the "-" placeholder in its transcripts.
                if let Some(resolved) = &resolved_join {
                    attribute_join::apply(
                        catalog,
                        &mut result_set,
                        resolved,
                        &requested_attributes,
                        attribute_join::SQL_MISSING,
                    )?;
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

                                                // `write` returns a typed error on a coordinate/dimension
                        // mismatch. (Note: an over-long coordinate list can
                        // reach here because `has_consolidated_coordinate` uses
                        // `.get(i)` and reports `false` for the out-of-range
                        // index rather than the mismatch - so the guard in
                        // `write` is the one that actually catches it.)
                        cube.write(&member_refs, measure_val)?;
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