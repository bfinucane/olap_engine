//! Trie write-cost benchmark for the Phase 1b decision ("measure before
//! committing").
//!
//! Phase 1b proposes making `SparseStore` a persistent (immutable) trie: a
//! write path-copies the nodes from the root down to the point where the new
//! coordinate diverges from existing shared structure, then allocates the
//! genuinely-new suffix. That copy-on-write (CoW) is what enables lock-free
//! reader isolation, but it has a cost that must be *measured* rather than
//! assumed.
//!
//! This binary answers two questions against the CURRENT mutable store:
//!
//!   1. What does the trie actually look like for a realistic cube, and how
//!      does its shape change as it fills? (nodes, branching factor, depth)
//!   2. If we replace the in-place write with an A1 CoW write, how many nodes
//!      are COPIED vs CREATED per write, and how does that amortize as the tree
//!      densifies? (This is the gate: if copied-nodes-per-write stays small and
//!      shrinks with density, CoW is cheap and we proceed.)
//!
//! The simulation replicates path-copying against the real tree's *shape*; it
//! does not modify the store. Run with:
//!
//!   cargo run --release --bin bench_trie
//!
//! (Use --release: the debug allocator is slow and would distort the picture.)

use std::collections::HashSet;
use std::time::Instant;

use olap_engine::cube::node::CellValue;
use olap_engine::storage::sparse_store::SparseStore;

/// A tiered, realistic cube shape: [top, mid, low]. Fan-out grows toward the
/// leaves, which is how real cubes look (few regions, more accounts, many
/// months/periods). Tweak to probe other shapes.
struct Shape {
    name: &'static str,
    /// Number of distinct members at each trie level (dim 0 = trie root level).
    fan_out: [u32; 3],
}

/// One measurement: how a fully-populated cube of this shape behaves.
struct Report {
    shape_name: &'static str,
    // Real (current, mutable) store facts.
    total_cells: usize,
    real_write_secs: f64,
    // Simulated A1 (immutable path-copy) facts.
    /// Nodes copied on the write path (parent chain that must be shallow-copied).
    copied_total: u64,
    /// Nodes newly allocated for the genuinely-new coordinate suffix.
    created_total: u64,
    /// Per-write series at increasing density, for the amortization curve.
    curve: Vec<CurvePoint>,
    /// A single warmed-cube overwrite of an existing cell (best case for CoW).
    warm_overwrite_copied: u64,
    warm_overwrite_created: u64,
    /// Whether cells were written in random order (vs. cartesian/dense order).
    randomized: bool,
}

struct CurvePoint {
    writes_done: u64,
    /// Trailing-window average nodes copied per write.
    copied_per_write: f64,
    /// Trailing-window average nodes created per write.
    created_per_write: f64,
}

/// A mirror of the node structure that the A1 simulation needs: children keyed
/// by coord, plus whether a value sits here. This is a *shadow* of the real
/// tree built by visiting it, used only to run the path-copy count.
#[derive(Default)]
struct ShadowNode {
    children: std::collections::HashMap<u32, ShadowNode>,
    has_value: bool,
}

impl ShadowNode {
    /// Inserts a coordinate path, returning (copied, created) exactly as an
    /// immutable A1 write would.
    ///
    /// Path-copy semantics: every node from the root down TO the point where
    /// this coordinate diverges is "copied" (its child map changes), and every
    /// node in the *new* suffix is "created". A write that re-uses an entire
    /// existing path (a warm overwrite) copies the whole path and creates none.
    fn cow_write(&mut self, coords: &[u32]) -> (u64, u64) {
        let mut copied = 0u64;
        let mut created = 0u64;
        let mut node = self;
        for (depth, &id) in coords.iter().enumerate() {
            if node.children.contains_key(&id) {
                // Shared child: in CoW this parent is copied, then we descend.
                copied += 1;
                node = node.children.get_mut(&id).unwrap();
            } else {
                // Divergence: from here down everything is newly created. We
                // must actually BUILD the new suffix in the shadow tree, so that
                // later writes sharing this prefix can count it as "copied".
                for &rest in &coords[depth..] {
                    created += 1;
                    node = node.children.entry(rest).or_default();
                }
                node.has_value = true;
                return (copied, created);
            }
        }
        // Whole path existed AND already held a value: a warm overwrite.
        if !node.has_value {
            created += 1;
        }
        node.has_value = true;
        (copied, created)
    }
}

/// Enumerate every leaf coordinate of the shape (the full cartesian product).
/// We generate writes over ALL of them so the tree ends fully dense, then look
/// at the amortization curve along the way.
fn all_coords(fan_out: &[u32; 3]) -> Vec<[u32; 3]> {
    let mut out = Vec::with_capacity(
        (fan_out[0] as usize) * (fan_out[1] as usize) * (fan_out[2] as usize),
    );
    for a in 0..fan_out[0] {
        for b in 0..fan_out[1] {
            for c in 0..fan_out[2] {
                out.push([a, b, c]);
            }
        }
    }
    out
}



/// Deterministic shuffle (xorshift64*) so a "random load order" run is
/// reproducible. Random order is closer to real ingestion than cartesian order,
/// which fills prefix groups densely from the start and therefore makes CoW
/// look better than it might be.
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

