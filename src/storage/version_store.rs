//! `VersionStore` — the store plus its version number and its version-tagged cache.
//!
//! WHY THIS EXISTS (Phase 1b, M2)
//!   `Cube` used to own a bare `SparseStore` and a `HashMap` cache that was cleared on
//!   EVERY write (`self.query_cache.clear()`), which is correct only under a
//!   single-writer assumption. This wrapper gives the store a version number that
//!   advances on write, and tags the cache with the version it was computed at:
//!
//!   - a read never needs the write lock: it takes a `SparseStore` snapshot, which is
//!     an `Arc` bump (O(1)) now that the trie is persistent;
//!   - a write publishes a new version and does NOT clear the cache;
//!   - the first read after a write notices the version changed and discards the
//!     entries lazily, so invalidation costs nothing when nobody reads.
//!
//! The on-disk shape is unchanged: this type serialises as the `SparseStore` it wraps,
//! so `FORMAT_VERSION` stays 1 and Phase 1a files keep loading.
//!
//! Why interior mutability at all: `Cube` is owned by value in `Catalog`, and the
//! server serialises everything through one `Mutex<Catalog>`. Making the store
//! `RwLock`ed is the first half of removing that coarse lock (M3); this milestone
//! only has to prove that versioning and lazy invalidation are correct.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{RwLock, RwLockReadGuard};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::cube::node::CellValue;
use crate::storage::sparse_store::SparseStore;

pub struct VersionStore {
    inner: RwLock<SparseStore>,
    /// Advances on every write. Runtime state only; never persisted.
    version: AtomicU64,
    /// Aggregate cache, valid only for `cache_version`.
    cache: RwLock<HashMap<Vec<u32>, f64>>,
    cache_version: AtomicU64,
}

impl VersionStore {
    pub fn new(store: SparseStore) -> Self {
        VersionStore {
            inner: RwLock::new(store),
            version: AtomicU64::new(0),
            cache: RwLock::new(HashMap::new()),
            cache_version: AtomicU64::new(u64::MAX), // forces a clear on first use
        }
    }

    /// The current version. Rises by one per write; never reused.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Publishes a new version WITHOUT writing a cell.
    ///
    /// For STRUCTURAL changes: a hierarchy edit alters what an aggregate MEANS while
    /// the stored values stay put, so nothing would otherwise invalidate a cached
    /// total. Bumping the version discards those entries lazily, exactly as a write
    /// does — without this, a cached aggregate survived a `.rollup`.
    pub fn bump_version(&self) {
        self.version.fetch_add(1, Ordering::AcqRel);
    }

    /// A point-in-time snapshot, O(1): one `Arc` bump inside the store.
    pub fn snapshot(&self) -> SparseStore {
        self.read().snapshot()
    }

    fn read(&self) -> RwLockReadGuard<'_, SparseStore> {
        // A poisoned lock still holds valid data; refusing to serve would be worse
        // than continuing with the data that is there.
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Applies a write and publishes a new version. The cache is NOT cleared.
    pub fn write(&self, coords: &[u32], value: CellValue) {
        {
            let mut store = self.inner.write().unwrap_or_else(|e| e.into_inner());
            store.write(coords, value);
        }
        self.version.fetch_add(1, Ordering::AcqRel);
    }

    /// Removes a member's cells and publishes a new version.
    pub fn remove_cells_with_id_at(&self, dim_index: usize, target_id: u32) {
        {
            let mut store = self.inner.write().unwrap_or_else(|e| e.into_inner());
            store.remove_cells_with_id_at(dim_index, target_id);
        }
        self.version.fetch_add(1, Ordering::AcqRel);
    }

    pub fn query_exact(&self, coords: &[u32]) -> Option<CellValue> {
        self.read().query_exact(coords)
    }

    /// Scans a snapshot rather than the live store, so a long scan cannot hold the
    /// read lock (and, once writes are concurrent in M3, cannot see a half-published
    /// version).
    pub fn scan_subcube(
        &self,
        allowed_leaves: &[Option<HashSet<u32>>],
    ) -> Vec<(Vec<u32>, CellValue)> {
        self.snapshot().scan_subcube(allowed_leaves)
    }

    pub fn print_tree(&self, out: &mut String) {
        self.read().print_tree(out);
    }

    /// A cached aggregate for the CURRENT version, or `None`.
    ///
    /// Lazy invalidation: the first lookup after a write notices the version moved and
    /// discards the entries. A cache hit still costs one lock acquisition, which is the
    /// price of not clearing on every write.
    pub fn cache_get(&self, key: &[u32]) -> Option<f64> {
        let version = self.version();
        let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
        if self.cache_version.load(Ordering::Relaxed) != version {
            cache.clear();
            self.cache_version.store(version, Ordering::Relaxed);
        }
        cache.get(key).copied()
    }

    /// Stores an aggregate, but only if the version it was computed at is still
    /// current. Without the `computed_at` check a value computed from an older
    /// snapshot could be filed under the newer version — the classic stale-cache bug
    /// that appears the moment writers run concurrently (M3).
    pub fn cache_put(&self, key: Vec<u32>, value: f64, computed_at: u64) {
        if self.version() != computed_at {
            return;
        }
        let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
        if self.cache_version.load(Ordering::Relaxed) != computed_at {
            cache.clear();
            self.cache_version.store(computed_at, Ordering::Relaxed);
        }
        cache.insert(key, value);
    }

    /// Number of cached aggregates for the current version. Test/diagnostic only.
    pub fn cached_len(&self) -> usize {
        self.cache.read().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl Clone for VersionStore {
    /// Cheap: the store clone is an `Arc` bump, and the cache starts empty (it is
    /// derived state, so a clone must not carry another version's answers).
    fn clone(&self) -> Self {
        VersionStore {
            inner: RwLock::new(self.snapshot()),
            version: AtomicU64::new(self.version()),
            cache: RwLock::new(HashMap::new()),
            cache_version: AtomicU64::new(u64::MAX),
        }
    }
}

// The on-disk shape must stay exactly `SparseStore`, so the format version does not
// change and existing databases keep loading. Version and cache are runtime state.
impl Serialize for VersionStore {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.read().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for VersionStore {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let store = SparseStore::deserialize(deserializer)?;
        Ok(VersionStore::new(store))
    }
}
