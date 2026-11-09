use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub enum CellValue {
    Numeric(f64),
    String(String),
}

/// Index into the fact-set cell heap. Two index tries over one fact set store the SAME id.
pub type CellId = u64;
// Implement Display so it prints nicely in our ASCII tables
impl fmt::Display for CellValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CellValue::Numeric(n) => write!(f, "{}", n),
            CellValue::String(s) => write!(f, "{}", s),
        }
    }
}

/// One coordinate step in the Patricia trie.
///
/// Children are `Arc<Node>` rather than owned `Node`: the trie is a PERSISTENT data
/// structure, so a write copies only the nodes on the path to the coordinate and
/// shares every untouched subtree with the version it grew from. That is what makes
/// a snapshot O(1) instead of O(cells) — see `PHASE-1B-DESIGN.md` §2.
///
/// `Arc` rather than a manual version table also makes reclamation automatic: a
/// retired version's nodes are freed when the last reader drops its snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    // A HashMap keeps sparse children cheap; the values are shared, not copied.
    pub children: HashMap<u32, Arc<Node>>, 
    /// Heap slot for this coordinate; the value lives once in the store heap, so several
    /// index tries over one fact set can reference the SAME cell.
    pub value: Option<CellId>,
}

impl Default for Node {
    fn default() -> Self {
        Self::new()
    }
}

impl Node {
    pub fn new() -> Self {
        Node {
            children: HashMap::new(),
            value: None,
        }
    }
}