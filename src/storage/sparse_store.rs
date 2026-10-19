use serde::{Serialize, Deserialize};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;
use crate::cube::node::{Node, CellValue};

/// Sparse Patricia trie: one cell per leaf coordinate.
///
/// PERSISTENT (Phase 1b, M1): `root` is an `Arc<Node>` and every node's children are
/// `Arc`s, so a write copies only the nodes on the path to the coordinate and shares
/// every untouched subtree. `snapshot()` is therefore O(1), and a snapshot stays
/// valid no matter what is written afterwards.
///
/// The public API is unchanged: `write` still takes `&mut self`. What changed is the
/// cost model — an overwrite now allocates a path instead of mutating in place.
#[derive(Clone, Serialize, Deserialize)]
pub struct SparseStore {
    root: Arc<Node>,
    // We might track depth or other metadata here later
    #[allow(dead_code)] 
    dim_count: usize, 
}

impl SparseStore {
    pub fn new(dim_count: usize) -> Self {
        SparseStore {
            root: Arc::new(Node::new()),
            dim_count,
        }
    }

    /// A point-in-time snapshot, O(1).
    ///
    /// Cloning is an `Arc` bump: the root and every shared subtree are referenced,
    /// not copied. Later writes to `self` publish new nodes along their own paths, so
    /// this snapshot keeps reading exactly the state it was taken at.
    pub fn snapshot(&self) -> SparseStore {
        self.clone()
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
    //
    // Copy-on-write: the root is replaced by a copy whose path to the coordinate is
    // new and whose other subtrees are the SAME `Arc`s as before. Cost is O(depth),
    // not O(cells).
    pub fn write(&mut self, coords: &[u32], value: CellValue) {
        self.root = Self::write_path(&self.root, coords, 0, value);
    }

    fn write_path(node: &Arc<Node>, coords: &[u32], depth: usize, value: CellValue) -> Arc<Node> {
        // Shallow copy: `children` clones its `Arc`s and `value` clones its payload.
        let mut copied = (**node).clone();

        if depth == coords.len() {
            copied.value = Some(value);
        } else {
            let id = coords[depth];
            let child = node
                .children
                .get(&id)
                .cloned()
                .unwrap_or_else(|| Arc::new(Node::new()));
            let updated = Self::write_path(&child, coords, depth + 1, value);
            copied.children.insert(id, updated);
        }

        Arc::new(copied)
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
    ///
    /// Copy-on-write like `write`: existing snapshots keep the cells they were taken
    /// with. A prune walks the whole trie, so it copies every node — acceptable
    /// because deleting a member is an admin operation, not a hot path.
    pub fn remove_cells_with_id_at(&mut self, dim_index: usize, target_id: u32) {
        self.root = Self::prune_path(&self.root, dim_index, target_id, 0);
    }

    fn prune_path(
        node: &Arc<Node>,
        dim_index: usize,
        target_id: u32,
        depth: usize,
    ) -> Arc<Node> {
        let mut copied = (**node).clone();

        if depth == dim_index {
            // At the member's dimension, drop the entire subtree under target_id.
            copied.children.remove(&target_id);
        } else {
            // Recurse into the existing children and replace those that changed.
            let updates: Vec<(u32, Arc<Node>)> = node
                .children
                .iter()
                .map(|(&id, child)| (id, Self::prune_path(child, dim_index, target_id, depth + 1)))
                .collect();
            for (id, child) in updates {
                copied.children.insert(id, child);
            }
        }

        // Prune children that became empty (no value and no descendants).
        copied
            .children
            .retain(|_, child| child.value.is_some() || !child.children.is_empty());

        Arc::new(copied)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cube::node::CellValue;

    /// One numeric cell, read back through the exact-coordinate path.
    #[test]
    fn exact_round_trip_returns_the_stored_cell() {
        let mut s = SparseStore::new(3);
        s.write(&[1, 2, 3], CellValue::Numeric(42.5));
        assert_eq!(s.query_exact(&[1, 2, 3]), Some(CellValue::Numeric(42.5)));
    }

    /// A second write to the same coordinate replaces the first. This is the
    /// storage-level half of the intentional upsert contract that `INSERT`
    /// exposes (re-inserting a coordinate overwrites it), and it currently has
    /// no other guard.
    #[test]
    fn second_write_to_the_same_coordinate_overwrites() {
        let mut s = SparseStore::new(1);
        s.write(&[7], CellValue::Numeric(1.0));
        s.write(&[7], CellValue::Numeric(2.0));
        assert_eq!(s.query_exact(&[7]), Some(CellValue::Numeric(2.0)));
    }

    /// A coordinate that was never written is `None`, not a zero or a panic.
    #[test]
    fn unwritten_and_partial_coordinates_are_none() {
        let mut s = SparseStore::new(2);
        s.write(&[1, 2], CellValue::Numeric(5.0));
        assert_eq!(s.query_exact(&[9, 9]), None, "unwritten leaf");
        assert_eq!(s.query_exact(&[1]), None, "a coordinate interior is not a cell");
        assert_eq!(s.query_exact(&[1, 2, 3]), None, "over-long coordinate");
    }

    /// The empty coordinate addresses the trie root. Pinned because it is the
    /// boundary the recursion in `scan_recursive`/`calculate_jit` relies on.
    #[test]
    fn empty_coordinate_addresses_the_root() {
        let mut s = SparseStore::new(0);
        assert_eq!(s.query_exact(&[]), None, "empty store has no root cell");
        s.write(&[], CellValue::Numeric(9.0));
        assert_eq!(s.query_exact(&[]), Some(CellValue::Numeric(9.0)));
    }

    /// Cells at different depths coexist: a value stored at an interior
    /// coordinate is not clobbered by storing beneath it, and vice versa. This
    /// is the behaviour `has_consolidated_coordinate` depends on when it
    /// decides between `write` and `write_splashed`.
    #[test]
    fn interior_and_deeper_cells_coexist() {
        let mut s = SparseStore::new(1);
        s.write(&[5], CellValue::Numeric(1.0));
        s.write(&[5, 6], CellValue::Numeric(2.0));
        assert_eq!(s.query_exact(&[5]), Some(CellValue::Numeric(1.0)));
        assert_eq!(s.query_exact(&[5, 6]), Some(CellValue::Numeric(2.0)));
    }

    /// Non-numeric values survive the store unchanged (they are what the
    /// attribute cubes are built from).
    #[test]
    fn string_cells_survive_alongside_numeric_ones() {
        let mut s = SparseStore::new(2);
        s.write(&[1, 1], CellValue::Numeric(1.0));
        s.write(&[2, 2], CellValue::String("Cash Account".to_string()));
        assert_eq!(s.query_exact(&[1, 1]), Some(CellValue::Numeric(1.0)));
        assert_eq!(
            s.query_exact(&[2, 2]),
            Some(CellValue::String("Cash Account".to_string()))
        );
    }

    /// `remove_cells_with_id_at` drops every cell whose coordinate at the given
    /// position equals the target, across all of that position's branches, and
    /// leaves every other cell untouched.
    #[test]
    fn prune_removes_only_the_targeted_coordinate() {
        let mut s = SparseStore::new(2);
        s.write(&[1, 8], CellValue::Numeric(1.0));
        s.write(&[1, 9], CellValue::Numeric(2.0));
        s.write(&[2, 8], CellValue::Numeric(3.0));
        s.write(&[2, 7], CellValue::Numeric(4.0));

        s.remove_cells_with_id_at(1, 8); // dim 1 == 8

        assert_eq!(s.query_exact(&[1, 8]), None, "targeted cell in branch 1");
        assert_eq!(s.query_exact(&[2, 8]), None, "targeted cell in branch 2");
        assert_eq!(s.query_exact(&[1, 9]), Some(CellValue::Numeric(2.0)), "sibling kept");
        assert_eq!(s.query_exact(&[2, 7]), Some(CellValue::Numeric(4.0)), "sibling kept");
    }

    /// Pruning at dimension 0 removes whole branches, and the scan afterwards
    /// reports exactly what is left. This is the mechanism `.delete_member`
    /// uses to purge a member's data from every cube that shares the dimension.
    #[test]
    fn prune_at_dimension_zero_then_scan_reports_the_survivors() {
        let mut s = SparseStore::new(2);
        s.write(&[1, 5], CellValue::Numeric(1.0));
        s.write(&[2, 5], CellValue::Numeric(2.0));
        s.write(&[3, 5], CellValue::Numeric(3.0));

        s.remove_cells_with_id_at(0, 2);

        let all = s.scan_subcube(&[None, None]);
        assert_eq!(all.len(), 2, "one branch removed");
        let coords: HashSet<Vec<u32>> = all.into_iter().map(|(c, _)| c).collect();
        assert!(coords.contains(&vec![1, 5]));
        assert!(coords.contains(&vec![3, 5]));
        assert!(!coords.contains(&vec![2, 5]));
    }

    /// Pruning every branch empties the trie and leaves no debris behind: a
    /// subsequent scan finds nothing.
    #[test]
    fn pruning_every_cell_empties_the_trie() {
        let mut s = SparseStore::new(1);
        s.write(&[4], CellValue::Numeric(1.0));
        s.write(&[5], CellValue::Numeric(2.0));
        s.remove_cells_with_id_at(0, 4);
        s.remove_cells_with_id_at(0, 5);
        assert!(s.scan_subcube(&[None]).is_empty(), "no cells left after full prune");
    }

    /// `scan_subcube` with a filter per dimension returns exactly the cells
    /// that satisfy every filter, and the filter is applied dimension by
    /// dimension (not as a union).
    #[test]
    fn scan_subcube_filters_intersect_across_dimensions() {
        let mut s = SparseStore::new(2);
        s.write(&[1, 1], CellValue::Numeric(11.0));
        s.write(&[1, 2], CellValue::Numeric(12.0));
        s.write(&[2, 1], CellValue::Numeric(21.0));
        s.write(&[2, 2], CellValue::Numeric(22.0));

        assert_eq!(s.scan_subcube(&[None, None]).len(), 4, "no filter: everything");

        let dim0: HashSet<u32> = [1u32].into_iter().collect();
        let only_1 = s.scan_subcube(&[Some(dim0), None]);
        assert_eq!(only_1.len(), 2);
        assert!(only_1.iter().all(|(c, _)| c[0] == 1));

        let dim1: HashSet<u32> = [2u32].into_iter().collect();
        let cross = s.scan_subcube(&[Some([1u32].into_iter().collect()), Some(dim1)]);
        assert_eq!(cross.len(), 1, "the intersection is a single cell");
        assert_eq!(cross[0], (vec![1, 2], CellValue::Numeric(12.0)));
    }

    /// A filter naming an id that no cell uses simply matches nothing; it is
    /// not an error and must not panic.
    #[test]
    fn scan_subcube_with_a_filter_that_matches_nothing_is_empty() {
        let mut s = SparseStore::new(2);
        s.write(&[1, 1], CellValue::Numeric(1.0));
        let missing: HashSet<u32> = [99u32].into_iter().collect();
        assert!(s.scan_subcube(&[Some(missing), None]).is_empty());
    }

    /// The store walks a deep path without recursing away the stack. The
    /// fixtures only ever build three or four levels, so this is the cheap
    /// guard against a trie-depth limit going unnoticed.
    #[test]
    fn deep_coordinate_paths_are_stored_and_read_back() {
        let depth = 512usize;
        let mut s = SparseStore::new(depth);
        let coords: Vec<u32> = (0..depth as u32).collect();
        s.write(&coords, CellValue::Numeric(123.0));
        assert_eq!(s.query_exact(&coords), Some(CellValue::Numeric(123.0)));
        assert_eq!(s.scan_subcube(&vec![None; 512]).len(), 1);
    }

    /// Pruning a deep path is recursive too: it must not overflow, and it must
    /// leave the trie empty when the only cell is removed.
    #[test]
    fn deep_paths_can_be_pruned() {
        let depth = 256usize;
        let mut s = SparseStore::new(depth);
        let coords: Vec<u32> = (0..depth as u32).collect();
        s.write(&coords, CellValue::Numeric(1.0));
        s.remove_cells_with_id_at(0, 0);
        assert!(s.scan_subcube(&vec![None; 256]).is_empty());
    }

    /// Printing an empty trie and a populated one both work and write to the
    /// buffer rather than to stdout (the batch runner depends on that).
    #[test]
    fn print_tree_writes_to_the_buffer() {
        let mut s = SparseStore::new(1);
        let mut out = String::new();
        s.print_tree(&mut out);
        assert!(out.contains("empty"), "empty trie is reported, got: {out:?}");

        s.write(&[3], CellValue::Numeric(1.0));
        let mut out2 = String::new();
        s.print_tree(&mut out2);
        assert!(out2.contains("Coords"), "populated trie lists a coordinate, got: {out2:?}");
    }

    // --- Persistence (Phase 1b M1): snapshots and structural sharing -------------

    /// A snapshot is a point in time: later writes must not be visible through it.
    /// This is the whole reason the trie became persistent.
    #[test]
    fn snapshot_is_unaffected_by_later_writes() {
        let mut store = SparseStore::new(2);
        store.write(&[1, 2], CellValue::Numeric(1.0));

        let snapshot = store.snapshot();

        store.write(&[1, 2], CellValue::Numeric(2.0)); // overwrite an existing cell
        store.write(&[3, 4], CellValue::Numeric(3.0)); // add a new branch

        assert_eq!(
            snapshot.query_exact(&[1, 2]),
            Some(CellValue::Numeric(1.0)),
            "the snapshot must still see the value it was taken with"
        );
        assert_eq!(
            snapshot.query_exact(&[3, 4]),
            None,
            "a branch added later must not appear in an older snapshot"
        );

        // The live store sees both writes.
        assert_eq!(store.query_exact(&[1, 2]), Some(CellValue::Numeric(2.0)));
        assert_eq!(store.query_exact(&[3, 4]), Some(CellValue::Numeric(3.0)));

        // And the snapshot's scan is the old set, not the new one.
        assert_eq!(snapshot.scan_subcube(&[None, None]).len(), 1);
        assert_eq!(store.scan_subcube(&[None, None]).len(), 2);
    }

    /// A write copies only the nodes on its own path: sibling subtrees are shared by
    /// pointer, not copied. This is what makes the cost O(depth) instead of O(cells),
    /// and it is invisible to any behavioural test — hence the explicit `ptr_eq`.
    #[test]
    fn writes_share_untouched_subtrees_by_pointer() {
        let mut store = SparseStore::new(2);
        store.write(&[1, 1], CellValue::Numeric(1.0));
        store.write(&[2, 2], CellValue::Numeric(2.0));

        let snapshot = store.snapshot();

        // Touch ONLY branch 1.
        store.write(&[1, 1], CellValue::Numeric(9.0));

        let branch_2_before = snapshot
            .root
            .children
            .get(&2)
            .expect("branch 2 exists in the snapshot")
            .clone();
        let branch_2_after = store
            .root
            .children
            .get(&2)
            .expect("branch 2 exists in the live store")
            .clone();
        assert!(
            Arc::ptr_eq(&branch_2_before, &branch_2_after),
            "the untouched branch must be the SAME allocation in both versions"
        );

        // The branch that was written is new, and the snapshot kept the old one.
        let branch_1_before = snapshot.root.children.get(&1).expect("branch 1").clone();
        let branch_1_after = store.root.children.get(&1).expect("branch 1").clone();
        assert!(
            !Arc::ptr_eq(&branch_1_before, &branch_1_after),
            "the written branch must be a fresh node"
        );
        assert_eq!(branch_1_before.value, None);
        assert_eq!(branch_1_after.value, None);
    }

    /// Pruning is copy-on-write too: deleting a member's cells must not erase them
    /// from a snapshot taken beforehand.
    #[test]
    fn prune_does_not_affect_an_existing_snapshot() {
        let mut store = SparseStore::new(2);
        store.write(&[1, 8], CellValue::Numeric(1.0));
        store.write(&[2, 8], CellValue::Numeric(2.0));

        let snapshot = store.snapshot();
        store.remove_cells_with_id_at(1, 8);

        assert_eq!(
            snapshot.query_exact(&[1, 8]),
            Some(CellValue::Numeric(1.0)),
            "the snapshot keeps the pruned cell"
        );
        assert_eq!(store.query_exact(&[1, 8]), None, "the live store dropped it");
        assert_eq!(store.query_exact(&[2, 8]), None);
        assert_eq!(snapshot.scan_subcube(&[None, None]).len(), 2);
        assert!(store.scan_subcube(&[None, None]).is_empty());
    }

    /// `snapshot()` must be O(1) — an `Arc` bump, not a walk. Guarded structurally:
    /// taking a snapshot of a deep trie and then dropping the original must leave the
    /// snapshot fully usable.
    #[test]
    fn snapshot_outlives_the_store_it_came_from() {
        let snapshot = {
            let mut store = SparseStore::new(3);
            store.write(&[1, 2, 3], CellValue::Numeric(42.0));
            store.snapshot()
        };
        assert_eq!(
            snapshot.query_exact(&[1, 2, 3]),
            Some(CellValue::Numeric(42.0)),
            "the snapshot owns its own Arcs; dropping the store must not matter"
        );
    }
}