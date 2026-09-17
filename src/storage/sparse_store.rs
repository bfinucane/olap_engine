use serde::{Serialize, Deserialize};
use std::collections::HashSet;
use std::fmt::Write as _;
use crate::cube::node::{Node, CellValue};

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

// A quick debug printer for the Trie.
    // Appends the tree representation to `out` instead of printing to stdout.
    pub fn print_tree(&self, out: &mut String) {
        if self.root.children.is_empty() {
            let _ = writeln!(out, "    (Trie is empty)");
            return;
        }
        self.print_node(&self.root, &mut Vec::new(), out);
    }

    fn print_node(&self, node: &Node, path: &mut Vec<u32>, out: &mut String) {
        if let Some(val) = &node.value {
            let _ = writeln!(out, "    Coords: {:?} -> Value: {}", path, val);
        }
        for (id, child) in &node.children {
            path.push(*id);
            self.print_node(child, path, out);
            path.pop();
        }
    }

    // --- WRITE LOGIC ---
    pub fn write(&mut self, coords: &[u32], value: CellValue) {
        let mut current_node = &mut self.root;
        
        for &id in coords {
            // "entry().or_insert()" is the standard Rust way to 
            // get a value or create it if missing.
            current_node = current_node.children
                .entry(id)
                .or_default();
        }
        
        current_node.value = Some(value);
    }


// --- EXACT QUERY FOR JIT CALCULATION ---
    // Takes an exact path of leaf IDs and fetches the value. No wildcards.
    // Changed return type from f64 to Option<CellValue>
    pub fn query_exact(&self, coords: &[u32]) -> Option<CellValue> {
        let mut current_node = &self.root;
        
        for id in coords {
            {
                let child = current_node.children.get(id)?;
                current_node = child
            }
        }
        
        // Return a clone of the value if it exists, otherwise None
        current_node.value.clone()
    }



        /// Removes every stored cell whose coordinate at `dim_index` equals
    /// `target_id`. Used when a member is deleted from a dimension: all data
    /// referencing that member must disappear from every cube.
    /// Empty branches left behind by the deletion are pruned.
    pub fn remove_cells_with_id_at(&mut self, dim_index: usize, target_id: u32) {
        Self::prune_recursive(&mut self.root, dim_index, target_id, 0);
    }

    fn prune_recursive(node: &mut Node, dim_index: usize, target_id: u32, depth: usize) -> bool {
        if depth == dim_index {
            // At the member's dimension, drop the entire subtree under target_id.
            node.children.remove(&target_id);
        } else {
            // Recurse into existing children.
            for child in node.children.values_mut() {
                Self::prune_recursive(child, dim_index, target_id, depth + 1);
            }
        }

        // Prune children that became empty (no value and no descendants).
        node.children.retain(|_, child| child.value.is_some() || !child.children.is_empty());

        // Return whether this node is now empty (used by the parent's retain).
        node.value.is_none() && node.children.is_empty()
    }

    // --- SUB-CUBE SCANNER ---
    // allowed_leaves: For each dimension, a Set of valid IDs, or None to allow all.
    pub fn scan_subcube(&self, allowed_leaves: &[Option<HashSet<u32>>]) -> Vec<(Vec<u32>, CellValue)> {
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
        results: &mut Vec<(Vec<u32>, CellValue)>
    ) {
        // Base case: We reached the bottom of the tree
        if depth == allowed_leaves.len() {
            if let Some(val) = &node.value {
                results.push((current_path.clone(), val.clone()));
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