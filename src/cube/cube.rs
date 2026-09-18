use serde::{Serialize, Deserialize};
use std::sync::{Arc, RwLock};
// use std::error::Error;
use std::collections::{HashMap, HashSet};

use crate::dimension::dimension::{Dimension, MemberType};
use crate::storage::sparse_store::SparseStore;
use crate::cube::node::CellValue;

/// Splits a coordinate into an optional hierarchy qualifier and a member name.
///
/// Multi-hierarchy dimensions let several hierarchies define a member with the
/// same name (e.g. `All`). A coordinate may be written `Hierarchy:Member` to
/// pick a specific hierarchy. When no colon is present the member is resolved
/// in the dimension's DEFAULT hierarchy (named after the dimension), which is
/// the only hierarchy a first-time user ever sees.
///
///   "ByMarket:All"  -> (Some("ByMarket"), "All")
///   "All"           -> (None, "All")
///   "France"        -> (None, "France")
///
/// Note: a member name that itself contains a colon is not supported; the last
/// colon is treated as the separator when one exists.
fn split_qualified(coord: &str) -> (Option<&str>, &str) {
    match coord.split_once(':') {
        Some((hier, member)) if !hier.is_empty() && !member.is_empty() => {
            (Some(hier), member)
        }
        _ => (None, coord),
    }
}

/// How `write_splashed` treats existing leaf data when distributing a value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SplashMode {
    /// Overwrite each leaf with its allocated share of the total.
    Replace,
    /// Add each leaf's allocated share on top of whatever is already there.
    Add,
}

/// Recursively forms the cross-product of per-dimension leaf lists, multiplying
/// the per-dimension weights together for each resulting cell.
fn build_leaf_product(
    per_dim_leaves: &[Vec<(u32, f64)>],
    current_coords: &mut Vec<u32>,
    depth: usize,
    current_weight: f64,
    out: &mut Vec<(Vec<u32>, f64)>,
) {
    if depth == per_dim_leaves.len() {
        out.push((current_coords.clone(), current_weight));
        return;
    }
    for &(leaf_id, weight) in &per_dim_leaves[depth] {
        current_coords.push(leaf_id);
        build_leaf_product(
            per_dim_leaves,
            current_coords,
            depth + 1,
            current_weight * weight,
            out,
        );
        current_coords.pop();
    }
}

#[derive(Default)]
pub struct SliceQuery {
    pub output_columns: Vec<String>,      		// The exact order requested in SELECT
    pub axes: Vec<String>,                		// The dimensions to group by
    pub requested_measures: Vec<String>,  		// The specific measures to pivot (if any)
    pub filters: HashMap<String, Vec<String>>, // The WHERE clause
    // For each DIMENSION name in `axes`, the HIERARCHY to aggregate/resolve it
    // through. Absent => the dimension's default hierarchy. This is what makes a
    // hierarchy name usable as a query axis (e.g. `SELECT Ops, value`).
    pub axis_hierarchies: HashMap<String, String>,
    // Maps an OUTPUT column name to the index of the dimension whose member it
    // should display. Lets a hierarchy name (e.g. `Ops`) be used directly as a
    // SELECT axis while still reading the right dimension's value.
    pub column_dim_index: HashMap<String, usize>,
}

pub struct ResultSet {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>, // All cells converted to string for easy printing
}

#[derive(Clone, Serialize, Deserialize)]

pub struct Cube {
    pub name: String,
    
    // For saving to disk (Foreign Keys)
    pub dimension_names: Vec<String>,

	pub measure_dimension: Option<String>,
	// Determines if this acts like an attribute table or a data cube
    pub is_aggregating: bool, 
    
    // For runtime speed (Shared Pointers). We skip saving this!
    #[serde(skip)]
    pub dimensions: Vec<Arc<RwLock<Dimension>>>,
    
    pub store: SparseStore,
	
    
    // The Cache. We skip saving this!
    #[serde(skip)]
    pub query_cache: HashMap<Vec<u32>, f64>,// keep cache f64
}

