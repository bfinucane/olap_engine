use serde::{Serialize, Deserialize};
use std::collections::HashMap;

/// One named hierarchy *within* a dimension.
///
/// A dimension owns a single, SHARED bank of Leaf members (see `Dimension`);
/// base elements therefore belong to every hierarchy automatically. What lives
/// here is only the *consolidation* structure: which aggregated members exist,
/// how they parent their children (which may be shared leaves OR other
/// aggregates of THIS hierarchy), and the display order derived from that tree.
///
/// Because aggregates are local to a hierarchy, two hierarchies may both define
/// a member named `All` without collision - they are distinct ids in the
/// dimension's single id space. Leaf ids are shared, so a fact written to a
/// leaf is visible through every hierarchy that eventually reaches it.
///
/// `member_to_id` intentionally also indexes the SHARED leaves (as aliases) so
/// that name lookups inside a hierarchy ("is 'France' a child of 'Europe'?")
/// resolve without consulting the dimension. Leaf aliases always map to the
/// dimension-wide leaf id; aggregate entries map to this hierarchy's own ids.
#[derive(Clone, Serialize, Deserialize)]
pub struct Hierarchy {
    pub name: String,

    /// Name (lowercased) -> node id. Holds this hierarchy's aggregate nodes,
    /// plus leaf aliases that point back to the shared leaf ids.
    member_to_id: HashMap<String, u32>,

    // Original-cased display name for each AGGREGATE node id of this hierarchy.
    // (Leaf aliases keep their casing in the dimension's leaf bank instead.)
    #[serde(default)]
    display_names: HashMap<u32, String>,

    // The tree: Parent ID -> Vec<(Child ID, Weight)>, sibling order preserved.
    consolidations: HashMap<u32, Vec<(u32, f64)>>,

    // Reverse index: Child ID -> its single Parent ID (one parent per hierarchy).
    #[serde(default)]
    child_to_parent: HashMap<u32, u32>,

    // Order of top-level (root) members; new members append (creation order).
    #[serde(default)]
    root_order: Vec<u32>,

    // Derived, flat depth-first flattening of the tree (see Dimension docs).
    #[serde(default)]
    display_order: Vec<u32>,

    // When true, `display_order` is stale and must be rebuilt before it is read.
    #[serde(skip)]
    display_order_dirty: bool,
}

impl Hierarchy {
    pub fn new(name: &str) -> Self {
        Hierarchy {
            name: name.to_string(),
            member_to_id: HashMap::new(),
            display_names: HashMap::new(),
            consolidations: HashMap::new(),
            child_to_parent: HashMap::new(),
            root_order: Vec::new(),
            display_order: Vec::new(),
            display_order_dirty: false,
        }
    }

    /// Looks up a name local to this hierarchy (aggregate or leaf alias).
    pub fn get_id(&self, member: &str) -> Option<u32> {
        self.member_to_id.get(&member.to_lowercase()).copied()
    }

    pub fn contains(&self, member: &str) -> bool {
        self.member_to_id.contains_key(&member.to_lowercase())
    }

    /// The name under which `id` was registered *in this hierarchy*. For an
    /// aggregate that is the aggregate's own original-cased name; for a shared
    /// leaf it is the leaf alias name. `None` if the id is not present here.
    pub fn member_name(&self, id: u32) -> Option<&str> {
        if let Some(n) = self.display_names.get(&id) {
            return Some(n.as_str());
        }
        // Fall back to a leaf alias (lowercased key) if this is a shared leaf.
        self.member_to_id.iter()
            .find(|(_, v)| **v == id)
            .map(|(k, _)| k.as_str())
    }

    /// The display name registered in this hierarchy for an id, if any.
    pub fn display_name_of(&self, id: u32) -> Option<&str> {
        self.display_names.get(&id).map(|s| s.as_str())
    }

    /// Number of distinct names registered in this hierarchy.
    pub fn len(&self) -> usize {
        self.member_to_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.member_to_id.is_empty()
    }

    /// Registers an aggregate node id under `name` in this hierarchy.
    /// Does not touch the tree; `add_component` builds the tree.
    pub(crate) fn register_aggregate(&mut self, name: &str, id: u32) {
        self.member_to_id.insert(name.to_lowercase(), id);
        self.display_names.insert(id, name.to_string());
        self.push_root_if_new(id);
    }

    /// Registers a leaf alias so hierarchy-local lookups by leaf name succeed.
    /// The leaf also enters the root ordering so it appears in the display order
    /// even when it has no aggregate parent.
    pub(crate) fn register_leaf_alias(&mut self, name: &str, leaf_id: u32) {
        let was_new = !self.member_to_id.contains_key(&name.to_lowercase());
        self.member_to_id.entry(name.to_lowercase()).or_insert(leaf_id);
        if was_new {
            self.push_root_if_new(leaf_id);
            self.mark_display_order_dirty();
        }
    }

