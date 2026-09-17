use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MemberType {
    Leaf,          // N-level in TM1
    Consolidated,  // C-level in TM1
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Dimension {
    pub name: String,
    
    // Dictionaries for fast lookup
    member_to_id: HashMap<String, u32>,
    id_to_name: HashMap<u32, String>,
    
    // Element Types
    member_types: HashMap<u32, MemberType>,

        // The Hierarchy: Parent ID -> Vec<(Child ID, Weight)>
    // Example: "Profit" (ID: 10) -> [("Revenue" (ID: 11), 1.0), ("Costs" (ID: 12), -1.0)]
    // The Vec preserves SIBLING ORDER (used for display ordering).
    consolidations: HashMap<u32, Vec<(u32, f64)>>,

    // Reverse index: Child ID -> its single Parent ID.
    // Enforces one-parent-per-hierarchy and makes membership tests O(1) instead
    // of scanning a parent's child Vec (which is O(N) per add, i.e. O(N^2) for
    // N children - ruinous for large imports).
    #[serde(default)]
    child_to_parent: HashMap<u32, u32>,
    
        next_id: u32,
	
	// Tracks the explicit default, or the first element added
	pub default_member_id: Option<u32>,

	// MODEL 1 (tree-native ordering):
	// The authoritative order of members is defined by the hierarchy itself.
	//   * `consolidations[parent]` is an ORDERED Vec of (child, weight),
	//     so sibling order lives in the tree.
	//   * `root_order` holds the order of top-level members (those with no
	//     parent). New members are appended, so it defaults to creation order.
	// `display_order` is a DERIVED, flat depth-first flattening of the tree
	// (roots in `root_order`, then each parent's children in sibling order).
	// It exists only so flat axes (e.g. a measure list) and run-time row
	// output have a stable, deterministic sequence. It is rebuilt from the
	// tree whenever the hierarchy changes; never edit it directly.
		#[serde(default)]
	root_order: Vec<u32>,
	#[serde(default)]
	display_order: Vec<u32>,
	// When true, `display_order` is stale and must be rebuilt from the tree
	// before it is read. Rebuilding on every structural change would make bulk
	// imports O(N^2); instead we mark dirty here and rebuild lazily on first
	// read. Not serialized: it is recomputed on load (see `dimension_hydrated`).
	#[serde(skip)]
	display_order_dirty: bool,
}

impl Dimension {

    // Converts an ID back to a String for tabular results
    pub fn get_name(&self, id: u32) -> String {
        self.id_to_name.get(&id).cloned().unwrap_or_else(|| "Unknown".to_string())
    }

    pub fn new(name: &str) -> Self {
        Dimension {
            name: name.to_string(),
            member_to_id: HashMap::new(),
            id_to_name: HashMap::new(),
                        member_types: HashMap::new(),
            consolidations: HashMap::new(),
            child_to_parent: HashMap::new(),
			next_id: 1,
			default_member_id: None, // Starts empty
                        root_order: Vec::new(),
            display_order: Vec::new(),
            display_order_dirty: false,
        }
    }

    // Creates or returns a Leaf member
    pub fn add_leaf(&mut self, name: &str) -> u32 {
        self.get_or_create(name, MemberType::Leaf)
    }

// Creates or returns a Consolidated member (Upgrades Leaves if necessary!)
    pub fn add_consolidated(&mut self, name: &str) -> u32 {
        let id = self.get_or_create(name, MemberType::Consolidated);
        
        // If this element already existed but was marked as a Leaf, 
        // we MUST upgrade it to a Parent now!
        self.member_types.insert(id, MemberType::Consolidated);
        
        id
    }

        // Connects a child to a parent (and prevents duplicates).
        //
        // ONE-PARENT-PER-HIERARCHY RULE: a member may belong to at most one parent.
        // If the child already has a DIFFERENT parent, it is automatically detached
        // from that old parent first (so `.rollup` re-parents rather than creating
        // a second parent). Returns the name of the old parent if a move happened,
        // so callers can report it; None if this was a plain add / weight update.
        pub fn add_component(&mut self, parent: &str, child: &str, weight: f64) -> Option<String> {
            let parent_id = self.add_consolidated(parent);
            let child_id = self.get_or_create(child, MemberType::Leaf);

            // Enforce the single-parent rule via the O(1) reverse index: if the
            // child already has a DIFFERENT parent, detach it from that one first.
            let mut moved_from: Option<String> = None;
            if let Some(&old_parent) = self.child_to_parent.get(&child_id) {
                if old_parent != parent_id {
                    moved_from = self.id_to_name.get(&old_parent).cloned();
                    if let Some(kids) = self.consolidations.get_mut(&old_parent) {
                        kids.retain(|(cid, _)| *cid != child_id);
                        if kids.is_empty() {
                            self.consolidations.remove(&old_parent);
                            self.member_types.insert(old_parent, MemberType::Leaf);
                        }
                    }
                }
            }

            let children = self.consolidations
                .entry(parent_id)
                .or_insert_with(Vec::new);

            // Membership test is O(1) via the reverse index; only when the child is
            // already a child of THIS parent do we scan the Vec to update its weight
            // (a rare case, not the bulk-import hot path).
            if self.child_to_parent.get(&child_id) == Some(&parent_id) {
                if let Some(existing) = children.iter_mut().find(|(id, _)| *id == child_id) {
                    existing.1 = weight; // Update weight just in case it changed
                }
            } else {
                children.push((child_id, weight));
                self.child_to_parent.insert(child_id, parent_id);
            }

            // The child now has a parent, so it is no longer a top-level root.
            self.root_order.retain(|&x| x != child_id);
            self.mark_display_order_dirty();
            moved_from
        }

    /// Detaches `child` from `parent` in the hierarchy - the inverse of
    /// `add_component`, viewed from either endpoint ("remove the parent from
    /// this member" / "remove this member from the parent").
    ///
    /// Errors if the parent does not exist, is not a consolidation, or has no
    /// such child. Afterwards the affected members' types are recomputed: a
    /// parent left with no children reverts to a Leaf.
    pub fn remove_component(&mut self, parent: &str, child: &str) -> Result<(), String> {
        let parent_id = self.get_id(parent)
            .ok_or_else(|| format!("Member '{}' does not exist in dimension '{}'.", parent, self.name))?;
        let child_id = self.get_id(child)
            .ok_or_else(|| format!("Member '{}' does not exist in dimension '{}'.", child, self.name))?;

        if self.member_types.get(&parent_id) != Some(&MemberType::Consolidated) {
            return Err(format!("Member '{}' is not a parent in dimension '{}'.", parent, self.name));
        }

                // Remove the relationship (single-parent rule: just remove the one).
        let removed = {
            let children = self.consolidations
                .get_mut(&parent_id)
                .ok_or_else(|| format!("Member '{}' has no children.", parent))?;
            let before = children.len();
            children.retain(|(id, _)| *id != child_id);
            let removed = children.len() != before;
            if removed && children.is_empty() {
                // A parent that lost its last child is no longer a consolidation.
                self.consolidations.remove(&parent_id);
                self.member_types.insert(parent_id, MemberType::Leaf);
            }
            removed
        };

        if !removed {
            return Err(format!("'{}' is not a child of '{}'.", child, parent));
        }

        // Drop the reverse-index entry so the child becomes a root again.
        self.child_to_parent.remove(&child_id);
        if !self.root_order.contains(&child_id) {
            self.root_order.push(child_id);
        }
        self.mark_display_order_dirty();
        Ok(())
    }

    /// Deletes a member entirely from this dimension.
    ///
    /// Effects:
    ///   * the member is removed from every dictionary and from the display
    ///     order,
    ///   * its own children (if any) are DETACHED and pop to the top level -
    ///     they keep their own type and sub-trees but lose this parent,
    ///   * it is detached from every parent that references it; any parent left
    ///     with no children reverts to a Leaf,
    ///   * if it was the default member, the default falls back to a remaining
    ///     member (or None if the dimension becomes empty).
    ///
    /// Returns the removed member's id so callers (e.g. the Catalog) can purge
    /// any data cells that referenced it. Purging data is the caller's job
    /// because the same dimension may be shared by several cubes.
    pub fn delete_member(&mut self, name: &str) -> Result<u32, String> {
        let id = self.get_id(name)
            .ok_or_else(|| format!("Member '{}' does not exist in dimension '{}'.", name, self.name))?;

                // 1. Detach this member from its (single) parent, if any.
        if let Some(&parent_id) = self.child_to_parent.get(&id) {
            let emptied = {
                if let Some(children) = self.consolidations.get_mut(&parent_id) {
                    children.retain(|(cid, _)| *cid != id);
                    children.is_empty()
                } else {
                    false
                }
            };
            if emptied {
                self.consolidations.remove(&parent_id);
                self.member_types.insert(parent_id, MemberType::Leaf);
            }
            self.child_to_parent.remove(&id);
        }

        // 2. Drop the member's own consolidation entry. Its children are not
        //    removed; they simply have no parent left and pop to the top level.
        let orphaned_children: Vec<u32> = self.consolidations
            .get(&id)
            .map(|cs| cs.iter().map(|(c, _)| *c).collect())
            .unwrap_or_default();
        self.consolidations.remove(&id);
        // Each orphan loses its reverse-index entry and becomes a root.
        for child_id in &orphaned_children {
            self.child_to_parent.remove(child_id);
        }

        // 3. Remove from all dictionaries, the root list, and the display order.
        let key = name.to_lowercase();
        self.member_to_id.remove(&key);
        self.id_to_name.remove(&id);
        self.member_types.remove(&id);
        self.root_order.retain(|&x| x != id);

        // Children that are now parentless become roots so they stay visible.
        for child_id in orphaned_children {
            if !self.root_order.contains(&child_id) {
                self.root_order.push(child_id);
            }
        }
                self.mark_display_order_dirty();

        // 4. Fix the default member if it pointed at the deleted member.
        if self.default_member_id == Some(id) {
            self.ensure_display_order();
            self.default_member_id = self.root_order.first().copied()
                .or_else(|| self.display_order.first().copied());
        }

        Ok(id)
    }

    fn get_or_create(&mut self, name: &str, m_type: MemberType) -> u32 {
        let key = name.to_lowercase(); // The hidden lookup key

        if let Some(&id) = self.member_to_id.get(&key) {
            return id;
        }

                                let id = self.next_id;
        self.member_to_id.insert(key, id);            // Save lowercase for fast lookups
        self.id_to_name.insert(id, name.to_string()); // Save original casing for output formatting!
        self.member_types.insert(id, m_type);
        self.next_id += 1;

        // A brand-new member has no parent yet, so it is a root. Roots default
        // to creation order, which we preserve simply by appending.
        self.root_order.push(id);
        self.mark_display_order_dirty();

		// If this is the very first element added, it becomes the default!
        if self.default_member_id.is_none() {
            self.default_member_id = Some(id);
        }		
		
		
        id
    }

        /// Moves a member from `root_order` into (or out of) the tree is implicit:
    /// a member is a root iff no parent lists it as a child. O(1) via the
    /// reverse index.
    fn parent_of(&self, id: u32) -> Option<u32> {
        self.child_to_parent.get(&id).copied()
    }

            /// Rebuilds the flat `display_order` as a depth-first flattening of the
    /// tree: roots in `root_order`, then each parent's children in sibling
    /// order. Any member not reachable from a root (should not happen in a
    /// well-formed tree, but can occur with loaded legacy data) is appended at
    /// the end so it is never lost. O(members).
    fn rebuild_display_order(&mut self) {
        let mut flat = Vec::with_capacity(self.member_to_id.len());
        let roots = self.root_order.clone();
        for rid in roots {
            self.flatten_dfs(rid, &mut flat);
        }
        // Safety net: include any member not visited. O(1) membership via a set.
        if flat.len() != self.member_to_id.len() {
            let seen: std::collections::HashSet<u32> = flat.iter().copied().collect();
            for &id in self.id_to_name.keys() {
                if !seen.contains(&id) {
                    flat.push(id);
                }
            }
        }
        self.display_order = flat;
        self.display_order_dirty = false;
    }

    /// Marks the derived display order stale. Cheap (O(1)); the rebuild is
    /// deferred to `ensure_display_order`, so bulk imports that touch thousands
    /// of cells do not pay O(N) per insert.
    fn mark_display_order_dirty(&mut self) {
        self.display_order_dirty = true;
    }

        /// Rebuilds the display order if it is stale. Call this at read boundaries
    /// (before sorting query rows, printing `.order`, etc.).
    pub fn ensure_display_order(&mut self) {
        if self.display_order_dirty {
            self.rebuild_display_order();
        }
    }

    /// Unconditionally rebuilds the derived display order. Used after loading
    /// from disk, where the (non-serialized) dirty flag cannot be trusted.
    pub fn refresh_display_order(&mut self) {
        self.rebuild_display_order();
    }

    fn flatten_dfs(&self, id: u32, out: &mut Vec<u32>) {
        out.push(id);
        if let Some(children) = self.consolidations.get(&id) {
            for &(child_id, _) in children {
                self.flatten_dfs(child_id, out);
            }
        }
    }


            /// Returns the presentation rank of a member id: its index in the
    /// depth-first flattening of the tree. Unknown ids sort last. Rows in query
    /// results are ordered by this rank, giving deterministic output that
    /// defaults to creation order but can be authored at design time.
    pub fn display_order_rank(&self, id: u32) -> usize {
        self.display_order
            .iter()
            .position(|&x| x == id)
            .unwrap_or(usize::MAX)
    }

    /// Returns the current depth-first display order as member names.
    pub fn member_order_names(&self) -> Vec<String> {
        self.display_order.iter().map(|&id| self.get_name(id)).collect()
    }

    /// Moves `name` so it appears immediately before `reference` in the
    /// TREE order. They must be siblings (share a parent, or both be roots);
    /// moving across different parents is rejected rather than silently
    /// corrupting the hierarchy.
    pub fn move_member_before(&mut self, name: &str, reference: &str) -> Result<(), String> {
        let id = self.get_id(name)
            .ok_or_else(|| format!("Member '{}' does not exist in dimension '{}'.", name, self.name))?;
        let ref_id = self.get_id(reference)
            .ok_or_else(|| format!("Member '{}' does not exist in dimension '{}'.", reference, self.name))?;
        if id == ref_id {
            return Ok(()); // No-op: moving a member before itself.
        }

        let id_parent = self.parent_of(id);
        let ref_parent = self.parent_of(ref_id);
        if id_parent != ref_parent {
            return Err(format!(
                "'{}' and '{}' are not siblings in dimension '{}', so they cannot be reordered relative to each other. (Re-parenting is a separate operation.)",
                name, reference, self.name
            ));
        }

        match id_parent {
            // Both are children of the same consolidation: reorder that Vec.
            Some(pid) => {
                let children = self.consolidations.get_mut(&pid)
                    .expect("parent must have a children entry");
                let moved = children.iter().position(|(c, _)| *c == id)
                    .map(|i| children.remove(i))
                    .expect("member must be a child of its parent");
                let pos = children.iter().position(|(c, _)| *c == ref_id)
                    .expect("reference sibling must be present");
                children.insert(pos, moved);
            }
            // Both are roots: reorder root_order.
            None => {
                self.root_order.retain(|&x| x != id);
                let pos = self.root_order.iter().position(|&x| x == ref_id)
                    .expect("reference root must be present");
                self.root_order.insert(pos, id);
            }
        }

        self.mark_display_order_dirty();
        Ok(())
    }

    /// Moves `name` to the first position among its siblings (first child of
    /// its parent, or first root).
    pub fn move_member_to_front(&mut self, name: &str) -> Result<(), String> {
        let id = self.get_id(name)
            .ok_or_else(|| format!("Member '{}' does not exist in dimension '{}'.", name, self.name))?;

        match self.parent_of(id) {
            Some(pid) => {
                let children = self.consolidations.get_mut(&pid)
                    .expect("parent must have a children entry");
                let moved = children.iter().position(|(c, _)| *c == id)
                    .map(|i| children.remove(i))
                    .expect("member must be a child of its parent");
                children.insert(0, moved);
            }
            None => {
                self.root_order.retain(|&x| x != id);
                self.root_order.insert(0, id);
            }
        }

        self.mark_display_order_dirty();
        Ok(())
    }

    // Allows a user to explicitly change the default member
    pub fn set_default_member(&mut self, name: &str) -> Result<(), String> {
        if let Some(id) = self.get_id(name) {
            self.default_member_id = Some(id);
            Ok(())
        } else {
            Err(format!("Member '{}' does not exist in dimension '{}'", name, self.name))
        }
    }
    
    // Safely fetches the name of the default member
    pub fn get_default_member_name(&self) -> Option<String> {
        self.default_member_id.map(|id| self.get_name(id))
    }



	// Returns how many unique strings are stored in this dimension
    pub fn len(&self) -> usize {
        self.member_to_id.len()
    }

        // Recursively appends an ASCII tree of the hierarchy to `out`.
    // (Previously printed to stdout; now the caller controls the destination.)
    pub fn print_tree(&self, member: &str, depth: usize, current_weight: f64, out: &mut String) {
        let prefix = "  ".repeat(depth);
        let key = member.to_lowercase(); 
        
        if let Some(&id) = self.member_to_id.get(&key) {
            let node_type = match self.member_types.get(&id) {
                Some(MemberType::Leaf) => "(Leaf)",
                Some(MemberType::Consolidated) => "(Parent)",
                None => "(Unknown)",
            };
            
            // Format the weight if it's not exactly 1.0
            let weight_str = if (current_weight - 1.0).abs() > 0.001 {
                format!(" [w: {}]", current_weight)
            } else {
                "".to_string()
            };
            
            let display_name = self.get_name(id);
            // Write the weight right next to the node!
            let _ = writeln!(out, "{}└─ {} {}{}", prefix, display_name, node_type, weight_str);

            if let Some(children) = self.consolidations.get(&id) {
                for &(child_id, child_weight) in children {
                    let child_name = self.get_name(child_id);
                    // Pass the child's weight into the recursive call
                    self.print_tree(&child_name, depth + 1, child_weight, out);
                }
            }
        } else {
            let _ = writeln!(out, "{}└─ {} (Not Found)", prefix, member);
        }
    }

	pub fn get_id(&self, member: &str) -> Option<u32> {
	    // Automatically lowercase any incoming query string
	    self.member_to_id.get(&member.to_lowercase()).copied()
		}

	/// Returns the member type (Leaf / Consolidated) for a raw member id.
	pub fn member_type(&self, id: u32) -> Option<MemberType> {
	    self.member_types.get(&id).cloned()
	}

	/// True when the named member exists and is a Consolidated (aggregated) node.
	/// Used by the splash logic to decide whether a write must be allocated down.
	pub fn is_consolidated(&self, member: &str) -> bool {
	    matches!(self.get_id(member).and_then(|id| self.member_type(id)), Some(MemberType::Consolidated))
	}


    /// THE MAGIC: Resolves any member into a list of its LEAF descendants and their aggregated weights.
    /// If you pass a Leaf, it just returns itself with weight 1.0.
    /// If you pass a Ragged Hierarchy parent, it navigates the graph to find all base leaves.
    pub fn get_leaf_descendants(&self, member: &str) -> HashMap<u32, f64> {
        let mut leaves = HashMap::new();
        
        if let Some(start_id) = self.get_id(member) {
            self.resolve_recursive(start_id, 1.0, &mut leaves);
        }
        
        leaves
    }

    fn resolve_recursive(&self, current_id: u32, current_weight: f64, leaves: &mut HashMap<u32, f64>) {
        if self.member_types.get(&current_id) == Some(&MemberType::Leaf) {
            // It's a base element. Add to our map, or add to existing weight.
            *leaves.entry(current_id).or_insert(0.0) += current_weight;
        } else if let Some(children) = self.consolidations.get(&current_id) {
            // It's a consolidation. Recurse down.
            for &(child_id, child_weight) in children {
                self.resolve_recursive(child_id, current_weight * child_weight, leaves);
            }
        }
    }
}