//! Measures the STORAGE baseline that Phase 1b's memory-churn gate is judged against.
//!
//! WHY THIS EXISTS
//!   `ROADMAP.md` Phase 1b gates the MVCC rewrite on measurement: "Copy-on-write has
//!   real overhead ... This must be measured, not assumed." This binary measures the
//!   CURRENT store so the versioned store can be compared against something concrete
//!   instead of an opinion.
//!
//! WHAT IT MEASURES (per write, and per snapshot)
//!   * bytes allocated        — the churn proxy Infor hit with page churn
//!   * allocator calls        — their real problem was allocator SPEED, not just bytes
//!   * wall time per write
//!   * scan throughput        — the number a pinned reader must not lose more than ~10%
//!   * clone cost             — what an Option-B snapshot (bulk clone) actually costs
//!
//! HOW
//!   A counting `GlobalAlloc` wraps the system allocator and tallies every allocation
//!   while a measured closure runs. No new dependency.
//!
//! RUN
//!   cargo run --release --bin bench_mvcc
//!   (Use --release: the debug allocator distorts both time and call counts.)

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use olap_engine::cube::node::CellValue;
use olap_engine::storage::sparse_store::SparseStore;

struct CountingAlloc;

static BYTES: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // A realloc moves data: count the new size and one call, which is the
        // conservative reading of the churn it causes.
        BYTES.fetch_add(new_size, Ordering::Relaxed);
        CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Deterministic pseudo-random coordinates — no `rand` dependency, and repeatable runs.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn coord(&mut self, dims: usize, cardinality: u64) -> Vec<u32> {
        (0..dims).map(|_| (self.next() % cardinality) as u32).collect()
    }
}

/// Runs `f`, returning (bytes allocated, allocations, elapsed nanoseconds).
fn measure<F: FnOnce()>(f: F) -> (usize, usize, u128) {
    BYTES.store(0, Ordering::Relaxed);
    CALLS.store(0, Ordering::Relaxed);
    let start = std::time::Instant::now();
    f();
    let elapsed = start.elapsed().as_nanos();
    (BYTES.load(Ordering::Relaxed), CALLS.load(Ordering::Relaxed), elapsed)
}

fn main() {
    println!();
    println!("  Phase 1b churn baseline — CURRENT store (mutable Patricia trie)");
    println!("  {}", "-".repeat(96));

    // Sparse but realistic: 3 and 4 dimensions over a cardinarity that leaves plenty
    // of holes, plus a deep-path case because ragged hierarchies are the worst case
    // for path copying later.
    for (label, dims, cardinality, warm) in [
        ("3 dims, 40^3 space", 3usize, 40u64, 8_000usize),
        ("4 dims, 25^4 space", 4usize, 25u64, 20_000usize),
        ("1 dim, 512-deep", 1usize, 0u64, 0usize), // handled separately below
    ] {
        let mut rng = Lcg(0x5EED_1234);

        if dims == 1 {
            // Deep path: one cell at depth 512, then re-write it to show path cost.
            let mut store = SparseStore::new(512);
            let deep: Vec<u32> = (0..512u32).collect();
            store.write(&deep, CellValue::Numeric(1.0));

            let mut probe = store.clone();
            let (bytes, calls, ns) = measure(|| {
                probe.write(&deep, CellValue::Numeric(2.0));
            });
            println!(
                "  {:<22} {:>9} B  {:>4} allocs  {:>8.1} µs   (re-write a 512-deep path)",
                label,
                bytes,
                calls,
                ns as f64 / 1000.0
            );
            continue;
        }

        // Warm the store so path creation does not dominate the measurement, keeping
        // the coordinates used so the overwrite case overwrites REAL cells (a random
        // draw would include never-written coordinates and silently measure path
        // creation instead).
        let mut store = SparseStore::new(dims);
        let mut warmed: Vec<Vec<u32>> = Vec::with_capacity(warm);
        for _ in 0..warm {
            let c = rng.coord(dims, cardinality);
            store.write(&c, CellValue::Numeric(1.0));
            warmed.push(c);
        }

        // 1) Steady-state writes to coordinates that already exist (overwrite path).
        let existing: Vec<Vec<u32>> = warmed.iter().take(2_000).cloned().collect();
        let mut probe = store.clone();
        let (bytes, calls, ns) = measure(|| {
            for c in &existing {
                probe.write(c, CellValue::Numeric(2.0));
            }
        });
        let n = existing.len();
        println!(
            "  {:<22} {:>9} B  {:>6.2} allocs  {:>8.2} µs   (per write, overwrite)",
            label,
            bytes / n,
            calls as f64 / n as f64,
            ns as f64 / 1000.0 / n as f64
        );

        // 2) Writes that introduce NEW leaves (path creation included).
        let fresh: Vec<Vec<u32>> = (0..500).map(|_| rng.coord(dims, cardinality * 2)).collect();
        let mut probe = store.clone();
        let (bytes, calls, ns) = measure(|| {
            for c in &fresh {
                probe.write(c, CellValue::Numeric(3.0));
            }
        });
        let n = fresh.len();
        println!(
            "  {:<22} {:>9} B  {:>6.2} allocs  {:>8.2} µs   (per write, new leaf)",
            label,
            bytes / n,
            calls as f64 / n as f64,
            ns as f64 / 1000.0 / n as f64
        );

        // 3) Scan throughput: the number a pinned reader must not lose to a writer.
        let cells = store.scan_subcube(&vec![None; dims]).len();
        let (_, _, ns) = measure(|| {
            for _ in 0..10 {
                let hit = store.scan_subcube(&vec![None; dims]).len();
                assert_eq!(hit, cells);
            }
        });
        println!(
            "  {:<22} {:>9}       {:>4}          {:>8.2} ns/cell  ({} cells, 10 scans)",
            label,
            "",
            "",
            ns as f64 / (cells as f64 * 10.0),
            cells
        );

        // 4) Option B's snapshot cost: a whole-store clone, per snapshot.
        let (bytes, calls, ns) = measure(|| {
            let _snapshot = store.clone();
        });
        println!(
            "  {:<22} {:>9} B  {:>4} allocs  {:>8.1} µs   (Option-B snapshot = clone)",
            label,
            bytes,
            calls,
            ns as f64 / 1000.0
        );
        println!();
    }

    println!("  {}", "-".repeat(96));
    println!("  Read it as: the versioned store must stay within a small multiple of the");
    println!("  'per write' bytes above, and must not regress 'ns/cell' by more than ~10%.");
    println!();
}
