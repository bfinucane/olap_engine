use serde::{Serialize, Deserialize};
use std::collections::HashMap;

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

		// If this is the very first element added, it becomes the default!
        if self.default_member_id.is_none() {
            self.default_member_id = Some(id);
        }		
		
		
        id
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

    // Recursively prints an ASCII tree of the hierarchy

    pub fn print_tree(&self, member: &str, depth: usize, current_weight: f64) {
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
            // Print the weight right next to the node!
            println!("{}└─ {} {}{}", prefix, display_name, node_type, weight_str);

            if let Some(children) = self.consolidations.get(&id) {
                for &(child_id, child_weight) in children {
                    let child_name = self.get_name(child_id);
                    // Pass the child's weight into the recursive call
                    self.print_tree(&child_name, depth + 1, child_weight);
                }
            }
        } else {
            println!("{}└─ {} (Not Found)", prefix, member);
        }
    }

	pub fn get_id(&self, member: &str) -> Option<u32> {
        // Automatically lowercase any incoming query string
        self.member_to_id.get(&member.to_lowercase()).copied()
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