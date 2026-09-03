use serde::{Serialize, Deserialize};
use std::sync::{Arc, RwLock};
// use std::error::Error;
use std::collections::{HashMap, HashSet};

use crate::dimension::dimension::Dimension;
use crate::storage::sparse_store::SparseStore;
use crate::cube::node::CellValue;

pub struct SliceQuery {
    pub output_columns: Vec<String>,      // The exact order requested in SELECT
    pub axes: Vec<String>,                // The dimensions to group by
    pub requested_measures: Vec<String>,  // The specific measures to pivot (if any)
    pub filters: HashMap<String, String>, // The WHERE clause
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

    /// Point 2: JIT Calculation & Caching
    pub fn query_consolidated(&mut self, members: &[&str]) -> f64 {
        let mut query_signature = Vec::new();
        let mut leaf_resolutions = Vec::new();

        // 1. Resolve strings to IDs and get their leaf descendents
        for (i, m) in members.iter().enumerate() {
            let dim = self.dimensions[i].read().unwrap();
            
            // Get the ID for the cache key
            let id = match dim.get_id(m) {
                Some(id) => id,
                None => return 0.0, // Element doesn't exist at all
            };
            query_signature.push(id);

            // Get all leaves under this member (e.g., Europe -> [France, Germany])
            leaf_resolutions.push(dim.get_leaf_descendants(m));
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
    pub fn import_csv(&mut self, filepath: &str, has_headers: bool) -> Result<(), Box<dyn std::error::Error>> {
        // Create a CSV reader
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(has_headers)
            .from_path(filepath)?;

        let dim_count = self.dimensions.len();
        let mut row_count = 0;

        // Iterate through each row in the CSV
        for result in rdr.records() {
            let record = result?; // This is a single row
            
            // Ensure the row has enough columns (Dimensions + 1 Value column)
            if record.len() < dim_count + 1 {
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

        println!("Successfully imported {} rows into cube '{}'.", row_count, self.name);
        Ok(())
    }

pub fn query_slice(&self, query: &SliceQuery) -> Result<ResultSet, String> {
        let dim_count = self.dimensions.len();
        let mut scanner_filters: Vec<Option<HashSet<u32>>> = vec![None; dim_count];
        let mut weight_maps: Vec<HashMap<u32, f64>> = vec![HashMap::new(); dim_count];
        let mut axis_indices = Vec::new();

        // 1. Setup the Trie Scanner filters
        for (i, dim_name) in self.dimension_names.iter().enumerate() {
            let dim = self.dimensions[i].read().unwrap();

            if query.axes.contains(dim_name) {
                axis_indices.push(i);
            }

            if let Some(filter_val) = query.filters.get(dim_name) {
                if self.is_aggregating {
                    // Standard Cube: Resolve to Leaves!
                    let leaves = dim.get_leaf_descendants(filter_val);
                    if leaves.is_empty() { return Err(format!("Member '{}' not found", filter_val)); }
                    scanner_filters[i] = Some(leaves.keys().cloned().collect());
                    weight_maps[i] = leaves;
                } else {
                    // Attribute Cube: Do NOT resolve to leaves. Just fetch the exact ID!
                    if let Some(id) = dim.get_id(filter_val) {
                        let mut exact_id = HashSet::new();
                        exact_id.insert(id);
                        scanner_filters[i] = Some(exact_id);
                        weight_maps[i].insert(id, 1.0); // Weight is irrelevant here, just set to 1.0
                    } else {
                        return Err(format!("Member '{}' not found", filter_val));
                    }
                }
            } else if Some(dim_name) == self.measure_dimension.as_ref() && !query.requested_measures.is_empty() {
                // PIVOT MODE (Unchanged)
                if !axis_indices.contains(&i) { axis_indices.push(i); }
                let mut valid_measures = HashSet::new();
                for m_name in &query.requested_measures {
                    if let Some(id) = dim.get_id(m_name) {
                        valid_measures.insert(id);
                    }
                }
                scanner_filters[i] = Some(valid_measures);
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
                if self.is_aggregating {
                    if let Some(weight) = weight_maps[i].get(&coords[i]) {
                        if let CellValue::Numeric(n) = &mut final_val {
                            *n *= weight; 
                        }
                    }
                }
                
                if axis_indices.contains(&i) {
                    if Some(i) == measure_dim_idx && !query.requested_measures.is_empty() {
                        let dim = self.dimensions[i].read().unwrap();
                        current_measure_name = dim.get_name(coords[i]);
                    } else {
                        row_key.push(coords[i]);
                    }
                }
            }

            // Accumulate (Add numbers, or just overwrite strings)
            let measure_map = grouped_results.entry(row_key).or_insert_with(HashMap::new);
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

        let display_axis_indices: Vec<usize> = axis_indices.into_iter()
            .filter(|&i| Some(i) != measure_dim_idx || query.requested_measures.is_empty())
            .collect();

        for (axis_ids, measure_map) in grouped_results {
            let mut final_row = Vec::new();
            let mut row_dim_strings = HashMap::new();
            
            for (idx, &id) in display_axis_indices.iter().zip(axis_ids.iter()) {
                let dim_name = &self.dimension_names[*idx];
                let dim_val = self.dimensions[*idx].read().unwrap().get_name(id);
                row_dim_strings.insert(dim_name.clone(), dim_val);
            }

            for col in &query.output_columns {
                if let Some(dim_val) = row_dim_strings.get(col) {
                    final_row.push(dim_val.clone());
                } else if query.requested_measures.contains(col) || col == "value" {
                    // Safely get the value or print "-"
                    if let Some(val) = measure_map.get(col) {
                        final_row.push(val.to_string());
                    } else {
                        final_row.push("-".to_string());
                    }
                }
            }
            result_set.rows.push(final_row);
        }

        Ok(result_set)
    }
}