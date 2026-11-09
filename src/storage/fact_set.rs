//! Fact set: one shared fact heap, several index tries, and SEPARATE attribute cubes.
//!
//! Facts own keys + measures only. Attributes live in their own non-aggregating cubes (each with
//! its own trie) and are referenced by key; they are never copied into the fact heap. This is the
//! first slice of the 2B design: it proves the storage relationships, not yet the catalog/wire
//! integration.

use std::collections::HashMap;
use std::sync::Arc;

use crate::cube::node::{CellId, CellValue};
use crate::storage::sparse_store::{CellHeap, SparseStore};

/// A separate, non-aggregating attribute cube with its own trie over its key coordinates.
pub struct AttributeCube {
    pub name: String,
    pub keys: Vec<String>,
    pub store: SparseStore,
}

/// One index over the fact set: a dimension ORDER (canonical dimension indices in index order)
/// and its trie. Several indexes share the fact set's heap.
pub struct FactIndex {
    pub name: String,
    pub order: Vec<usize>,
    pub store: SparseStore,
}

pub struct FactSet {
    pub name: String,
    pub dimensions: Vec<String>,
    pub measures: Vec<String>,
    pub heap: Arc<CellHeap>,
    pub indexes: HashMap<String, FactIndex>,
    pub attributes: HashMap<String, AttributeCube>,
}

impl FactSet {
    pub fn new(name: &str, dimensions: Vec<String>, measures: Vec<String>) -> Self {
        FactSet {
            name: name.to_string(),
            dimensions,
            measures,
            heap: Arc::new(CellHeap::new()),
            indexes: HashMap::new(),
            attributes: HashMap::new(),
        }
    }

    /// Adds an index whose coordinate order is `order` (canonical dimension indices).
    pub fn add_index(&mut self, name: &str, order: Vec<usize>) {
        assert_eq!(order.len(), self.dimensions.len(), "index order must cover every dimension");
        let store = SparseStore::with_heap(self.dimensions.len(), self.heap.clone());
        self.indexes.insert(name.to_string(), FactIndex { name: name.to_string(), order, store });
    }

    /// Adds a separate non-aggregating attribute cube (own trie and own storage).
    pub fn add_attribute(&mut self, name: &str, keys: Vec<String>) {
        let store = SparseStore::new(keys.len());
        self.attributes.insert(name.to_string(), AttributeCube { name: name.to_string(), keys, store });
    }

    fn map(&self, index: &FactIndex, canonical: &[u32]) -> Vec<u32> {
        index.order.iter().map(|&i| canonical[i]).collect()
    }

    /// Writes one fact into the shared heap and every index. Returns the cell id.
    pub fn write_fact(&mut self, canonical: &[u32], value: CellValue) -> CellId {
        let id = self.heap.append(value);
        for index in self.indexes.values_mut() {
            let coords: Vec<u32> = index.order.iter().map(|&i| canonical[i]).collect();
            index.store.write_id(&coords, id);
        }
        id
    }

    pub fn query_exact(&self, index_name: &str, canonical: &[u32]) -> Option<CellValue> {
        let index = self.indexes.get(index_name)?;
        let coords = self.map(index, canonical);
        index.store.query_exact(&coords)
    }

    pub fn set_attribute(&mut self, cube: &str, keys: &[u32], value: CellValue) {
        if let Some(attr) = self.attributes.get_mut(cube) {
            attr.store.write(keys, value);
        }
    }

    pub fn get_attribute(&self, cube: &str, keys: &[u32]) -> Option<CellValue> {
        self.attributes.get(cube)?.store.query_exact(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_fact_set_two_index_orders_share_the_heap() {
        let mut fs = FactSet::new("Orders", vec!["Customer".into(), "City".into()], vec!["Sales".into()]);
        fs.add_index("by_customer", vec![0, 1]);
        fs.add_index("by_city", vec![1, 0]);
        fs.write_fact(&[7, 3], CellValue::Numeric(42.5));
        assert_eq!(fs.query_exact("by_customer", &[7, 3]), Some(CellValue::Numeric(42.5)));
        assert_eq!(fs.query_exact("by_city", &[7, 3]), Some(CellValue::Numeric(42.5)));
        assert_eq!(fs.heap.len(), 1, "one fact allocates one heap cell, shared by both indexes");
    }

    #[test]
    fn overwrite_is_seen_through_every_index() {
        let mut fs = FactSet::new("Orders", vec!["Customer".into(), "City".into()], vec!["Sales".into()]);
        fs.add_index("a", vec![0, 1]);
        fs.add_index("b", vec![1, 0]);
        fs.write_fact(&[1, 2], CellValue::Numeric(1.0));
        fs.write_fact(&[1, 2], CellValue::Numeric(9.0));
        assert_eq!(fs.query_exact("a", &[1, 2]), Some(CellValue::Numeric(9.0)));
        assert_eq!(fs.query_exact("b", &[1, 2]), Some(CellValue::Numeric(9.0)));
    }

    #[test]
    fn attributes_are_separate_from_facts() {
        let mut fs = FactSet::new("Orders", vec!["Customer".into(), "City".into()], vec!["Sales".into()]);
        fs.add_index("a", vec![0, 1]);
        fs.add_attribute("Customer", vec!["CustomerID".into()]);
        fs.write_fact(&[7, 3], CellValue::Numeric(42.5));
        let heap_before = fs.heap.len();
        fs.set_attribute("Customer", &[7], CellValue::String("Ava Thompson".into()));
        assert_eq!(fs.get_attribute("Customer", &[7]), Some(CellValue::String("Ava Thompson".into())));
        assert_eq!(fs.heap.len(), heap_before, "writing an attribute must not touch the fact heap");
        assert_eq!(fs.query_exact("a", &[7, 3]), Some(CellValue::Numeric(42.5)));
    }
}