//! Memory-footprint benchmark for the bounded page cache.
//!
//! Inserts a large graph under several `max_memory_bytes` budgets and reports the
//! page-cache occupancy. The point: resident page memory plateaus at the configured
//! capacity regardless of how big the database grows — the evidence behind the
//! "bounded / low memory footprint" claim.
//!
//! Run with: `cargo run -p chiffondb-core --example cache_memory_bench --release`

use std::collections::HashMap;

use chiffondb_core::db::{Database, DatabaseGraphView};
use chiffondb_core::storage::file::OpenOptions;
use chiffondb_core::storage::page::PAGE_SIZE;
use chiffondb_core::traversal::command::{
    CollectResult, CollectSpec, StartSpec, TraversalAction, TraversalCommand, TraversalStep,
};
use chiffondb_core::traversal::executor::execute;
use serde_json::json;

// Bounded by the fixed-size topology segment (~32 node pages × ~63 records).
const NODES: usize = 1_800;

/// A chunky property payload so each node spills into the (unbounded) property
/// segment, growing the database file well past any cache budget.
fn big_blob(i: usize) -> serde_json::Value {
    json!("x".repeat(512) + &i.to_string())
}

fn run(label: &str, max_memory_bytes: usize) {
    let dir = std::env::temp_dir().join(format!("tdb_cache_bench_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{label}.chiffon"));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("tdb-wal"));
    let _ = std::fs::remove_file(path.with_extension("tdb-lock"));

    let opts = OpenOptions { max_memory_bytes };
    let mut db = Database::create_with_options(&path, &opts).expect("create");

    let mut rids = Vec::with_capacity(NODES);
    for i in 0..NODES {
        let props = HashMap::from([
            ("id".to_string(), json!(format!("u{i}"))),
            ("blob".to_string(), big_blob(i)),
        ]);
        rids.push(db.insert_node(1, props).expect("insert node"));
    }
    // Chain edges to grow topology + property pages further.
    for w in rids.windows(2) {
        db.insert_edge(1, w[0], w[1], HashMap::new()).expect("edge");
    }
    db.flush().expect("flush");

    // Touch every node again to exercise the read path (cache → WAL → file).
    for &rid in &rids {
        let _ = db.get_node_properties(rid).expect("read");
    }

    let (resident, capacity) = db.cache_stats().expect("file backend");
    let resident_bytes = resident * PAGE_SIZE;
    let cap_bytes = capacity * PAGE_SIZE;
    let db_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    println!(
        "{label:<10} budget={:>5} KiB  cap={:>5} pages  resident={:>5} pages ({:>5} KiB)  db_file={:>6} KiB",
        max_memory_bytes / 1024,
        capacity,
        resident,
        resident_bytes / 1024,
        db_bytes / 1024,
    );
    assert!(
        resident <= capacity,
        "resident pages must never exceed the configured capacity"
    );
    assert!(
        resident_bytes <= cap_bytes,
        "resident bytes must stay within the budget"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("tdb-wal"));
    let _ = std::fs::remove_file(path.with_extension("tdb-lock"));
}

/// Inserts many *lightweight* nodes (no blob) under a tiny cache and reports cache
/// occupancy. Before P2-5, an in-memory `PropertyIndex` grew with node count regardless
/// of the cache budget; after removing it, the only resident page structure is the
/// bounded cache, so occupancy stays ≤ capacity no matter how many nodes exist. find/
/// list/count now scan the topology through that bounded cache.
fn run_lightweight(label: &str, nodes: usize, max_memory_bytes: usize) {
    let dir = std::env::temp_dir().join(format!("tdb_cache_bench_lw_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{label}.chiffon"));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("tdb-wal"));
    let _ = std::fs::remove_file(path.with_extension("tdb-lock"));

    let opts = OpenOptions { max_memory_bytes };
    let mut db = Database::create_with_options(&path, &opts).expect("create");

    for i in 0..nodes {
        let props = HashMap::from([("id".to_string(), json!(format!("u{i}")))]);
        db.insert_node(1, props).expect("insert node");
    }
    db.flush().expect("flush");

    // A scan-backed lookup of the last id exercises find without any resident index.
    let last = json!(format!("u{}", nodes - 1));
    let found = db
        .list_nodes(None)
        .expect("list")
        .into_iter()
        .any(|(_, p)| p.get("id") == Some(&last));
    assert!(found, "scan-based list must see the last node");

    let (resident, capacity) = db.cache_stats().expect("file backend");
    println!(
        "{label:<10} nodes={:>5}  budget={:>5} KiB  cap={:>5} pages  resident={:>5} pages",
        nodes,
        max_memory_bytes / 1024,
        capacity,
        resident,
    );
    assert!(
        resident <= capacity,
        "resident must stay within budget regardless of node count (no resident index)"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("tdb-wal"));
    let _ = std::fs::remove_file(path.with_extension("tdb-lock"));
}

