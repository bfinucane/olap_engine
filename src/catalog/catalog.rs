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

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
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

    /// Looks up a dimension by name, case-insensitively.
    pub fn get_dimension(&self, name: &str) -> Option<Arc<RwLock<Dimension>>> {
        self.dimensions.get(name).cloned().or_else(|| {
            self.dimensions.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| Arc::clone(v))
        })
    }

    /// Creates a hierarchy inside a dimension (creating the dimension if
    /// needed). Hierarchy names are unique per DATABASE, so this also refuses a
    /// name that already exists as a hierarchy of ANY dimension.
    ///
    /// Returns the name of the owning dimension on success.
    pub fn create_hierarchy(&mut self, dimension: &str, hierarchy: &str) -> Result<String, String> {
        let dim_arc = self.get_or_create_dimension(dimension);
        let dim_name = dim_arc.read().unwrap().name.clone();

                // Global uniqueness: no other dimension may already own this hierarchy.
        if let Some(owner) = self.find_hierarchy_owner(hierarchy)
            && !owner.eq_ignore_ascii_case(&dim_name)
        {
            return Err(format!(
                "Hierarchy '{}' already exists in dimension '{}'; hierarchy names must be \
                 unique across the database.",
                hierarchy, owner
            ));
        }

        dim_arc.write().unwrap().create_hierarchy(hierarchy)?;
        // A new hierarchy changes nothing about stored data (leaves are shared),
        // so no cache purge is strictly required; we clear anyway for safety.
        self.clear_all_caches();
        Ok(dim_name)
    }

    /// Finds the dimension that owns a hierarchy with this name (case-
    /// insensitive). Used to enforce database-wide hierarchy-name uniqueness
    /// and to resolve a bare hierarchy name to its dimension.
    pub fn find_hierarchy_owner(&self, hierarchy: &str) -> Option<String> {
        for dim_arc in self.dimensions.values() {
            let dim = dim_arc.read().unwrap();
            if dim.has_hierarchy(hierarchy) {
                return Some(dim.name.clone());
            }
        }
        None
    }

        /// Resolves a hierarchy name to (dimension_arc, hierarchy_display_name).
    pub fn resolve_hierarchy(
        &self,
        hierarchy: &str,
    ) -> Option<(Arc<RwLock<Dimension>>, String)> {
        for dim_arc in self.dimensions.values() {
            let dim = dim_arc.read().unwrap();
            if let Some(h) = dim.hierarchy(hierarchy) {
                return Some((Arc::clone(dim_arc), h.name.clone()));
            }
        }
        None
    }

    /// Resolves a user-supplied reference (from `.rollup`, `.tree`, `.order`,
    /// imports, etc.) to a concrete hierarchy.
    ///
    /// OPTION B RESOLUTION RULE - dimension names are dynamic ALIASES:
    ///   1. If the reference names a HIERARCHY anywhere in the database, that
    ///      hierarchy wins (hierarchy names are globally unique).
    ///   2. Otherwise, if it names a DIMENSION, it resolves to that dimension's
    ///      DEFAULT hierarchy (the one named after the dimension).
    ///   3. If it names a dimension that does not yet exist, the dimension is
    ///      created (with its implicit default hierarchy) and returned - this
    ///      preserves the historic "first mention creates the dimension" flow.
    ///
    /// Returns `(dimension, hierarchy_name, dimension_was_created)`.
    pub fn resolve_reference(
        &mut self,
        reference: &str,
    ) -> (Arc<RwLock<Dimension>>, String, bool) {
        // 1. An existing hierarchy anywhere?
        if let Some((arc, name)) = self.resolve_hierarchy(reference) {
            return (arc, name, false);
        }
        // 2/3. A dimension -> its default hierarchy (creating if needed).
        let existed = self.get_dimension(reference).is_some();
        let arc = self.get_or_create_dimension(reference);
        let name = arc.read().unwrap().default_hierarchy_name().to_string();
        (arc, name, !existed)
    }

    /// Resolves a user reference to the DIMENSION it belongs to, plus the
    /// specific hierarchy it names (if any).
    ///
    ///   * If `reference` is a hierarchy name, returns its owning dimension and
    ///     `Some(hierarchy)` - so a query axis/filter can be qualified as
    ///     `Hierarchy:Member`.
    ///   * If `reference` is a dimension name only, returns `(dimension, None)`
    ///     meaning "the dimension's default hierarchy".
    ///   * Returns `None` if the name is neither a known hierarchy nor a known
    ///     dimension.
        pub fn dimension_of_reference(&self, reference: &str) -> Option<(String, Option<String>)> {
                // A DIMENSION name is the common case and always means "this dimension,
        // default hierarchy" - even though the default hierarchy shares its name.
        if let Some(dim_arc) = self.get_dimension(reference) {
            return Some((dim_arc.read().unwrap().name.clone(), None));
        }
        // Otherwise, an ADDITIONAL hierarchy name: return its owning dimension
        // and the hierarchy name so callers can qualify members as
        // `Hierarchy:Member`.
        for dim_arc in self.dimensions.values() {
            let dim = dim_arc.read().unwrap();
            if let Some(h) = dim.hierarchy(reference) {
                return Some((dim.name.clone(), Some(h.name.clone())));
            }
        }
        None
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

    /// Deletes a member from a dimension AND purges every data cell that
    /// referenced it in ANY cube sharing that dimension. This is the inverse
    /// of adding a member and is intentionally a "big operation":
    ///   1. the member is removed from the dimension (children pop to top
    ///      level; see `Dimension::delete_member`),
    ///   2. every cube that uses this dimension has all cells with that member
    ///      as a coordinate deleted from its trie,
    ///   3. all caches are cleared.
    ///
    /// Returns a short human-readable summary.
    pub fn delete_member(&mut self, dimension: &str, member: &str) -> Result<String, String> {
        // Resolve the dimension (case-insensitive) and remove the member.
        let dim_arc = self.dimensions.get(dimension)
            .or_else(|| self.dimensions.iter().find(|(k, _)| k.eq_ignore_ascii_case(dimension)).map(|(_, v)| v))
            .cloned()
            .ok_or_else(|| format!("Dimension '{}' not found.", dimension))?;

        let dim_name = dim_arc.read().unwrap().name.clone();
        let member_id = dim_arc.write().unwrap().delete_member(member)?;

        // Sweep every cube that references this dimension. `dim_index` is the
        // position of the dimension within that cube; the member id can only
        // appear at that depth in the trie.
        let mut cubes_touched = 0;
        for cube in self.cubes.values_mut() {
            let dim_index = cube.dimension_names.iter()
                .position(|d| d.eq_ignore_ascii_case(&dim_name));
            if let Some(idx) = dim_index {
                cube.store.remove_cells_with_id_at(idx, member_id);
                cube.clear_cache();
                cubes_touched += 1;
            }
        }

        Ok(format!(
            "Deleted member '{}' from dimension '{}' (data purged from {} cube(s)).",
            member, dim_name, cubes_touched
        ))
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

        // 3. Rebuild each dimension's derived display order. The dirty flag is
        //    not serialized, so force a rebuild to be safe against a dimension
        //    that was saved before its order was regenerated.
        for dim_arc in catalog.dimensions.values() {
            dim_arc.write().unwrap().refresh_display_order();
        }

        catalog
    }
}