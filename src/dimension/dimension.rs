use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::fmt::Write as _;

use super::hierarchy::Hierarchy;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MemberType {
    Leaf,          // N-level in TM1
    Consolidated,  // C-level in TM1
}

/// A dimension is a named set of BASE (leaf) members plus one or more
/// HIERARCHIES that aggregate those same leaves in different ways.
///
/// DESIGN (multiple hierarchies per dimension):
///   * Base elements (leaves) live ONCE, in the shared leaf bank below. They
///     are automatically members of every hierarchy of the dimension, so a
///     fact written against a leaf is visible through every hierarchy that
///     reaches it.
///   * Aggregated elements are EXCLUSIVE to a single hierarchy. Two
///     hierarchies may each define a member named `All`; they are distinct
///     members (distinct ids) in the dimension's single id space.
///   * Leaf ids and aggregate ids share one u32 space (`next_id`), so a cube
///     cell coordinate is unambiguous regardless of which hierarchy the
///     aggregate came from. Leaf ids are shared across hierarchies; aggregate
///     ids are not. (This is what makes the query cache sound: a resolved id
///     determines exactly one subtree.)
///   * Every dimension is born with a DEFAULT hierarchy named after the
///     dimension itself, so single-hierarchy usage is unchanged and existing
///     scripts/tests keep working.
///
/// WHERE DATA LIVES: measures are stored ONLY at leaf coordinates (see
/// `Cube::write`). Because a leaf belongs to EVERY hierarchy, a single stored
/// value aggregates differently under each hierarchy purely via that
/// hierarchy's tree - no per-hierarchy duplication. Writes therefore resolve a
/// member in the DEFAULT hierarchy; only reads address a specific hierarchy
/// (via a `Hierarchy:Member` qualifier).
#[derive(Clone, Serialize, Deserialize)]
pub struct Dimension {
    pub name: String,

    // --- SHARED LEAF BANK ---
    // Base members shared by every hierarchy.
    leaf_name_to_id: HashMap<String, u32>, // lowercased name -> leaf id
    leaf_id_to_name: HashMap<u32, String>, // leaf id -> original casing

    // --- HIERARCHIES ---
    // Keyed by lowercased hierarchy name.
    hierarchies: HashMap<String, Hierarchy>,
    // The default hierarchy's name (original casing). Present for every
    // dimension; refers to an entry in `hierarchies`.
    default_hierarchy: String,

    // Types spanning both namespaces (leaves are Leaf; aggregates Consolidated).
    member_types: HashMap<u32, MemberType>,

    next_id: u32,

    // Tracks the explicit default leaf, or the first element added.
    pub default_member_id: Option<u32>,
}

impl Dimension {

    pub fn new(name: &str) -> Self {
        let mut dim = Dimension {
            name: name.to_string(),
            leaf_name_to_id: HashMap::new(),
            leaf_id_to_name: HashMap::new(),
            hierarchies: HashMap::new(),
            default_hierarchy: name.to_string(),
            member_types: HashMap::new(),
            next_id: 1,
            default_member_id: None,
        };
        // Every dimension is born with a default hierarchy named after itself.
        dim.hierarchies.insert(name.to_lowercase(), Hierarchy::new(name));
        dim
    }

    // --- HIERARCHY PLUMBING ---

    /// The name of this dimension's default hierarchy.
    pub fn default_hierarchy_name(&self) -> &str {
        &self.default_hierarchy
    }

    /// Looks up a hierarchy by name (case-insensitive).
    pub fn hierarchy(&self, name: &str) -> Option<&Hierarchy> {
        self.hierarchies.get(&name.to_lowercase())
    }

    pub fn hierarchy_mut(&mut self, name: &str) -> Option<&mut Hierarchy> {
        self.hierarchies.get_mut(&name.to_lowercase())
    }

    /// Resolves a hierarchy name to a real hierarchy, defaulting to the
    /// dimension's default hierarchy when `name` is None.
    pub fn resolve_hierarchy(&self, name: Option<&str>) -> Option<&Hierarchy> {
        match name {
            None => self.hierarchy(&self.default_hierarchy),
            Some(n) => self.hierarchy(n),
        }
    }