impl Cube {
    pub fn new( name: &str, 
				dimension_names: Vec<String>, 
				dimensions: Vec<Arc<RwLock<Dimension>>>,
				measure_dimension: Option<String>,
				is_aggregating: bool				
				)
				-> Self {
        let dim_count = dimension_names.len();
        Cube {
            name: name.to_string(),
            dimension_names,
			measure_dimension,
			is_aggregating,
            dimensions,
            store: SparseStore::new(dim_count),
            query_cache: HashMap::new(),
        }
    }

    // Called when loading from disk to re-connect the pointers
    pub fn attach_dimensions(&mut self, dimensions: Vec<Arc<RwLock<Dimension>>>) {
        self.dimensions = dimensions;
    }

    /// Point 1: Row-by-row data import. Auto-creates leaves if they don't exist.
    pub fn write(&mut self, members: &[&str], value: CellValue) {
        let mut coords = Vec::new();

        for (i, m) in members.iter().enumerate() {
            // We lock the dimension for writing. If it's a new element, it gets added!
			// Note: If it's a non-aggregating cube, we might be writing to a parent node!
            // add_consolidated is safe to use here because it handles both new and existing members
            let mut dim = self.dimensions[i].write().unwrap();
			
            let id = dim.add_leaf(m);
            coords.push(id);
        }

                self.store.write(&coords, value);
        
        // Point 2: Cache invalidation. Base data changed, so aggregates are invalid.
        self.query_cache.clear(); 
    }

            /// True when at least one of the given coordinates names a Consolidated
            /// (aggregated) member of this cube. Used by INSERT to decide whether a
            /// numeric write should be splashed down to leaves.
            ///
            /// A coordinate may be hierarchy-qualified as `Hierarchy:Member`; when it
            /// is not, the member is resolved in the dimension's DEFAULT hierarchy.
            pub fn has_consolidated_coordinate(&self, members: &[&str]) -> bool {
                members.iter().enumerate().any(|(i, m)| {
                    let (hier, member) = split_qualified(m);
                    self.dimensions
                        .get(i)
                        .map(|d| d.read().unwrap().is_consolidated_in(hier, member))
                        .unwrap_or(false)
                })
            }

