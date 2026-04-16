use crate::cube::cube::Cube;
use crate::cube::node::Node;

impl Cube {

    pub fn query(&self, coords: &[Option<u32>]) -> f64 {
        Self::query_node(&self.root, coords, 0)
    }

    fn query_node(node: &Node, coords: &[Option<u32>], depth: usize) -> f64 {

        if depth == coords.len() {
            return node.value.unwrap_or(0.0);
        }

        match coords[depth] {

            Some(id) => {
                match node.children.get(&id) {
                    Some(child) => Self::query_node(child, coords, depth + 1),
                    None => 0.0,
                }
            }

            None => {
                let mut sum = 0.0;

                for child in node.children.values() {
                    sum += Self::query_node(child, coords, depth + 1);
                }

                sum
            }

        }
    }
}