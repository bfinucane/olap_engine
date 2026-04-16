use serde::{Serialize, Deserialize};
use std::collections::HashMap;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Node {
    // Rust requires explicit types. 
    // We use a HashMap for sparse children storage.
    pub children: HashMap<u32, Node>, 
    pub value: Option<f64>,
}

impl Node {
    pub fn new() -> Self {
        Self::default()
    }
}