    /// Writes `value` to a cell, but if any coordinate names a CONSOLIDATED
    /// (aggregated) member, the value is "splashed" down to that member's leaf
    /// descendants instead of being stored at the aggregate node.
    ///
    /// This is data allocation. The written total, when read back through the
    /// aggregation path, equals `value`:
    ///
    ///   * Each consolidated dimension is expanded into its leaf descendants
    ///     (the cross-product across dimensions is taken).
    ///   * Weights combine multiplicatively down each dimension.
    ///   * The target total is distributed aThere aer still cross leaves proportionally to the
    ///     ABSOLUTE weight share, with each leaf's sign following its own
    ///     weight. This keeps the weighted read-back exactly equal to `value`,
    ///     even when a parent has negative weights (e.g. Profit = Rev - Costs).
    ///
    /// If no coordinate is consolidated this behaves exactly like `write`.
    ///
    /// `mode` selects how existing leaf data is treated:
    ///   * `SplashMode::Replace` - leaves are overwritten with their share.
    ///   * `SplashMode::Add`     - the share is added on top of the current leaf.
    pub fn write_splashed(
        &mut self,
        members: &[&str],
        value: f64,
        mode: SplashMode,
    ) -> Result<usize, String> {
        if self.dimensions.len() != members.len() {
            return Err(format!(
                "Expected {} coordinate(s), got {}.",
                self.dimensions.len(),
                members.len()
            ));
        }

        // 1. For every dimension, resolve the member into leaf -> weight pairs.
        //    A leaf resolves to itself (weight 1.0); a consolidated member
        //    resolves to its descendants with aggregated weights.
        let mut per_dim_leaves: Vec<Vec<(u32, f64)>> = Vec::with_capacity(members.len());
        let mut any_consolidated = false;

                                for (i, m) in members.iter().enumerate() {
            let mut dim = self.dimensions[i].write().unwrap();

            // A coordinate may be hierarchy-qualified as `Hierarchy:Member`;
            // otherwise resolve in the dimension's default hierarchy.
            let (hier, member) = split_qualified(m);

            // Missing members are auto-created as leaves, mirroring `write`.
            // (A brand-new name can never be a consolidated node, since we
            //  cannot guess its children.)
            let id = match dim.get_id_in(hier, member) {
                Some(id) => id,
                None => dim.add_leaf(member),
            };

            if dim.member_type(id) == Some(MemberType::Consolidated) {
                any_consolidated = true;
                let leaves = dim.get_leaf_descendants_in(hier, member);
                if leaves.is_empty() {
                    return Err(format!(
                        "Consolidated member '{}' has no leaf descendants to allocate to.",
                        m
                    ));
                }
                per_dim_leaves.push(leaves.into_iter().collect());
            } else {
                per_dim_leaves.push(vec![(id, 1.0)]);
            }
        }

        // Fast path: nothing to splash, behave like a normal leaf write.
        if !any_consolidated {
            let mut coords = Vec::with_capacity(members.len());
            for leaves in &per_dim_leaves {
                coords.push(leaves[0].0);
            }
            self.store.write(&coords, CellValue::Numeric(value));
            self.query_cache.clear();
            return Ok(1);
        }

        // 2. Build the cross-product of leaf cells and their combined weights.
        let mut cells: Vec<(Vec<u32>, f64)> = Vec::new();
        build_leaf_product(&per_dim_leaves, &mut Vec::new(), 0, 1.0, &mut cells);

        // 3. Total absolute weight, used to split `value` proportionally.
        let total_abs: f64 = cells.iter().map(|(_, w)| w.abs()).sum();
        if total_abs == 0.0 {
            return Err("Cannot allocate: total absolute weight is zero.".to_string());
        }
        let scale = value / total_abs;

        // 4. Write each leaf. Sign follows the leaf's own weight so that the
        //    weighted read-back equals the requested total.
        for (coords, weight) in &cells {
            let share = scale * weight.abs() * weight.signum();
            let to_store = match mode {
                SplashMode::Replace => share,
                SplashMode::Add => {
                    let existing = match self.store.query_exact(coords) {
                        Some(CellValue::Numeric(n)) => n,
                        _ => 0.0,
                    };
                    existing + share
                }
            };
            self.store.write(coords, CellValue::Numeric(to_store));
        }

        // Point 2: Cache invalidation.
        self.query_cache.clear();
        Ok(cells.len())
    }

    /// Point 2: JIT Calculation & Caching
    pub fn query_consolidated(&mut self, members: &[&str]) -> f64 {
        let mut query_signature = Vec::new();
        let mut leaf_resolutions = Vec::new();

                // 1. Resolve strings to IDs and get their leaf descendents.
        //
        // CACHE KEY SOUNDNESS: `query_signature` is the vector of RESOLVED ids.
        // Because leaf ids are shared across hierarchies but aggregate ids are
        // per-hierarchy (distinct ids in the dimension's single id space), a
        // given signature always maps to exactly one (hierarchy, member) choice
        // per dimension - so the resolved ids fully determine the answer. Never
        // "share" an aggregate id between hierarchies; that would silently
        // corrupt this cache.
        for (i, m) in members.iter().enumerate() {
            let dim = self.dimensions[i].read().unwrap();
            let (hier, member) = split_qualified(m);

            // Get the ID for the cache key
            let id = match dim.get_id_in(hier, member).or_else(|| dim.get_id(member)) {
                Some(id) => id,
                None => return 0.0, // Element doesn't exist at all
            };
            query_signature.push(id);

            // Get all leaves under this member (e.g., Europe -> [France, Germany])
            leaf_resolutions.push(dim.get_leaf_descendants_in(hier, member));
        }

        // 2. Check the Cache FIRST to avoid database explosion
        if let Some(&cached_value) = self.query_cache.get(&query_signature) {
            return cached_value;
        }

        // 3. JIT Calculation (Cartesian product of all leaves)
        // For a prototype, we do a simple recursive combination of the resolved leaves
        let total = self.calculate_jit(&leaf_resolutions, &mut Vec::new(), 0, 1.0);

        // 4. Store in Cache and return
        self.query_cache.insert(query_signature, total);
        
        total
    }

// Fast O(1) lookup for exact strings (Perfect for Attribute Joins)
    pub fn read_cell(&self, members: &[&str]) -> Option<CellValue> {
        if members.len() != self.dimensions.len() { return None; }
        
                let mut coords = Vec::new();
        for (i, m) in members.iter().enumerate() {
            let dim = self.dimensions[i].read().unwrap();
            {
                let (hier, member) = split_qualified(m);
                let id = dim.get_id_in(hier, member)?;
                coords.push(id);
            }
        }
        
        self.store.query_exact(&coords)
    }

