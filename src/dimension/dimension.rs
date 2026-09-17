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
    consolidations: HashMap<u32, Vec<(u32, f64)>>,
    
        next_id: u32,
	
	// Tracks the explicit default, or the first element added
    pub default_member_id: Option<u32>,

    // The authoritative presentation order of members (by ID). New members
    // are appended, so by default this is creation order. A cube author can
    // reorder it at design time (see `set_display_order` / `move_member`),
    // and query results honour this order for their rows.
    #[serde(default)]
    display_order: Vec<u32>,
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
                        next_id: 1,
			default_member_id: None, // Starts empty
            display_order: Vec::new(),
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

    // Connects a child to a parent (and prevents duplicates)
    pub fn add_component(&mut self, parent: &str, child: &str, weight: f64) {
        let parent_id = self.add_consolidated(parent);
        let child_id = self.get_or_create(child, MemberType::Leaf); 

        let children = self.consolidations
            .entry(parent_id)
            .or_insert_with(Vec::new);

        // Prevent Duplicate Relationships!
                // If the child is already linked to this parent, just update the weight.
        if let Some(existing_child) = children.iter_mut().find(|(id, _)| *id == child_id) {
            existing_child.1 = weight; // Update weight just in case it changed
        } else {
            // Otherwise, add the new child
            children.push((child_id, weight));
        }
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

        let children = self.consolidations
            .get_mut(&parent_id)
            .ok_or_else(|| format!("Member '{}' has no children.", parent))?;

        let before = children.len();
        children.retain(|(id, _)| *id != child_id);
        if children.len() == before {
            return Err(format!("'{}' is not a child of '{}'.", child, parent));
        }

        // A parent that lost its last child is no longer a consolidation.
        if children.is_empty() {
            self.consolidations.remove(&parent_id);
            self.member_types.insert(parent_id, MemberType::Leaf);
        }
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

        // 1. Detach this member from every parent that lists it as a child.
        let mut emptied_parents: Vec<u32> = Vec::new();
        for (&parent_id, children) in self.consolidations.iter_mut() {
            let before = children.len();
            children.retain(|(cid, _)| *cid != id);
            if children.is_empty() && before > 0 {
                emptied_parents.push(parent_id);
            }
        }
        // A parent that lost its last child is no longer a consolidation.
        for parent_id in emptied_parents {
            self.consolidations.remove(&parent_id);
            self.member_types.insert(parent_id, MemberType::Leaf);
        }

        // 2. Drop the member's own consolidation entry. Its children are not
        //    removed; they simply have no parent left and pop to the top level.
        self.consolidations.remove(&id);

        // 3. Remove from all dictionaries and the display order.
        let key = name.to_lowercase();
        self.member_to_id.remove(&key);
        self.id_to_name.remove(&id);
        self.member_types.remove(&id);
        self.display_order.retain(|&x| x != id);

        // 4. Fix the default member if it pointed at the deleted member.
        if self.default_member_id == Some(id) {
            self.default_member_id = self.display_order.first().copied();
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

        // New members are appended, so display order defaults to creation order.
        self.display_order.push(id);

		// If this is the very first element added, it becomes the default!
        if self.default_member_id.is_none() {
            self.default_member_id = Some(id);
        }		
		
		
        id
    }

    /// Returns the presentation rank of a member id: its index in the
    /// authoritative display order. Unknown ids sort last. Rows in query
    /// results are ordered by this rank, giving deterministic output that
    /// defaults to creation order but can be authored at design time.
    pub fn display_order_rank(&self, id: u32) -> usize {
        self.display_order
            .iter()
            .position(|&x| x == id)
            .unwrap_or(usize::MAX)
    }

    /// Returns the current display order as member names (handy for output).
    pub fn member_order_names(&self) -> Vec<String> {
        self.display_order.iter().map(|&id| self.get_name(id)).collect()
    }

    /// Moves `name` so that it appears immediately before `reference` in the
    /// display order. Both members must already exist. This is a design-time
    /// authoring operation (the order is persisted and used at run time).
    pub fn move_member_before(&mut self, name: &str, reference: &str) -> Result<(), String> {
        let id = self.get_id(name)
            .ok_or_else(|| format!("Member '{}' does not exist in dimension '{}'.", name, self.name))?;
        let ref_id = self.get_id(reference)
            .ok_or_else(|| format!("Member '{}' does not exist in dimension '{}'.", reference, self.name))?;
        if id == ref_id {
            return Ok(()); // No-op: moving a member before itself.
        }

        self.display_order.retain(|&x| x != id);
        let pos = self.display_order.iter().position(|&x| x == ref_id)
            .expect("reference member must be present in display order");
        self.display_order.insert(pos, id);
        Ok(())
    }

    /// Moves `name` to the very front of the display order.
    pub fn move_member_to_front(&mut self, name: &str) -> Result<(), String> {
        let id = self.get_id(name)
            .ok_or_else(|| format!("Member '{}' does not exist in dimension '{}'.", name, self.name))?;
        self.display_order.retain(|&x| x != id);
        self.display_order.insert(0, id);
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