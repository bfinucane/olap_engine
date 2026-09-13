use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::sync::{Arc, RwLock};
use crate::dimension::dimension::Dimension;
use crate::cube::cube::Cube;

// A temporary struct just for moving data to/from the hard drive
#[derive(Serialize, Deserialize)]
struct CatalogDiskFormat {
    dimensions: HashMap<String, Dimension>,
    cubes: HashMap<String, Cube>,
}

pub struct Catalog {
    pub dimensions: HashMap<String, Arc<RwLock<Dimension>>>,
    pub cubes: HashMap<String, Cube>, // The Catalog now owns the Cubes
}

impl Catalog {
    pub fn new() -> Self {
        Catalog {
            dimensions: HashMap::new(),
            cubes: HashMap::new(),
        }
    }
	pub fn clear_all_caches(&mut self) {
        for cube in self.cubes.values_mut() {
            cube.clear_cache();
        }
    }
    pub fn get_or_create_dimension(&mut self, name: &str) -> Arc<RwLock<Dimension>> {
        if let Some(dim) = self.dimensions.get(name) {
            return Arc::clone(dim);
        }
        let new_dim = Arc::new(RwLock::new(Dimension::new(name)));
        self.dimensions.insert(name.to_string(), Arc::clone(&new_dim));
        new_dim
    }

	pub fn add_cube(&mut self, name: &str, dim_names: &[&str], measure_dim: Option<&str>, is_aggregating : bool) {
        let mut dim_arcs = Vec::new();
        let mut dim_names_vec = Vec::new();

        for &d in dim_names {
            dim_arcs.push(self.get_or_create_dimension(d));
            dim_names_vec.push(d.to_string()); // Cube remembers original dimension casing
        }
        // Convert the Option<&str> to Option<String>
        let measure_string = measure_dim.map(|s| s.to_string());
		
		// Pass information to cube
        let cube = Cube::new(name, dim_names_vec, dim_arcs, measure_string, is_aggregating);
        // Save the cube under a lowercase key
        self.cubes.insert(name.to_lowercase(), cube);
    }

    pub fn get_cube_mut(&mut self, name: &str) -> Option<&mut Cube> {
        // Always look up using lowercase
        self.cubes.get_mut(&name.to_lowercase())
    }
	
	pub fn get_cube(&self, name: &str) -> Option<&Cube> {
			self.cubes.get(&name.to_lowercase())
		}
    // --- PERSISTENCE ---

	pub fn save_to_disk(&self, filepath: &str) {
        let mut disk_data = CatalogDiskFormat {
            dimensions: HashMap::new(),
            cubes: self.cubes.clone(),
        };
        for (name, dim_arc) in &self.dimensions {
            disk_data.dimensions.insert(name.clone(), dim_arc.read().unwrap().clone());
        }
        let file = File::create(filepath).expect("Failed to create database file");
        let writer = BufWriter::new(file);
        // Changed to bincode!
        bincode::serialize_into(writer, &disk_data).expect("Failed to serialize"); 
    }

	pub fn load_from_disk(filepath: &str) -> Self {
        let file = File::open(filepath).expect("Failed to open database file");
        let reader = BufReader::new(file);
        // Changed to bincode!
        let disk_data: CatalogDiskFormat = bincode::deserialize_from(reader).expect("Failed to parse");

        let mut catalog = Catalog::new();

        // 1. Restore dimensions and put them back into locks
        for (name, dim) in disk_data.dimensions {
            catalog.dimensions.insert(name, Arc::new(RwLock::new(dim)));
        }

        // 2. Restore cubes & reconnect shared dimensions
        for (name, mut cube) in disk_data.cubes {
            let mut dim_arcs = Vec::new();
            for dim_name in &cube.dimension_names {
                dim_arcs.push(catalog.get_or_create_dimension(dim_name));
            }
            cube.attach_dimensions(dim_arcs);
            catalog.cubes.insert(name, cube);
        }

        catalog
    }
}