    // Helper: Recursively combine leaves. (e.g., [France, Germany] x [Laptop] x [Q1])
fn calculate_jit(
        &self, 
        resolutions: &[HashMap<u32, f64>], 
        current_coords: &mut Vec<u32>, 
        depth: usize, 
        current_weight: f64
    ) -> f64 {
        if depth == resolutions.len() {
            // Fetch from the Patria Trie!
            if let Some(CellValue::Numeric(val)) = self.store.query_exact(current_coords) {
                return val * current_weight;
            }
            return 0.0; // If it's a String or None, it doesn't contribute to the math sum
        }

        let mut sum = 0.0;
        for (&leaf_id, &weight) in &resolutions[depth] {
            current_coords.push(leaf_id);
            sum += self.calculate_jit(resolutions, current_coords, depth + 1, current_weight * weight);
            current_coords.pop();
        }

        sum
    }
	
	pub fn clear_cache(&mut self) {
        self.query_cache.clear();
    }
	/// Streams data from a CSV file directly into the Cube.
	/// Assumes the CSV columns exactly match the Dimension order, 
	/// and the LAST column is the numeric Value.
	/// Returns a human-readable summary of the import on success.
	pub fn import_csv(&mut self, filepath: &str, has_headers: bool) -> Result<String, Box<dyn std::error::Error>> {
                // Create a CSV reader
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(has_headers)
            .flexible(true)
            .from_path(filepath)?;

                let dim_count = self.dimensions.len();
        let mut row_count = 0;
        let mut skipped = 0;

        // Iterate through each row in the CSV
        for result in rdr.records() {
            let record = result?; // This is a single row
            
            // Every dimension is a column; the value is the "extra" trailing
            // column, so a well-formed row has exactly dim_count + 1 fields.
            // (In the classic measureless form, a 'Measure' dimension is just
            // another coordinate, not a magic value slot.)
            if record.len() < dim_count + 1 {
                skipped += 1;
                continue;
            }

            // 1. Extract the string members for the dimensions
            let mut members = Vec::with_capacity(dim_count);
            for i in 0..dim_count {
                members.push(record.get(i).unwrap_or(""));
            }

            // 2. Extract and parse the numeric value from the last column
            let val_str = record.get(dim_count).unwrap_or("0");
            let value = match val_str.parse::<f64>() {
                Ok(n) => CellValue::Numeric(n),
                Err(_) => CellValue::String(val_str.to_string()),
            };

            // 3. Write it to the engine! 
            // (This automatically updates the Dictionaries and the Trie)
            self.write(&members, value);
            row_count += 1;
        }

        if skipped > 0 {
            Ok(format!(
                "Successfully imported {} rows into cube '{}' (skipped {} row(s) with fewer than {} column(s)).",
                row_count, self.name, skipped, dim_count + 1
            ))
        } else {
            Ok(format!("Successfully imported {} rows into cube '{}'.", row_count, self.name))
        }
    }

pub fn query_slice(&self, query: &SliceQuery) -> Result<ResultSet, String> {
        let dim_count = self.dimensions.len();

        // Make sure each dimension's derived display order is up to date before
        // the result rows are sorted by it. This is cheap when nothing changed
        // (the dirty flag is set only by structural edits, not by data writes).
        for dim_arc in &self.dimensions {
            dim_arc.write().unwrap().ensure_display_order();
        }

        let mut scanner_filters: Vec<Option<HashSet<u32>>> = vec![None; dim_count];
        let mut weight_maps: Vec<HashMap<u32, f64>> = vec![HashMap::new(); dim_count];
        let mut axis_indices = Vec::new();

        // 1. Setup the Trie Scanner filters
        for (i, dim_name) in self.dimension_names.iter().enumerate() {
            let dim = self.dimensions[i].read().unwrap();

            if query.axes.contains(dim_name) {
                axis_indices.push(i);
            }

						if let Some(filter_vals) = query.filters.get(dim_name) {
                let mut combined_leaf_ids = HashSet::new();
                
                for filter_val in filter_vals {
                    // A filter value may be hierarchy-qualified `Hierarchy:Member`;
                    // otherwise it resolves in the dimension's default hierarchy.
                    let (hier, member) = split_qualified(filter_val);
                    if self.is_aggregating {
                        let leaves = dim.get_leaf_descendants_in(hier, member);
                        if leaves.is_empty() { return Err(format!("Member '{}' not found", filter_val)); }
                        combined_leaf_ids.extend(leaves.keys().cloned());
                        weight_maps[i].extend(leaves);
                    } else {
                        if let Some(id) = dim.get_id_in(hier, member) {
                            combined_leaf_ids.insert(id);
                            weight_maps[i].insert(id, 1.0); // Attribute
                        } else {
                            return Err(format!("Member '{}' not found", filter_val));
                        }
                    }
                }
                scanner_filters[i] = Some(combined_leaf_ids);
            }
        }

        let raw_data = self.store.scan_subcube(&scanner_filters);

        // 3. Aggregate Results (Now using CellValue!)
        let mut grouped_results: HashMap<Vec<u32>, HashMap<String, CellValue>> = HashMap::new();
        let measure_dim_idx = self.measure_dimension.as_ref()
            .and_then(|m| self.dimension_names.iter().position(|d| d == m));

                for (coords, base_val) in raw_data {
            let mut final_val = base_val.clone(); // Can be String or Numeric
                        let mut row_key = Vec::new();
            let mut current_measure_name = "value".to_string();

            for i in 0..dim_count {
                // Only apply math if the cell is Numeric AND the cube is aggregating
                if self.is_aggregating
                    && let Some(weight) = weight_maps[i].get(&coords[i])
                        && let CellValue::Numeric(n) = &mut final_val {
                            *n *= weight; 
                        }
                
                if axis_indices.contains(&i) || Some(i) == measure_dim_idx{
                    if Some(i) == measure_dim_idx && !query.requested_measures.is_empty() {
                        let dim = self.dimensions[i].read().unwrap();
						let raw_name = dim.get_name(coords[i]);
						// Find the exact casing the user asked for in the SELECT, or fallback to the dictionary casing
						current_measure_name = query.requested_measures.iter()
							.find(|m| m.eq_ignore_ascii_case(&raw_name))
							.cloned()
							.unwrap_or(raw_name);
                    } else {
                        // NOTE: `row_key` is COMPACTED - it holds only the members
                        // for dimensions that are actually displayed as a row axis
                        // (the measure dimension is excluded when measures are
                        // pivoted). So position in `row_key` is NOT the dimension
                        // index; we record the mapping below.
                        row_key.push(coords[i]);
                    }
                }
            }

            // Accumulate (Add numbers, or just overwrite strings)
            let measure_map = grouped_results.entry(row_key).or_default();
            let existing_val = measure_map.entry(current_measure_name).or_insert(CellValue::Numeric(0.0));
            
            match (existing_val, final_val) {
                (CellValue::Numeric(existing), CellValue::Numeric(new)) => *existing += new,
                (val, new) => *val = new, // If it's a string, just overwrite it (no math)
            }
        }

        // 4. Format Output (Unchanged from before, because .to_string() handles the Display trait we just made!)
        let mut result_set = ResultSet {
            headers: query.output_columns.clone(),
            rows: Vec::new(),
        };

                        // Dimensions that produce a row axis AND end up as a column in the
        // compacted `row_key` (i.e. everything shown except the pivoted measure
        // dimension). `row_key` stores members in this exact order, so the
        // position of a dimension in this list is its offset within `row_key`.
        let display_axis_indices: Vec<usize> = axis_indices.iter()
            .copied()
            .filter(|&i| Some(i) != measure_dim_idx || query.requested_measures.is_empty())
            .collect();

        // Deterministic row order. HashMap iteration order is randomized per
        // process, so we sort rows by the DISPLAY ORDER of each shown axis.
        // Display order defaults to member creation order but can be authored
        // at design time on the dimension, in which case run-time output
        // follows the authored order.
        //
        // IMPORTANT: `row_key` is COMPACTED (see the aggregation loop above), so
        // we must index it by the axis's position WITHIN `display_axis_indices`,
        // not by its dimension index `i`. Using the dimension index would read
        // past the end of short keys and silently collapse every comparison to
        // Equal, leaving the (randomized) HashMap order intact.
                let mut ordered: Vec<(Vec<u32>, HashMap<String, CellValue>)> =
            grouped_results.into_iter().collect();
        ordered.sort_by(|(a, _), (b, _)| {
            for (slot, &i) in display_axis_indices.iter().enumerate() {
                let dim = self.dimensions[i].read().unwrap();
                // Honour the axis's hierarchy for ordering when one was named.
                let hier = query.axis_hierarchies
                    .get(&self.dimension_names[i])
                    .map(|h| h.as_str());
                let ra = a.get(slot).map(|&id| dim.display_order_rank_in(hier, id)).unwrap_or(usize::MAX);
                let rb = b.get(slot).map(|&id| dim.display_order_rank_in(hier, id)).unwrap_or(usize::MAX);
                match ra.cmp(&rb) {
                    std::cmp::Ordering::Equal => continue,
                    other => return other,
                }
            }
            std::cmp::Ordering::Equal
        });

 for (axis_ids, measure_map) in ordered {
            let mut final_row = Vec::new();
            let mut row_dim_strings = HashMap::new();
            
                        // 1. Safely map any available axis IDs to their String names, keyed
            //    BOTH by dimension name and by output-column name (so a hierarchy
            //    used as a SELECT axis displays correctly).
            for (idx, &id) in display_axis_indices.iter().zip(axis_ids.iter()) {
                let dim_name = &self.dimension_names[*idx];
                let dim_val = self.dimensions[*idx].read().unwrap().get_name(id);
                row_dim_strings.insert(dim_name.clone(), dim_val);
            }
            // Resolve any output column that points at a displayed axis dimension.
            for col in &query.output_columns {
                if row_dim_strings.contains_key(col) {
                    continue;
                }
                if let Some(&idx) = query.column_dim_index.get(col)
                    && let Some(slot) = display_axis_indices.iter().position(|&i| i == idx)
                    && let Some(&id) = axis_ids.get(slot)
                {
                    let dim_val = self.dimensions[idx].read().unwrap().get_name(id);
                    row_dim_strings.insert(col.clone(), dim_val);
                }
            }

                        // 2. Build the output row exactly matching the requested SELECT order
            for col in &query.output_columns {
                if let Some(dim_val) = row_dim_strings.get(col) {
                    // It's a dimension
                    final_row.push(dim_val.clone());
                } else if query.requested_measures.contains(col) || col == "value" {
                    // It's a measure! Make sure we lookup the EXACT string, case-sensitive!
                    let matched_val = query.requested_measures.iter().find(|m| m.eq_ignore_ascii_case(col));
                    let lookup_key = matched_val.unwrap_or(col);
                    
                    if let Some(val) = measure_map.get(lookup_key) {
                        final_row.push(val.to_string());
                    } else {
                        final_row.push("-".to_string());
                    }
                } else {
                    // It's a Joined Attribute placeholder
                    final_row.push(String::new());
                }
            }
            
            result_set.rows.push(final_row);
        }
		
        Ok(result_set)
    }
}