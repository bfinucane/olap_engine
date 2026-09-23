//! Write-cost baseline for the UNVERSIONED database, through the REAL
//! `SparseStore` (not a mirror).
//!
//! WHY THIS EXISTS
//!   The engine's headline advantage over columnar stores is fast writes. Before
//!   designing versioning / copy-on-write on top, we need a trustworthy baseline
//!   of what a write costs TODAY, in the code that actually ships. This binary
//!   measures that baseline. It deliberately does NOT model versions, pages, or
//!   CoW - those are unknown and would be fabrication. When the page/version
//!   model is settled, re-run this harness to see the delta.
//!
//! TWO WRITE CASES (the distinction that matters for versioning)
//!   * INSERT    - write to a NEW cell: the trie grows a new branch. This is the
//!                 only case that touches trie structure.
//!   * OVERWRITE - write a new value to an EXISTING cell: no structural change,
//!                 only the cell value is replaced.
//!
//! METRICS PER WRITE
//!   * allocations and bytes (counting #[global_allocator])
//!   * wall-clock ns/write, with a warmup pass and a median over N passes so the
//!     numbers are stable enough to publish.
//!
//! Run with:
//!   cargo run --release --bin bench_write
//!
//! Always --release.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use olap_engine::cube::node::CellValue;
use olap_engine::storage::sparse_store::SparseStore;

// ---------------------------------------------------------------------------
// Counting allocator
// ---------------------------------------------------------------------------

struct CountingAllocator;

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

struct Snap {
    count: u64,
    bytes: u64,
}

fn snapshot() -> Snap {
    Snap {
        count: ALLOC_COUNT.load(Ordering::Relaxed),
        bytes: ALLOC_BYTES.load(Ordering::Relaxed),
    }
}

fn measure_allocs<F: FnOnce()>(f: F) -> (u64, u64) {
    let before = snapshot();
    f();
    let after = snapshot();
    (after.count - before.count, after.bytes - before.bytes)
}

// ---------------------------------------------------------------------------
// Shapes and coordinates
// ---------------------------------------------------------------------------

struct Shape {
    name: &'static str,
    fan_out: Vec<u32>,
}

impl Shape {
    fn dims(&self) -> usize {
        self.fan_out.len()
    }
    fn cells(&self) -> usize {
        self.fan_out.iter().map(|&f| f as usize).product()
    }
}

/// All leaf coordinates (full cartesian product), last dim fastest.
fn all_coords(fan_out: &[u32]) -> Vec<Vec<u32>> {
    let total: usize = fan_out.iter().map(|&f| f as usize).product();
    let mut out = Vec::with_capacity(total);
    let mut cur = vec![0u32; fan_out.len()];
    loop {
        out.push(cur.clone());
        let mut i = fan_out.len();
        loop {
            if i == 0 {
                return out;
            }
            i -= 1;
            cur[i] += 1;
            if cur[i] < fan_out[i] {
                break;
            }
            cur[i] = 0;
        }
    }
}

/// Deterministic shuffle (xorshift64*), so a random-order run is reproducible.
fn shuffle_deterministic<T>(items: &mut [T], seed: u64) {
    let mut state = seed | 1;
    for i in (1..items.len()).rev() {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let r = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        let j = (r as usize) % (i + 1);
        items.swap(i, j);
    }
}

