use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use crate::dimension::dimension::Dimension;
use crate::cube::cube::Cube;

/// Magic bytes at the head of a saved catalog file. Lets us reject a file that
/// is not one of ours before attempting a (positional, non-self-describing)
/// bincode decode.
const CATALOG_MAGIC: [u8; 4] = *b"OLAP";

/// Current on-disk format version. Bump this whenever the serialized layout
/// changes (new fields, changed types) and add a migration branch in
/// `load_from_disk`.
const FORMAT_VERSION: u32 = 1;

/// Errors that can occur while persisting or loading a catalog.
#[derive(Debug)]
pub enum PersistError {
    /// An underlying I/O failure (create, write, fsync, rename, open, read).
    Io(std::io::Error),
    /// Serialization of the in-memory catalog failed.
    Encode(bincode::Error),
    /// Deserialization of the on-disk catalog failed.
    Decode(bincode::Error),
    /// The file is not a catalog file (bad magic bytes).
    NotACatalog,
    /// The file was written by a NEWER format than this binary understands.
    UnsupportedVersion { found: u32, supported: u32 },
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistError::Io(e) => write!(f, "I/O error: {}", e),
            PersistError::Encode(e) => write!(f, "Could not encode catalog: {}", e),
            PersistError::Decode(e) => write!(f, "Could not decode catalog: {}", e),
            PersistError::NotACatalog => write!(f, "File is not an OLAP catalog."),
            PersistError::UnsupportedVersion { found, supported } => write!(
                f,
                "Catalog format v{} is newer than this binary supports (v{}). Upgrade to open it.",
                found, supported
            ),
        }
    }
}

impl std::error::Error for PersistError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PersistError::Io(e) => Some(e),
            PersistError::Encode(e) | PersistError::Decode(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for PersistError {
    fn from(e: std::io::Error) -> Self {
        PersistError::Io(e)
    }
}

/// The self-identifying envelope written to disk: magic + version + payload.
/// The header lets us detect foreign or newer files BEFORE we attempt the
/// positional bincode decode, and gives future versions a place to hang
/// migrations.
#[derive(Serialize, Deserialize)]
struct PersistedCatalog {
    magic: [u8; 4],
    version: u32,
    payload: CatalogDiskFormat,
}

// The serialized shape of the catalog. This is the PAYLOAD of `PersistedCatalog`
// and should change only in lockstep with `FORMAT_VERSION`.
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

    /// Takes a consistent point-in-time snapshot of the whole catalog.
    ///
    /// This is the DURABILITY snapshot (Phase 1a): everything is cloned once so
    /// the on-disk file reflects a single coherent moment, never a half-applied
    /// write. (Phase 1b will replace the deep clone with a copy-on-write version
    /// store that does not freeze writers.)
    fn snapshot(&self) -> CatalogDiskFormat {
        let mut disk_data = CatalogDiskFormat {
            dimensions: HashMap::new(),
            cubes: self.cubes.clone(),
        };
        for (name, dim_arc) in &self.dimensions {
            disk_data.dimensions.insert(name.clone(), dim_arc.read().unwrap().clone());
        }
        disk_data
    }

    /// Durably saves the catalog to `filepath`.
    ///
    /// Crash-safety: we NEVER write into the live file. `File::create` would
    /// truncate it to zero bytes before a single byte is written, so a crash
    /// mid-save would destroy the database. Instead we:
    ///   1. serialize to a uniquely-named SIBLING temp file (same filesystem, so
    ///      the later rename is atomic),
    ///   2. flush + `sync_all` (fsync) it so the bytes are truly on disk,
        ///   3. atomically `rename` it over the target.
    ///
    /// A reader (or a crash) therefore sees either the complete OLD file or the
    /// complete NEW file - never a partial one.
    pub fn save_to_disk(&self, filepath: &str) -> Result<(), PersistError> {
        let snapshot = self.snapshot();
        let envelope = PersistedCatalog {
            magic: CATALOG_MAGIC,
            version: FORMAT_VERSION,
            payload: snapshot,
        };

        let target = Path::new(filepath);
        let tmp = temp_sibling_path(target);

        // Scope the writer so its fd is closed before the rename (required on
        // Windows, where you cannot rename over an open handle).
        {
            let file = File::create(&tmp)?;
            let mut writer = BufWriter::new(file);
            bincode::serialize_into(&mut writer, &envelope).map_err(PersistError::Encode)?;
            writer.flush()?;
            // fsync: without it the bytes may still be buffered in the OS when
            // the rename lands, so a power loss could leave a renamed-but-empty
            // file.
            writer.into_inner().map_err(|e| PersistError::Io(e.into_error()))?.sync_all()?;
        }

                // Atomic publish. On Windows `rename` fails if the destination exists,
                // so we remove it first; the window is tiny and, crucially, the temp
                // file is already fully durable, so a crash here still leaves a valid
                // (old) file rather than a truncated one.
                match std::fs::rename(&tmp, target) {
                    Ok(()) => {}
                    Err(_e) => {
                        #[cfg(windows)]
                        {
                            let _ = std::fs::remove_file(target);
                            std::fs::rename(&tmp, target)?;
                        }
                        #[cfg(not(windows))]
                        {
                            return Err(PersistError::Io(_e));
                        }
                    }
                }
                Ok(())
    }

    /// Loads a catalog from `filepath`, or returns an error.
    ///
    /// The file is validated (magic + version) BEFORE the positional bincode
    /// decode, so a foreign or newer file is reported cleanly instead of being
    /// silently mis-parsed.
    pub fn load_from_disk(filepath: &str) -> Result<Self, PersistError> {
        let mut file = File::open(filepath)?;

        // Peek the fixed-size header: 4 magic bytes + 4 version bytes.
        let mut header = [0u8; 8];
        if let Err(e) = file.read_exact(&mut header) {
            // A too-short file cannot be a valid catalog.
            return Err(if e.kind() == std::io::ErrorKind::UnexpectedEof {
                PersistError::NotACatalog
            } else {
                PersistError::Io(e)
            });
        }
        if header[0..4] != CATALOG_MAGIC {
            return Err(PersistError::NotACatalog);
        }
        let version = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if version > FORMAT_VERSION {
            return Err(PersistError::UnsupportedVersion {
                found: version,
                supported: FORMAT_VERSION,
            });
        }
        // (When FORMAT_VERSION grows, migration branches for older versions go
        //  here. For now there is only v1, so nothing to migrate.)

        // Decode the payload. We re-read the whole file from the start because
        // bincode consumes the header bytes again into the envelope struct.
        let mut reader = BufReader::new(File::open(filepath)?);
        let envelope: PersistedCatalog =
            bincode::deserialize_from(&mut reader).map_err(PersistError::Decode)?;

        let disk_data = envelope.payload;
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

        Ok(catalog)
    }
}

/// Builds a unique sibling temp path for `target` (same directory, so the
/// rename is atomic within one filesystem). Uniqueness uses the PID plus a
/// process-wide counter, so two concurrent saves never collide.
fn temp_sibling_path(target: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);

    let dir = target.parent().filter(|p| !p.as_os_str().is_empty());
    let file_name = target.file_name().map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "database.bin".to_string());
    let tmp_name = format!(".{}.{}.{}.tmp", file_name, std::process::id(), n);

    match dir {
        Some(d) => d.join(tmp_name),
        None => PathBuf::from(tmp_name),
    }
}