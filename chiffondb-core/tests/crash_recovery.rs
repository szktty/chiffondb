//! Crash-recovery regression tests (promoted from the 2026-07-10 review probe for M-1).
//!
//! `DatabaseFile::open` must checkpoint the WAL before reading the header. The header (node/edge/
//! property dir roots, page counts, label/property index roots) is updated through the WAL, so a
//! crash/drop before flush leaves the newest header only in the WAL. Reading the main-file header
//! first would load a stale header and orphan every change since the last flush.

use chiffondb_core::db::Database;
use std::collections::HashMap;

#[test]
fn recovery_after_drop_without_flush_preserves_nodes() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("probe.chiffon");

    let rid;
    {
        let mut db = Database::create(&path).unwrap();
        // insert_node updates the header (node dir root / page count) through the WAL.
        rid = db
            .insert_node(
                1,
                HashMap::from([("k".to_string(), serde_json::json!("v"))]),
            )
            .unwrap();
        // Simulate a crash: drop WITHOUT flush. The WAL file retains all writes.
    }

    let wal = path.with_file_name("probe.chiffon-wal");
    assert!(wal.exists(), "WAL should exist before recovery");

    {
        let mut db = Database::open(&path).unwrap();
        let nodes = db.list_nodes(None).unwrap();
        assert_eq!(
            nodes.len(),
            1,
            "node inserted before crash must be visible after WAL recovery"
        );
        let props = db.get_node_properties(rid).unwrap();
        assert_eq!(props.get("k"), Some(&serde_json::json!("v")));
    }
}

#[test]
fn recovery_after_drop_without_flush_preserves_schema() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("probe2.chiffon");
    {
        let mut db = Database::create(&path).unwrap();
        db.apply_schema("node User { id: String @unique }").unwrap();
        let _ = db
            .insert_node_by_name(
                "User",
                HashMap::from([("id".into(), serde_json::json!("u1"))]),
            )
            .unwrap();
        // drop without flush
    }
    {
        let mut db = Database::open(&path).unwrap();
        let nodes = db.list_nodes(Some("User")).unwrap();
        assert_eq!(nodes.len(), 1, "typed node must survive crash recovery");
    }
}
