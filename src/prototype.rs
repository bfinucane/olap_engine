use std::collections::HashMap;

type ElementId = u32;
type Value = f64;

#[derive(Debug)]
struct Node {
    children: HashMap<ElementId, Node>,
    value: Option<Value>,
}

impl Node {
    fn new() -> Self {
        Node {
            children: HashMap::new(),
            value: None,
        }
    }
}

struct Cube {
    root: Node,
    dimensions: usize,
}

impl Cube {
    fn new(dimensions: usize) -> Self {
        Cube {
            root: Node::new(),
            dimensions,
        }
    }
}

impl Cube {

    fn write(&mut self, coords: &[ElementId], value: Value) {

        // Check to see if the right number of dimensions are being specified
		assert_eq!(coords.len(), self.dimensions);

        let mut node = &mut self.root;

        for &c in coords {
			// if child exists → return it
			// else → create new node
            node = node.children.entry(c).or_insert_with(Node::new);
        }

        node.value = Some(value);
    }

}

impl Cube {

    fn read(&self, coords: &[ElementId]) -> Option<Value> {

        let mut node = &self.root;

        for &c in coords {

            match node.children.get(&c) {
                Some(n) => node = n,
                None => return None,
            }

        }

        node.value
    }

}

fn main() {

    let mut cube = Cube::new(4);

    cube.write(&[1,2,3,4], 100.0);
    cube.write(&[1,2,3,5], 200.0);

    println!("{:?}", cube.read(&[1,2,3,4]));
    println!("{:?}", cube.read(&[1,2,3,5]));
    println!("{:?}", cube.read(&[1,2,3,6]));

}