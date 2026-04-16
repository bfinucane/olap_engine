use std::collections::HashMap;

pub struct Dictionary {

    map: HashMap<String, u32>,
    next_id: u32,

}

impl Dictionary {

    pub fn new() -> Self {
        Dictionary {
            map: HashMap::new(),
            next_id: 1,
        }
    }

    pub fn get_or_create(&mut self, name: &str) -> u32 {

        if let Some(&id) = self.map.get(name) {
            return id;
        }

        let id = self.next_id;
        self.map.insert(name.to_string(), id);
        self.next_id += 1;

        id
    }
}