    /// Removes a leaf alias (used when a shared leaf is promoted to an aggregate
    /// of some other hierarchy). Only the alias is dropped; tree structure and
    /// aggregate display names are untouched.
    pub(crate) fn forget_alias(&mut self, leaf_id: u32) {
        // No display name for a plain leaf, so a display-names entry means this
        // id is an aggregate here and must be left alone.
        if self.display_names.contains_key(&leaf_id) {
            return;
        }
        self.member_to_id.retain(|_, &mut v| v != leaf_id);
        self.root_order.retain(|&x| x != leaf_id);
        self.mark_display_order_dirty();
    }

    /// Removes a name from this hierarchy's dictionary and detaches it from any
    /// parent. Children of the removed aggregate are orphaned to the top level.
    pub(crate) fn forget(&mut self, id: u32) {
        self.member_to_id.retain(|_, &mut v| v != id);
        self.display_names.remove(&id);

        if let Some(&parent_id) = self.child_to_parent.get(&id) {
            let emptied = if let Some(children) = self.consolidations.get_mut(&parent_id) {
                children.retain(|(cid, _)| *cid != id);
                children.is_empty()
            } else {
                false
            };
            if emptied {
                self.consolidations.remove(&parent_id);
            }
            self.child_to_parent.remove(&id);
        }

        let orphaned: Vec<u32> = self.consolidations
            .get(&id)
            .map(|cs| cs.iter().map(|(c, _)| *c).collect())
            .unwrap_or_default();
        self.consolidations.remove(&id);
        for child_id in &orphaned {
            self.child_to_parent.remove(child_id);
        }

        self.root_order.retain(|&x| x != id);
        for child_id in orphaned {
            self.push_root_if_new(child_id);
        }

        self.mark_display_order_dirty();
    }

    /// Appends `id` to `root_order` if not already present.
    fn push_root_if_new(&mut self, id: u32) {
        if !self.root_order.contains(&id) {
            self.root_order.push(id);
        }
    }

    /// Connects a child to a parent within this hierarchy (one-parent rule).
    /// Returns the name's previous parent id if the child was re-parented.
    pub(crate) fn link(&mut self, parent_id: u32, child_id: u32, weight: f64) -> Option<u32> {
        let mut moved_from: Option<u32> = None;
        if let Some(&old_parent) = self.child_to_parent.get(&child_id)
            && old_parent != parent_id
        {
            moved_from = Some(old_parent);
            if let Some(kids) = self.consolidations.get_mut(&old_parent) {
                kids.retain(|(cid, _)| *cid != child_id);
                if kids.is_empty() {
                    self.consolidations.remove(&old_parent);
                }
            }
        }

        let children = self.consolidations.entry(parent_id).or_default();
        if self.child_to_parent.get(&child_id) == Some(&parent_id) {
            if let Some(existing) = children.iter_mut().find(|(id, _)| *id == child_id) {
                existing.1 = weight;
            }
        } else {
            children.push((child_id, weight));
            self.child_to_parent.insert(child_id, parent_id);
        }

        self.root_order.retain(|&x| x != child_id);
        self.mark_display_order_dirty();
        moved_from
    }

    pub fn parent_of(&self, id: u32) -> Option<u32> {
        self.child_to_parent.get(&id).copied()
    }

