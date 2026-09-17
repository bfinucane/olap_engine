use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]

pub enum CellValue {
    Numeric(f64),
    String(String),
}
// Implement Display so it prints nicely in our ASCII tables
impl fmt::Display for CellValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CellValue::Numeric(n) => write!(f, "{}", n),
            CellValue::String(s) => write!(f, "{}", s),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    // UZse a HashMap for sparse children storage.
    pub children: HashMap<u32, Node>, 
    pub value: Option<CellValue>,
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