use serde::{Serialize, Deserialize};
use std::collections::HashSet;
use crate::cube::node::Node;

#[derive(Clone, Serialize, Deserialize)]
pub struct SparseStore {
    root: Node,
    // We might track depth or other metadata here later
    #[allow(dead_code)] 
    dim_count: usize, 
}

impl SparseStore {
    pub fn new(dim_count: usize) -> Self {
        SparseStore {
            root: Node::new(),
            dim_count,
        }
    }

    // --- WRITE LOGIC ---
    pub fn write(&mut self, coords: &[u32], value: f64) {
        let mut current_node = &mut self.root;
        
        for &id in coords {
            // "entry().or_insert()" is the standard Rust way to 
            // get a value or create it if missing.
            current_node = current_node.children
                .entry(id)
                .or_insert_with(Node::new);
        }
        
        current_node.value = Some(value);
    }

    // --- QUERY LOGIC (Moved from query.rs) ---
    pub fn query(&self, coords: &[Option<u32>]) -> f64 {
        Self::query_recursive(&self.root, coords, 0)
    }
	// --- EXACT QUERY FOR JIT CALCULATION ---
    // Takes an exact path of leaf IDs and fetches the value. No wildcards.
    pub fn query_exact(&self, coords: &[u32]) -> f64 {
        let mut current_node = &self.root;
        
        // Walk down the tree following the IDs
        for id in coords {
            match current_node.children.get(id) {
                Some(child) => current_node = child,
                None => return 0.0, // If the path breaks, there is no data here
            }
        }
        
        // Return the value if it exists, otherwise 0.0
        current_node.value.unwrap_or(0.0)
    }
	
    fn query_recursive(node: &Node, coords: &[Option<u32>], depth: usize) -> f64 {
        // Base case: we reached the target depth
        if depth == coords.len() {
            return node.value.unwrap_or(0.0);
        }

        match coords[depth] {
            // Case 1: Exact Match (Drill down)
            Some(id) => {
                match node.children.get(&id) {
                    Some(child) => Self::query_recursive(child, coords, depth + 1),
                    None => 0.0,
                }
            }
            // Case 2: Wildcard/None (Aggregate all children)
            None => {
                let mut sum = 0.0;
                for child in node.children.values() {
                    sum += Self::query_recursive(child, coords, depth + 1);
                }
                sum
            }
        }
    }

    // --- SUB-CUBE SCANNER ---
    // allowed_leaves: For each dimension, a Set of valid IDs, or None to allow all.
    pub fn scan_subcube(&self, allowed_leaves: &[Option<HashSet<u32>>]) -> Vec<(Vec<u32>, f64)> {
        let mut results = Vec::new();
        self.scan_recursive(&self.root, allowed_leaves, 0, &mut Vec::new(), &mut results);
        results
    }
    fn scan_recursive(
        &self, 
        node: &Node, 
        allowed_leaves: &[Option<HashSet<u32>>], 
        depth: usize, 
        current_path: &mut Vec<u32>, 
        results: &mut Vec<(Vec<u32>, f64)>
    ) {
        // Base case: We reached the bottom of the tree
        if depth == allowed_leaves.len() {
            if let Some(val) = node.value {
                results.push((current_path.clone(), val));
            }
            return;
        }

        match &allowed_leaves[depth] {
            // Filter applied: Only check the specific requested children
            Some(valid_ids) => {
                for id in valid_ids {
                    if let Some(child) = node.children.get(id) {
                        current_path.push(*id);
                        self.scan_recursive(child, allowed_leaves, depth + 1, current_path, results);
                        current_path.pop();
                    }
                }
            }
            // No filter (Axis): Walk every child that actually exists in the database
            None => {
                for (id, child) in &node.children {
                    current_path.push(*id);
                    self.scan_recursive(child, allowed_leaves, depth + 1, current_path, results);
                    current_path.pop();
                }
            }
        }
    }
}