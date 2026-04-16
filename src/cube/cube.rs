use serde::{Serialize, Deserialize};
use std::sync::{Arc, RwLock};
// use std::error::Error;
use std::collections::{HashMap, HashSet};

use crate::dimension::dimension::Dimension;
use crate::storage::sparse_store::SparseStore;



pub struct SliceQuery {
    pub axes: Vec<String>,                 // Dimensions to group by (e.g., ["Product"])
    pub filters: HashMap<String, String>,  // Dimensions to lock (e.g., {"Geography": "Europe"})
}

pub struct ResultSet {
    pub headers: Vec<String>,
    pub rows: Vec<(Vec<String>, f64)>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Cube {
    pub name: String,
    
    // For saving to disk (Foreign Keys)
    pub dimension_names: Vec<String>,
    
    // For runtime speed (Shared Pointers). We skip saving this!
    #[serde(skip)]
    pub dimensions: Vec<Arc<RwLock<Dimension>>>,
    
    pub store: SparseStore,
    
    // The Cache. We skip saving this!
    #[serde(skip)]
    pub query_cache: HashMap<Vec<u32>, f64>,
}

impl Cube {
    pub fn new(name: &str, dimension_names: Vec<String>, dimensions: Vec<Arc<RwLock<Dimension>>>) -> Self {
        let dim_count = dimension_names.len();
        Cube {
            name: name.to_string(),
            dimension_names,
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
    pub fn write(&mut self, members: &[&str], value: f64) {
        let mut coords = Vec::new();

        for (i, m) in members.iter().enumerate() {
            // We lock the dimension for writing. If it's a new element, it gets added!
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
            // We have a base-level coordinate. Fetch from the Patria Trie!
            let val = self.store.query_exact(current_coords);
            return val * current_weight;
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
            let value: f64 = val_str.parse().unwrap_or(0.0);

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
        
        // 1. Prepare the push-down filters for the Trie
        let mut scanner_filters: Vec<Option<HashSet<u32>>> = vec![None; dim_count];
        
        // 2. Track the weights for JIT calculation (e.g., France = 1.0, Germany = 1.0)
        let mut weight_maps: Vec<HashMap<u32, f64>> = vec![HashMap::new(); dim_count];
        
        // 3. Track which indices represent our "Axes" (Columns in the output table)
        let mut axis_indices = Vec::new();

        for (i, dim_name) in self.dimension_names.iter().enumerate() {
            let dim = self.dimensions[i].read().unwrap();

            // 1. Is it requested in the SELECT clause?
            let is_axis = query.axes.contains(dim_name);
            if is_axis {
                axis_indices.push(i);
            }

            // 2. Is it filtered in the WHERE clause?
            if let Some(filter_val) = query.filters.get(dim_name) {
                let leaves = dim.get_leaf_descendants(filter_val);
                if leaves.is_empty() {
                    return Err(format!("Member '{}' not found in '{}'", filter_val, dim_name));
                }
                
                let leaf_ids: HashSet<u32> = leaves.keys().cloned().collect();
                scanner_filters[i] = Some(leaf_ids);
                weight_maps[i] = leaves; 

            } else if !is_axis {
                // It is NEITHER selected NOR filtered.
                return Err(format!("Dimension '{}' is neither filtered nor an axis.", dim_name));
            }
        }

        // 4. SCAN THE TRIE! (This fetches the raw base data instantly)
        let raw_data = self.store.scan_subcube(&scanner_filters);

        // 5. Aggregate and group the results
        // We use a HashMap to sum values that roll up into the same Axis combination
        let mut grouped_results: HashMap<Vec<u32>, f64> = HashMap::new();

        for (coords, base_val) in raw_data {
            let mut final_val = base_val;
            let mut row_key = Vec::new();

            for i in 0..dim_count {
                // Apply the consolidation weight if it was a filtered parent
                if let Some(weight) = weight_maps[i].get(&coords[i]) {
                    final_val *= weight;
                }
                
                // If this dimension is an Axis, add its ID to the grouping key
                if axis_indices.contains(&i) {
                    row_key.push(coords[i]);
                }
            }

            // Add the calculated value to the specific group
            *grouped_results.entry(row_key).or_insert(0.0) += final_val;
        }

        // 6. Format into a Tabular ResultSet (Translating IDs back to Strings)
        let mut result_set = ResultSet {
            headers: query.axes.clone(),
            rows: Vec::new(),
        };

        for (axis_ids, total_val) in grouped_results {
            let mut string_row = Vec::new();
            
            // Map the IDs in the row_key back to their Dimension Names
            for (idx, &id) in axis_indices.iter().zip(axis_ids.iter()) {
                let dim = self.dimensions[*idx].read().unwrap();
                string_row.push(dim.get_name(id));
            }
            
            result_set.rows.push((string_row, total_val));
        }

        Ok(result_set)
    }

}