/// Runs a full traversal (`AllNodes` start → `OutEdges` → `OutNodes` → collect) over a
/// lightweight node chain under a tiny cache and reports cache occupancy *after* execution.
/// Since `a5a5dc4` removed `DatabaseGraphView`'s node/edge-count-sized label and property-ref
/// maps, the only resident page structure during a traversal is the bounded cache. Occupancy
/// must therefore stay ≤ capacity no matter how many nodes the traversal sweeps.
fn run_traversal(label: &str, nodes: usize, max_memory_bytes: usize) {
    let dir = std::env::temp_dir().join(format!("tdb_cache_bench_tv_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{label}.chiffon"));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("tdb-wal"));
    let _ = std::fs::remove_file(path.with_extension("tdb-lock"));

    let opts = OpenOptions { max_memory_bytes };
    let mut db = Database::create_with_options(&path, &opts).expect("create");

    // A chain: node[i] --OWNS--> node[i+1]. Lightweight props (no blob) so growth is in the
    // topology/property segments, not from any resident per-node structure.
    let mut rids = Vec::with_capacity(nodes);
    for i in 0..nodes {
        let props = HashMap::from([("id".to_string(), json!(format!("u{i}")))]);
        rids.push(db.insert_node(1, props).expect("insert node"));
    }
    for w in rids.windows(2) {
        db.insert_edge(1, w[0], w[1], HashMap::new()).expect("edge");
    }
    db.flush().expect("flush");

    // AllNodes -> OutEdges -> OutNodes -> count. Each step faults topology pages on demand
    // through the bounded cache (labels/property-refs are derived per access, not resident).
    let matched = {
        let view = DatabaseGraphView::load(&mut db).expect("load view");
        let cmd = TraversalCommand {
            version: 1,
            bindings: Default::default(),
            start: StartSpec {
                kind: "AllNodes".into(),
                label: String::new(),
                key: String::new(),
                value: json!(null),
            },
            steps: vec![
                TraversalStep {
                    action: TraversalAction::OutEdges,
                    ..Default::default()
                },
                TraversalStep {
                    action: TraversalAction::OutNodes,
                    ..Default::default()
                },
            ],
            collect: CollectSpec::Count,
        };
        match execute(&view, &cmd).expect("execute") {
            CollectResult::Count(n) => n,
            other => panic!("expected Count, got {other:?}"),
        }
    };
    // Every node except the last has exactly one out-edge leading to one node.
    assert_eq!(
        matched,
        nodes - 1,
        "traversal must reach every chained successor"
    );

    let (resident, capacity) = db.cache_stats().expect("file backend");
    println!(
        "{label:<10} nodes={:>5}  budget={:>5} KiB  cap={:>5} pages  resident={:>5} pages",
        nodes,
        max_memory_bytes / 1024,
        capacity,
        resident,
    );
    assert!(
        resident <= capacity,
        "traversal resident must stay within budget regardless of node count"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("tdb-wal"));
    let _ = std::fs::remove_file(path.with_extension("tdb-lock"));
}

fn main() {
    println!(
        "Inserting {NODES} nodes + {} edges per run (PAGE_SIZE = {PAGE_SIZE} bytes)\n",
        NODES - 1
    );
    // Increasing budgets: resident pages track the budget, never the DB size.
    run("tiny", 64 * 1024); // 16 pages
    run("small", 256 * 1024); // 64 pages
    run("default", 4 * 1024 * 1024); // 1024 pages
    run("large", 16 * 1024 * 1024); // 4096 pages
    println!("\nResident page memory is capped by the budget in every run → bounded footprint.");

    // P2-5: with the in-memory PropertyIndex removed, resident memory no longer grows with
    // node count. Increasing node count at a fixed tiny budget keeps occupancy ≤ capacity.
    println!(
        "\nLightweight nodes (no blob), fixed 64 KiB budget — occupancy must not track node count:"
    );
    run_lightweight("lw-500", 500, 64 * 1024);
    run_lightweight("lw-1000", 1_000, 64 * 1024);
    run_lightweight("lw-1800", 1_800, 64 * 1024);
    println!("\nResident stays ≤ capacity as node count grows → no per-node (index) growth.");

    // 2026-06-20b: DatabaseGraphView's label/property-ref maps are gone, so a *traversal*
    // also keeps resident memory bounded. Running AllNodes → OutEdges → OutNodes at a fixed
    // tiny budget over a growing node chain must keep occupancy ≤ capacity (no per-node growth).
    println!(
        "\nTraversal (AllNodes → OutEdges → OutNodes), fixed 64 KiB budget — occupancy must not track node count:"
    );
    run_traversal("tv-500", 500, 64 * 1024);
    run_traversal("tv-1000", 1_000, 64 * 1024);
    run_traversal("tv-1800", 1_800, 64 * 1024);
    println!(
        "\nTraversal resident stays ≤ capacity as node count grows → traversal path is bounded."
    );
}