    pub fn resolve_hierarchy_mut(&mut self, name: Option<&str>) -> Option<&mut Hierarchy> {
        match name {
            None => {
                let d = self.default_hierarchy.clone();
                self.hierarchy_mut(&d)
            }
            Some(n) => self.hierarchy_mut(n),
        }
    }

    /// Names of this dimension's hierarchies, in a deterministic (sorted) order.
    pub fn hierarchy_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.hierarchies.values().map(|h| h.name.clone()).collect();
        v.sort();
        v
    }

    /// True when this dimension has the named hierarchy (case-insensitive).
    pub fn has_hierarchy(&self, name: &str) -> bool {
        self.hierarchies.contains_key(&name.to_lowercase())
    }

    /// Creates a hierarchy in this dimension, returning an error if one already
    /// exists with that name (case-insensitive).
    pub fn create_hierarchy(&mut self, name: &str) -> Result<(), String> {
        let key = name.to_lowercase();
        if self.hierarchies.contains_key(&key) {
            return Err(format!(
                "Hierarchy '{}' already exists in dimension '{}'.",
                name, self.name
            ));
        }
        let mut h = Hierarchy::new(name);
        // Pre-register existing leaves as aliases so lookups by leaf name work
        // immediately (leaves belong to every hierarchy).
        let aliases: Vec<(String, u32)> = self.leaf_id_to_name.iter()
            .map(|(&id, n)| (n.clone(), id))
            .collect();
        for (leaf_name, id) in aliases {
            h.register_leaf_alias(&leaf_name, id);
        }
        h.refresh_display_order();
        self.hierarchies.insert(key, h);
        Ok(())
    }

    // --- MEMBER LOOKUP (dimension-wide) ---

    /// Converts an id back to a String for tabular results. Aggregate names are
    /// resolved through whichever hierarchy registered them.
    pub fn get_name(&self, id: u32) -> String {
        if let Some(n) = self.leaf_id_to_name.get(&id) {
            return n.clone();
        }
        for h in self.hierarchies.values() {
            if let Some(n) = h.member_name(id) {
                return n.to_string();
            }
        }
        "Unknown".to_string()
    }

    /// Dimension-wide id lookup: understands shared leaves and any aggregate
    /// name across all hierarchies. Used where context (a hierarchy) is not
    /// available. Returns the first match in a deterministic order.
    pub fn get_id(&self, member: &str) -> Option<u32> {
        if let Some(&id) = self.leaf_name_to_id.get(&member.to_lowercase()) {
            return Some(id);
        }
        // Deterministic scan: prefer the default hierarchy, then others sorted.
        if let Some(h) = self.hierarchy(&self.default_hierarchy)
            && let Some(id) = h.get_id(member)
        {
            return Some(id);
        }
        let mut names: Vec<String> = self.hierarchies.keys().cloned().collect();
        names.sort();
        for key in names {
            if let Some(id) = self.hierarchies[&key].get_id(member) {
                return Some(id);
            }
        }
        None
    }

    /// Id lookup scoped to a hierarchy (defaulting when `hier` is None).
    pub fn get_id_in(&self, hier: Option<&str>, member: &str) -> Option<u32> {
        self.resolve_hierarchy(hier).and_then(|h| h.get_id(member))
    }

    // --- LEAF / AGGREGATE CONSTRUCTION ---

    /// Creates or returns a shared Leaf member (belongs to every hierarchy).
    pub fn add_leaf(&mut self, name: &str) -> u32 {
        let key = name.to_lowercase();
        if let Some(&id) = self.leaf_name_to_id.get(&key) {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.leaf_name_to_id.insert(key, id);
        self.leaf_id_to_name.insert(id, name.to_string());
        self.member_types.insert(id, MemberType::Leaf);

        // Leaves are members of every hierarchy; register the alias in each.
        for h in self.hierarchies.values_mut() {
            h.register_leaf_alias(name, id);
        }

        if self.default_member_id.is_none() {
            self.default_member_id = Some(id);
        }
        id
    }

    /// Creates or returns a Consolidated member *within* a hierarchy
    /// (defaulting to the default hierarchy).
    ///
    /// PROMOTION: if the name already exists as a shared leaf, it is *promoted* -
    /// the leaf is removed from the shared bank and re-registered as an
    /// aggregate of this hierarchy, KEEPING ITS ID so any data already stored
    /// under that coordinate remains reachable. This mirrors the historic import
    /// flow (a name seen first as a child, then as a parent, becomes a parent).
    /// A name therefore maps to exactly one id, and is never simultaneously a
    /// leaf and an aggregate.
    pub fn add_consolidated_in(&mut self, hier: Option<&str>, name: &str) -> Result<u32, String> {
        let hier_key = match hier {
            None => self.default_hierarchy.clone(),
            Some(h) => h.to_string(),
        };
        let hier_lc = hier_key.to_lowercase();
        if !self.hierarchies.contains_key(&hier_lc) {
            return Err(format!(
                "Hierarchy '{}' does not exist in dimension '{}'.",
                hier_key, self.name
            ));
        }

        // Already an aggregate here?
        if let Some(id) = self.hierarchies[&hier_lc].get_id(name)
            && self.member_types.get(&id) == Some(&MemberType::Consolidated)
        {
            return Ok(id);
        }

        // Promote an existing shared leaf, reusing its id.
        if let Some(&leaf_id) = self.leaf_name_to_id.get(&name.to_lowercase()) {
            let original = self.leaf_id_to_name.remove(&leaf_id).unwrap_or_else(|| name.to_string());
            self.leaf_name_to_id.remove(&name.to_lowercase());
            // Drop the leaf alias from EVERY hierarchy; it is now an aggregate of
            // `hier_key` only. Other hierarchies simply stop seeing it.
            for h in self.hierarchies.values_mut() {
                h.forget_alias(leaf_id);
            }
            self.member_types.insert(leaf_id, MemberType::Consolidated);
            self.hierarchies.get_mut(&hier_lc).unwrap()
                .register_aggregate(&original, leaf_id);
            return Ok(leaf_id);
        }

        let id = self.next_id;
        self.next_id += 1;
        self.member_types.insert(id, MemberType::Consolidated);

        let h = self.hierarchies.get_mut(&hier_lc).unwrap();
        h.register_aggregate(name, id);
        Ok(id)
    }

    /// Determines whether the named member is currently a consolidation in the
    /// given hierarchy (defaulting when `hier` is None).
    pub fn is_consolidated_in(&self, hier: Option<&str>, member: &str) -> bool {
        match self.resolve_hierarchy(hier).and_then(|h| h.get_id(member)) {
            Some(id) => self.member_types.get(&id) == Some(&MemberType::Consolidated),
            None => false,
        }
    }

    /// Backwards-compatible shim: is `member` consolidated in the DEFAULT
    /// hierarchy?
    pub fn is_consolidated(&self, member: &str) -> bool {
        self.is_consolidated_in(None, member)
    }

    /// Connects a child to a parent *within* a hierarchy (defaulting when
    /// `hier` is None). Both endpoints are created/registered as needed:
    /// the parent as an aggregate of this hierarchy, the child as a shared
    /// leaf (or an existing aggregate of this hierarchy).
    ///
    /// Returns the name of the old parent if a re-parent happened.
    pub fn add_component_in(
        &mut self,
        hier: Option<&str>,
        parent: &str,
        child: &str,
        weight: f64,
    ) -> Result<Option<String>, String> {
        let hier_key = match hier {
            None => self.default_hierarchy.clone(),
            Some(h) => h.to_string(),
        };
        if !self.hierarchies.contains_key(&hier_key.to_lowercase()) {
            return Err(format!(
                "Hierarchy '{}' does not exist in dimension '{}'.",
                hier_key, self.name
            ));
        }

        // Parent is always an aggregate of this hierarchy.
        let parent_id = self.add_consolidated_in(Some(&hier_key), parent)?;

        // Child: an aggregate already registered in this hierarchy, otherwise a
        // shared leaf.
        let child_id = if let Some(id) = self.hierarchies
            .get(&hier_key.to_lowercase())
            .and_then(|h| h.get_id(child))
            .filter(|&id| self.member_types.get(&id) == Some(&MemberType::Consolidated))
        {
            id
        } else if self.leaf_name_to_id.contains_key(&child.to_lowercase()) {
            self.leaf_name_to_id[&child.to_lowercase()]
        } else {
            // Unknown name: treat it as a shared leaf (mirrors legacy behavior).
            self.add_leaf(child)
        };

        let old_parent_id = {
            let h = self.hierarchies.get_mut(&hier_key.to_lowercase()).unwrap();
            h.link(parent_id, child_id, weight)
        };

        // Re-parenting may have emptied the old parent; if it is now childless
        // and no longer referenced, downgrade its type so it reads as a leaf.
        let moved_from = old_parent_id.map(|old_id| {
            let still_parent = self.hierarchies[&hier_key.to_lowercase()].has_children(old_id);
            if !still_parent {
                self.member_types.insert(old_id, MemberType::Leaf);
            }
            self.get_name(old_id)
        });

        Ok(moved_from)
    }

    /// Backwards-compatible shim onto the DEFAULT hierarchy.
    pub fn add_component(&mut self, parent: &str, child: &str, weight: f64) -> Option<String> {
        self.add_component_in(None, parent, child, weight)
            .ok()
            .flatten()
    }

    /// Backwards-compatible shim onto the DEFAULT hierarchy.
    pub fn add_consolidated(&mut self, name: &str) -> u32 {
        self.add_consolidated_in(None, name).unwrap_or(0)
    }

    /// Detaches `child` from `parent` within a hierarchy (defaulting when
    /// `hier` is None). Inverse of `add_component_in`.
    pub fn remove_component_in(
        &mut self,
        hier: Option<&str>,
        parent: &str,
        child: &str,
    ) -> Result<(), String> {
        let hier_key = match hier {
            None => self.default_hierarchy.clone(),
            Some(h) => h.to_string(),
        };
        if !self.hierarchies.contains_key(&hier_key.to_lowercase()) {
            return Err(format!("Hierarchy '{}' does not exist in dimension '{}'.", hier_key, self.name));
        }
        let parent_id = self.hierarchies[&hier_key.to_lowercase()].get_id(parent)
            .ok_or_else(|| format!("Member '{}' does not exist in dimension '{}'.", parent, self.name))?;
        let child_id = self.hierarchies[&hier_key.to_lowercase()].get_id(child)
            .ok_or_else(|| format!("Member '{}' does not exist in dimension '{}'.", child, self.name))?;

        if self.member_types.get(&parent_id) != Some(&MemberType::Consolidated) {
            return Err(format!("Member '{}' is not a parent in dimension '{}'.", parent, self.name));
        }

        let removed = {
            let h = self.hierarchies.get_mut(&hier_key.to_lowercase()).unwrap();
            let children = h.consolidations_mut()
                .get_mut(&parent_id)
                .ok_or_else(|| format!("Member '{}' has no children.", parent))?;
            let before = children.len();
            children.retain(|(id, _)| *id != child_id);
            let removed = children.len() != before;
            if removed && children.is_empty() {
                h.consolidations_mut().remove(&parent_id);
            }
            removed
        };
        if !removed {
            return Err(format!("'{}' is not a child of '{}'.", child, parent));
        }
        if !self.hierarchies[&hier_key.to_lowercase()].has_children(parent_id) {
            self.member_types.insert(parent_id, MemberType::Leaf);
        }
        let h = self.hierarchies.get_mut(&hier_key.to_lowercase()).unwrap();
        h.detach_child(child_id);
        Ok(())
    }

    /// Backwards-compatible shim onto the DEFAULT hierarchy.
    pub fn remove_component(&mut self, parent: &str, child: &str) -> Result<(), String> {
        self.remove_component_in(None, parent, child)
    }

    /// Deletes a member entirely from this dimension (from the shared leaf bank
    /// if it is a leaf, and from every hierarchy's aggregate set if it is an
    /// aggregate). Returns the removed id so the caller can purge data cells.
    pub fn delete_member(&mut self, name: &str) -> Result<u32, String> {
        let key = name.to_lowercase();

        // Shared leaf?
        if let Some(&id) = self.leaf_name_to_id.get(&key) {
            self.leaf_name_to_id.remove(&key);
            self.leaf_id_to_name.remove(&id);
            self.member_types.remove(&id);
            for h in self.hierarchies.values_mut() {
                h.forget(id);
            }
            self.fix_default(id);
            return Ok(id);
        }

        // Aggregate? Find the first hierarchy that registers it.
        let found = self.hierarchies.iter()
            .find_map(|(k, h)| h.get_id(name).map(|id| (k.clone(), id)));
        let (hier_key, id) = found.ok_or_else(|| {
            format!("Member '{}' does not exist in dimension '{}'.", name, self.name)
        })?;
        self.member_types.remove(&id);
        let h = self.hierarchies.get_mut(&hier_key).unwrap();
        h.forget(id);
        // Children of the deleted aggregate became orphaned roots; their type is
        // recomputed so any that lost all children read as leaves.
        self.recompute_types_in(&hier_key);
        self.fix_default(id);
        Ok(id)
    }

    fn fix_default(&mut self, deleted_id: u32) {
        if self.default_member_id == Some(deleted_id) {
            self.default_member_id = self.leaf_name_to_id.values().copied().min();
        }
    }

    /// Recomputes Leaf/Consolidated types for all members of a hierarchy.
    fn recompute_types_in(&mut self, hier_key: &str) {
        if let Some(h) = self.hierarchies.get(hier_key) {
            let ids: Vec<u32> = h.registered_ids();
            for id in ids {
                let t = if h.has_children(id) {
                    MemberType::Consolidated
                } else {
                    MemberType::Leaf
                };
                self.member_types.insert(id, t);
            }
        }
    }

    // --- ORDERING (hierarchy-scoped) ---

    pub fn move_member_before_in(
        &mut self,
        hier: Option<&str>,
        name: &str,
        reference: &str,
    ) -> Result<(), String> {
        let dim_name = self.name.clone();
        let h = self.resolve_hierarchy_mut(hier)
            .ok_or_else(|| format!("Hierarchy not found in dimension '{}'.", dim_name))?;
        let id = h.get_id(name)
            .ok_or_else(|| format!("Member '{}' does not exist in dimension '{}'.", name, dim_name))?;
        let ref_id = h.get_id(reference)
            .ok_or_else(|| format!("Member '{}' does not exist in dimension '{}'.", reference, dim_name))?;
        if h.parent_of(id) != h.parent_of(ref_id) {
            return Err(format!(
                "'{}' and '{}' are not siblings in hierarchy '{}', so they cannot be reordered relative to each other. (Re-parenting is a separate operation.)",
                name, reference, h.name
            ));
        }
        h.move_member_before(id, ref_id);
        Ok(())
    }

    pub fn move_member_to_front_in(
        &mut self,
        hier: Option<&str>,
        name: &str,
    ) -> Result<(), String> {
        let dim_name = self.name.clone();
        let h = self.resolve_hierarchy_mut(hier)
            .ok_or_else(|| format!("Hierarchy not found in dimension '{}'.", dim_name))?;
        let id = h.get_id(name)
            .ok_or_else(|| format!("Member '{}' does not exist in dimension '{}'.", name, dim_name))?;
        h.move_member_to_front(id);
        Ok(())
    }

    /// Backwards-compatible shims (default hierarchy).
    pub fn move_member_before(&mut self, name: &str, reference: &str) -> Result<(), String> {
        self.move_member_before_in(None, name, reference)
    }
    pub fn move_member_to_front(&mut self, name: &str) -> Result<(), String> {
        self.move_member_to_front_in(None, name)
    }

    // --- DEFAULT MEMBER ---

    pub fn set_default_member(&mut self, name: &str) -> Result<(), String> {
        if let Some(id) = self.get_id(name) {
            self.default_member_id = Some(id);
            Ok(())
        } else {
            Err(format!("Member '{}' does not exist in dimension '{}'", name, self.name))
        }
    }

    pub fn get_default_member_name(&self) -> Option<String> {
        self.default_member_id.map(|id| self.get_name(id))
    }

    // --- QUERIES OVER THE LEAF BANK / HIERARCHIES ---

    /// Total number of distinct members across the leaf bank and all
    /// hierarchies' aggregate sets.
    pub fn len(&self) -> usize {
        self.leaf_name_to_id.len()
            + self.hierarchies.values()
                .flat_map(|h| h.registered_ids())
                .filter(|id| !self.leaf_id_to_name.contains_key(id))
                .count()
    }

    pub fn is_empty(&self) -> bool {
        self.leaf_name_to_id.is_empty() && self.hierarchies.values().all(|h| h.is_empty())
    }

    /// Number of shared leaves.
    pub fn leaf_count(&self) -> usize {
        self.leaf_name_to_id.len()
    }

    /// Resolves a member (defaulting to the default hierarchy) into its leaf
    /// descendants with aggregated weights.
    pub fn get_leaf_descendants_in(&self, hier: Option<&str>, member: &str) -> HashMap<u32, f64> {
        let mut leaves = HashMap::new();
        let Some(h) = self.resolve_hierarchy(hier) else {
            return leaves;
        };
        let Some(start_id) = h.get_id(member) else {
            return leaves;
        };
        let is_leaf = |id: u32| self.leaf_id_to_name.contains_key(&id);
        h.resolve_leaf_descendants(start_id, is_leaf, &mut leaves);
        leaves
    }

    /// Backwards-compatible shim: resolve within the DEFAULT hierarchy.
    pub fn get_leaf_descendants(&self, member: &str) -> HashMap<u32, f64> {
        self.get_leaf_descendants_in(None, member)
    }

    /// Recursively appends an ASCII tree, starting at `member` in `hier`.
    pub fn print_tree(&self, member: &str, depth: usize, current_weight: f64, out: &mut String) {
        // Legacy signature: print from the default hierarchy.
        self.print_tree_in(None, member, depth, current_weight, out);
    }

    pub fn print_tree_in(
        &self,
        hier: Option<&str>,
        member: &str,
        _depth: usize,
        current_weight: f64,
        out: &mut String,
    ) {
        let Some(h) = self.resolve_hierarchy(hier) else {
            let _ = writeln!(out, "└─ {} (Hierarchy Not Found)", member);
            return;
        };
        let Some(id) = h.get_id(member) else {
            let _ = writeln!(out, "└─ {} (Not Found)", member);
            return;
        };
        let display = |id: u32| self.get_name(id);
        h.print_tree(id, &display, 0, current_weight, out);
    }

    /// The current depth-first display order of a hierarchy as member names.
    pub fn member_order_names_in(&mut self, hier: Option<&str>) -> Vec<String> {
        let Some(h) = self.resolve_hierarchy_mut(hier) else {
            return Vec::new();
        };
        h.ensure_display_order();
        let ids: Vec<u32> = h.display_order().to_vec();
        ids.into_iter().map(|id| self.get_name(id)).collect()
    }

    /// Backwards-compatible shim (default hierarchy).
    pub fn member_order_names(&mut self) -> Vec<String> {
        self.member_order_names_in(None)
    }

    pub fn display_order_rank_in(&self, hier: Option<&str>, id: u32) -> usize {
        self.resolve_hierarchy(hier)
            .map(|h| h.display_order_rank(id))
            .unwrap_or(usize::MAX)
    }

    /// Presentation rank within the DEFAULT hierarchy (used when a cube axis
    /// does not name a hierarchy explicitly).
    pub fn display_order_rank(&self, id: u32) -> usize {
        self.display_order_rank_in(None, id)
    }

    // --- DISPLAY ORDER MAINTENANCE ---

    /// Rebuilds every hierarchy's display order lazily if stale.
    pub fn ensure_display_order(&mut self) {
        for h in self.hierarchies.values_mut() {
            h.ensure_display_order();
        }
    }

    /// Unconditionally rebuilds every hierarchy's display order (after load).
    pub fn refresh_display_order(&mut self) {
        for h in self.hierarchies.values_mut() {
            h.refresh_display_order();
        }
    }

    /// Returns the member type (Leaf / Consolidated) for a raw member id.
    pub fn member_type(&self, id: u32) -> Option<MemberType> {
        self.member_types.get(&id).cloned()
    }
}