fn build_store(dims: usize, coords: &[Vec<u32>], value: f64) -> SparseStore {
    let mut store = SparseStore::new(dims);
    for c in coords {
        store.write(c, CellValue::Numeric(value));
    }
    store
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

/// INSERT: build the store over `setup`, then measure writing `fresh` (all new
/// cells). Returns (allocs, bytes) for the measured writes.
fn alloc_insert(dims: usize, setup: &[Vec<u32>], fresh: &[Vec<u32>]) -> (u64, u64) {
    let mut store = build_store(dims, setup, 1.0);
    measure_allocs(|| {
        for c in black_box(fresh) {
            store.write(c, CellValue::Numeric(1.0));
        }
        black_box(store.query_exact(&fresh[fresh.len() - 1]));
    })
}

/// OVERWRITE: build over `setup` (== the cells we then re-write), then measure
/// writing new values to those SAME coordinates.
fn alloc_overwrite(dims: usize, setup: &[Vec<u32>]) -> (u64, u64) {
    let mut store = build_store(dims, setup, 1.0);
    measure_allocs(|| {
        for c in black_box(setup) {
            store.write(c, CellValue::Numeric(2.0));
        }
        black_box(store.query_exact(&setup[setup.len() - 1]));
    })
}

/// Median of `passes` samples. `prepare` runs before each timer (untimed) so
/// store construction never pollutes the sample; `run` is what we time and must
/// consume the prepared state.
fn median_nanos<P, R, S, T>(mut prepare: P, mut run: S, passes: usize) -> u128
where
    P: FnMut() -> T,
    S: FnMut(T) -> R,
{
    // Warmup pass (not recorded).
    let warm = prepare();
    black_box(run(warm));

    let mut samples: Vec<u128> = Vec::with_capacity(passes);
    for _ in 0..passes {
        let state = prepare();
        let start = Instant::now();
        let out = run(state);
        samples.push(start.elapsed().as_nanos());
        black_box(out);
    }
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// Timed insert: prepare = rebuild the base store (untimed), run = write the
/// fresh coordinates + read one back.
fn time_insert(dims: usize, setup: &[Vec<u32>], fresh: &[Vec<u32>], passes: usize) -> u128 {
    median_nanos(
        || build_store(dims, setup, 1.0),
        |mut store: SparseStore| {
            for c in fresh {
                store.write(c, CellValue::Numeric(1.0));
            }
            black_box(store.query_exact(&fresh[fresh.len() - 1]))
        },
        passes,
    )
}

/// Timed overwrite: prepare = rebuild the base store (untimed), run = re-write
/// the same coordinates with new values.
fn time_overwrite(dims: usize, setup: &[Vec<u32>], passes: usize) -> u128 {
    median_nanos(
        || build_store(dims, setup, 1.0),
        |mut store: SparseStore| {
            for c in setup {
                store.write(c, CellValue::Numeric(2.0));
            }
            black_box(store.query_exact(&setup[setup.len() - 1]))
        },
        passes,
    )
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn report_case(label: &str, n: usize, allocs: u64, bytes: u64, nanos: u128) {
    let d = n.max(1) as f64;
    println!(
        "  {:<26} {:>10} {:>14} {:>12} {:>13} {:>12}",
        label,
        n,
        allocs,
        bytes,
        format!("{:.3}", allocs as f64 / d),
        format!("{:.1}", nanos as f64 / d),
    );
}

fn run_shape(shape: &Shape, passes: usize) {
    // Workload size: ~10% of the cube, floored/capped. The floor must never
    // exceed the cube size (tiny cubes), hence the .min(total).
    let total = shape.cells();
    let want = (total / 10).clamp(1000, 200_000);
    let n_insert = want.min(total / 2).max(1);
    let n_overwrite = want.min(total / 2).max(1);

    // Shuffle so setup/inserts are spread over the shape (not a dense prefix).
    let mut coords = all_coords(&shape.fan_out);
    shuffle_deterministic(&mut coords, 0x9E37_79B9_7F4A_7C15);

    // First part builds structure; last part = fresh inserts into it.
    let split = coords.len().saturating_sub(n_insert);
    let (setup, fresh) = coords.split_at(split);
    let setup: Vec<Vec<u32>> = setup.to_vec();
    let fresh: Vec<Vec<u32>> = fresh.to_vec();

    // Overwrite set: a slice of already-built cells.
    let ow: Vec<Vec<u32>> = setup[..n_overwrite.min(setup.len())].to_vec();

    println!("--------------------------------------------------------");
    println!(
        "Shape: {}  ({} cells, {} dims)",
        shape.name,
        shape.cells(),
        shape.dims()
    );
    println!(
        "  {:<26} {:>10} {:>14} {:>12} {:>13} {:>12}",
        "case", "writes", "allocs", "bytes", "alloc/write", "ns/write"
    );

    // --- INSERT ---
    let (ia, ib) = alloc_insert(shape.dims(), &setup, &fresh);
    let it = time_insert(shape.dims(), &setup, &fresh, passes);
    report_case("INSERT (new cell)", fresh.len(), ia, ib, it);

    // --- OVERWRITE ---
    let (oa, ob) = alloc_overwrite(shape.dims(), &ow);
    let ot = time_overwrite(shape.dims(), &ow, passes);
    report_case("OVERWRITE (existing cell)", ow.len(), oa, ob, ot);

    println!(
        "  (insert: {} setup cells | overwrite: {} existing cells | {} timed passes)",
        setup.len(),
        ow.len(),
        passes
    );
    println!();
}

fn main() {
    // Realistic (3-5 dims) and deep (8-9 dims) shapes.
    let shapes = vec![
        Shape { name: "small 10x20x12", fan_out: vec![10, 20, 12] },
        Shape { name: "medium 50x200x60", fan_out: vec![50, 200, 60] },
        Shape { name: "typical 20x40x30x10", fan_out: vec![20, 40, 30, 10] },
        Shape { name: "deep 8x9x7x6x5x4x3", fan_out: vec![8, 9, 7, 6, 5, 4, 3] },
        Shape { name: "deep 3x4x5x6x7x8x9x10", fan_out: vec![3, 4, 5, 6, 7, 8, 9, 10] },
        Shape { name: "deep 2x2x2x2x2x2x2x2x2", fan_out: vec![2, 2, 2, 2, 2, 2, 2, 2, 2] },
    ];

    let passes = 5;

    println!("Unversioned write-cost baseline (real SparseStore)");
    println!("Counting #[global_allocator]; timing = median of {passes} passes after warmup.");
    println!("INSERT = write to a new cell (grows the trie).");
    println!("OVERWRITE = new value for an existing cell (trie unchanged).\n");

    for s in &shapes {
        run_shape(s, passes);
    }

    println!("Reading: INSERT allocates new nodes + map storage along the new branch;");
    println!("OVERWRITE should allocate ~nothing (value-only change) - the property that");
    println!("makes per-cell updates cheap today and the baseline versioning must preserve.");
}