fn run_shape(shape: &Shape, randomized: bool) -> Report {
    let mut coords = all_coords(&shape.fan_out);
    if randomized {
        shuffle_deterministic(&mut coords, 0x9E37_79B9_7F4A_7C15);
    }

    // ---- 1. Real store: measure actual mutable-write time. ----
    let mut store = SparseStore::new(3);
    let real_start = Instant::now();
    for c in &coords {
        store.write(c, CellValue::Numeric(1.0));
    }
    let real_write_secs = real_start.elapsed().as_secs_f64();

    // Verify the store really holds every cell (so the shadow mirrors it).
    let flat: Vec<Option<HashSet<u32>>> = (0..3).map(|_| None).collect();
    let total_cells = store.scan_subcube(&flat).len();

    // ---- 2. Simulated A1 CoW: run path-copy over a shadow tree. ----
    let mut shadow = ShadowNode::default();
    let mut copied_total = 0u64;
    let mut created_total = 0u64;

    // Collect the amortization curve at logarithmically-spaced checkpoints.
    let mut curve = Vec::new();
    let mut window_copied = 0u64;
    let mut window_created = 0u64;
    let mut last_checkpoint = 0u64;
    let n = coords.len() as u64;
    // Checkpoints at ~1%, 2%, 5%, 10%, ... up to 100%.
    let mut checkpoints: Vec<u64> = vec![];
    let mut step = (n / 100).max(1);
    while step <= n {
        checkpoints.push(step);
        step *= 2;
    }
    checkpoints.push(n);

    for (i, c) in coords.iter().enumerate() {
        let (copied, created) = shadow.cow_write(c);
        copied_total += copied;
        created_total += created;
        window_copied += copied;
        window_created += created;

        let done = (i + 1) as u64;
        if checkpoints.contains(&done) {
            let span = (done - last_checkpoint).max(1) as f64;
            curve.push(CurvePoint {
                writes_done: done,
                copied_per_write: window_copied as f64 / span,
                created_per_write: window_created as f64 / span,
            });
            window_copied = 0;
            window_created = 0;
            last_checkpoint = done;
        }
    }

    // ---- 3. Warm overwrite: rewrite existing cells, count CoW cost. ----
    // A warm write re-touches cells that already exist (the analytical
    // re-load / re-calc case). This is the best case for CoW.
    let warm_sample = coords.iter().take(1000.max(coords.len() / 10));
    let mut warm_copied = 0u64;
    let mut warm_created = 0u64;
    for c in warm_sample {
        let (cp, cr) = shadow.cow_write(c);
        warm_copied += cp;
        warm_created += cr;
    }
    let warm_n = 1000.max(coords.len() / 10) as u64;
    let warm_overwrite_copied = warm_copied / warm_n.max(1);
    let warm_overwrite_created = warm_created / warm_n.max(1);

    Report {
        shape_name: shape.name,
        randomized,
        total_cells,
        real_write_secs,
        copied_total,
        created_total,
        curve,
        warm_overwrite_copied,
        warm_overwrite_created,
    }
}

fn print_report(r: &Report) {
    let n = r.total_cells.max(1) as u64;
    let order = if r.randomized { "random" } else { "cartesian" };
    println!("========================================================");
    println!("Shape: {}  [{} write order]", r.shape_name, order);
    println!("  Cells written:            {}", r.total_cells);
    println!(
        "  Real (mutable) write time: {:.3} ms total  ({:.3} us/cell)",
        r.real_write_secs * 1000.0,
        r.real_write_secs * 1_000_000.0 / n as f64
    );
    println!("  --- Simulated A1 (immutable path-copy) ---");
    println!("  Copied nodes (total):     {}", r.copied_total);
    println!("  Created nodes (total):    {}", r.created_total);
    println!(
        "  avg copied/write:         {:.3}",
        r.copied_total as f64 / n as f64
    );
    println!(
        "  avg created/write:        {:.3}",
        r.created_total as f64 / n as f64
    );
    println!(
        "  warm overwrite copies:    {}  (creates {})",
        r.warm_overwrite_copied, r.warm_overwrite_created
    );
    println!("  density amortization (copied/write, created/write):");
    for p in &r.curve {
        println!(
            "    {:>8} writes: copy {:.3}  create {:.3}",
            p.writes_done, p.copied_per_write, p.created_per_write
        );
    }
    println!();
}

fn main() {
    // A few shapes: small, medium, and a wide/deep one to stress the paths.
    let shapes = vec![
        Shape { name: "small  10 x 20 x 12", fan_out: [10, 20, 12] },
        Shape { name: "medium 50 x 200 x 60", fan_out: [50, 200, 60] },
        Shape { name: "wide   200 x 50 x 50", fan_out: [200, 50, 50] },
        Shape { name: "deep   8 x 5000 x 5", fan_out: [8, 5000, 5] },
    ];

    println!("Phase 1b trie write-cost benchmark");
    println!("(simulated A1 CoW vs. real mutable store)\n");

    for s in &shapes {
        for &randomized in &[false, true] {
            let report = run_shape(s, randomized);
            print_report(&report);
        }
    }

    println!("Reading: 'copy' is the irreducible CoW cost (the ancestor chain that");
    println!("changes on every write). 'create' is new structure, which amortizes");
    println!("down as the cube fills. If copy/write stays small and shrinks with");
    println!("density, A1 is cheap and Phase 1b proceeds as planned.");
}