    pub fn children_of(&self, id: u32) -> &[(u32, f64)] {
        self.consolidations.get(&id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn has_children(&self, id: u32) -> bool {
        self.consolidations.get(&id).is_some_and(|c| !c.is_empty())
    }

    // --- ORDERING ---

    pub fn move_member_before(&mut self, id: u32, ref_id: u32) {
        if id == ref_id {
            return;
        }
        let id_parent = self.parent_of(id);
        match id_parent {
            Some(pid) => {
                let Some(children) = self.consolidations.get_mut(&pid) else {
                    return;
                };
                let Some(moved) = children.iter().position(|(c, _)| *c == id)
                    .map(|i| children.remove(i)) else {
                    return;
                };
                // If the reference sibling is gone, re-append to avoid panicking.
                let pos = children.iter().position(|(c, _)| *c == ref_id)
                    .unwrap_or(children.len());
                children.insert(pos, moved);
            }
            None => {
                let Some(moved) = self.root_order.iter().position(|&x| x == id)
                    .map(|i| self.root_order.remove(i)) else {
                    return;
                };
                let pos = self.root_order.iter().position(|&x| x == ref_id)
                    .unwrap_or(self.root_order.len());
                self.root_order.insert(pos, moved);
            }
        }
        self.mark_display_order_dirty();
    }

    pub fn move_member_to_front(&mut self, id: u32) {
        match self.parent_of(id) {
            Some(pid) => {
                let Some(children) = self.consolidations.get_mut(&pid) else {
                    return;
                };
                let Some(moved) = children.iter().position(|(c, _)| *c == id)
                    .map(|i| children.remove(i)) else {
                    return;
                };
                children.insert(0, moved);
            }
            None => {
                self.root_order.retain(|&x| x != id);
                self.root_order.insert(0, id);
            }
        }
        self.mark_display_order_dirty();
    }

    // --- DISPLAY ORDER ---

    fn rebuild_display_order(&mut self) {
        let mut flat = Vec::with_capacity(self.member_to_id.len());
        let roots = self.root_order.clone();
        for rid in roots {
            self.flatten_dfs(rid, &mut flat, 0);
        }
        self.display_order = flat;
        self.display_order_dirty = false;
    }

    fn flatten_dfs(&self, id: u32, out: &mut Vec<u32>, depth: usize) {
        // Guard against pathological cycles (should not occur in a well-formed
        // tree, but a malformed import could create one).
        if depth > self.member_to_id.len() + 1 {
            return;
        }
        out.push(id);
        if let Some(children) = self.consolidations.get(&id) {
            for &(child_id, _) in children {
                self.flatten_dfs(child_id, out, depth + 1);
            }
        }
    }

    fn mark_display_order_dirty(&mut self) {
        self.display_order_dirty = true;
    }

    pub fn ensure_display_order(&mut self) {
        if self.display_order_dirty {
            self.rebuild_display_order();
        }
    }

    pub fn refresh_display_order(&mut self) {
        self.rebuild_display_order();
    }

    /// Presentation rank of a member id (its index in the depth-first order).
    pub fn display_order_rank(&self, id: u32) -> usize {
        // The display order only contains members reachable from the tree; a
        // leaf that has no aggregate parent is still a root and included.
        self.display_order.iter().position(|&x| x == id).unwrap_or(usize::MAX)
    }

    pub fn display_order(&self) -> &[u32] {
        &self.display_order
    }

    /// All ids registered in this hierarchy (aggregates + leaf aliases).
    pub fn registered_ids(&self) -> Vec<u32> {
        self.member_to_id.values().copied().collect()
    }

    /// Mutable access to the raw consolidation map, used by the owning
    /// dimension for in-place detach/remove edits.
    pub(crate) fn consolidations_mut(&mut self) -> &mut HashMap<u32, Vec<(u32, f64)>> {
        &mut self.consolidations
    }

    /// Detaches a child from its parent (drops the reverse-index entry and
    /// re-adds it as a root). Used after `remove_component`.
    pub(crate) fn detach_child(&mut self, child_id: u32) {
        self.child_to_parent.remove(&child_id);
        self.push_root_if_new(child_id);
        self.mark_display_order_dirty();
    }

    /// Resolves a member id into leaf descendants, given an external predicate
    /// that decides whether an id is a leaf of the owning dimension.
    pub fn resolve_leaf_descendants(
        &self,
        start_id: u32,
        is_leaf: impl Fn(u32) -> bool,
        leaves: &mut HashMap<u32, f64>,
    ) {
        self.resolve_recursive(start_id, 1.0, &is_leaf, leaves, 0);
    }

    /// Resolves a member id into its LEAF descendants and aggregated weights.
    /// Aggregate members recurse; a leaf resolves to itself (weight 1.0).
    pub fn get_leaf_descendants(
        &self,
        start_id: u32,
        is_leaf: &dyn Fn(u32) -> bool,
    ) -> HashMap<u32, f64> {
        let mut leaves = HashMap::new();
        self.resolve_recursive(start_id, 1.0, is_leaf, &mut leaves, 0);
        leaves
    }

    fn resolve_recursive(
        &self,
        current_id: u32,
        current_weight: f64,
        is_leaf: &dyn Fn(u32) -> bool,
        leaves: &mut HashMap<u32, f64>,
        depth: usize,
    ) {
        if depth > self.member_to_id.len() + 1 {
            return;
        }
        if is_leaf(current_id) {
            *leaves.entry(current_id).or_insert(0.0) += current_weight;
        } else if let Some(children) = self.consolidations.get(&current_id) {
            for &(child_id, child_weight) in children {
                self.resolve_recursive(
                    child_id,
                    current_weight * child_weight,
                    is_leaf,
                    leaves,
                    depth + 1,
                );
            }
        }
    }

    /// Like `get_leaf_descendants` but reports whether every visited id is a
    /// leaf, used to classify a member's type.
    pub fn is_leaf_id(&self, id: u32) -> bool {
        !self.has_children(id)
    }

    /// Recursively appends an ASCII tree to `out`, starting at `start_id`.
    pub fn print_tree(
        &self,
        start_id: u32,
        display_name: &dyn Fn(u32) -> String,
        depth: usize,
        current_weight: f64,
        out: &mut String,
    ) {
        use std::fmt::Write as _;
        let prefix = "  ".repeat(depth);
        let node_type = if self.has_children(start_id) {
            "(Parent)"
        } else {
            "(Leaf)"
        };
        let weight_str = if (current_weight - 1.0).abs() > 0.001 {
            format!(" [w: {}]", current_weight)
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "{}└─ {} {}{}",
            prefix,
            display_name(start_id),
            node_type,
            weight_str
        );
        if let Some(children) = self.consolidations.get(&start_id) {
            for &(child_id, child_weight) in children {
                self.print_tree(child_id, display_name, depth + 1, child_weight, out);
            }
        }
    }
}
