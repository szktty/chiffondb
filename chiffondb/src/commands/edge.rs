use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

use chiffondb_core::db::Database;
use chiffondb_core::storage::page::NodeRid;
use serde_json::Value;

pub fn run_add(
    db: &Path,
    type_name: &str,
    from_page: u32,
    from_slot: u16,
    to_page: u32,
    to_slot: u16,
    props_json: &str,
) -> Result<()> {
    let props: HashMap<String, Value> =
        serde_json::from_str(props_json).with_context(|| "failed to parse --props JSON")?;

    let mut db =
        Database::open(db).with_context(|| format!("failed to open DB: {}", db.display()))?;

    let from = NodeRid::new(from_page, from_slot);
    let to = NodeRid::new(to_page, to_slot);

    let rid = db
        .insert_edge_by_name(type_name, from, to, props)
        .with_context(|| format!("failed to insert edge of type '{type_name}'"))?;

    db.flush().with_context(|| "failed to flush DB")?;

    println!(
        "Edge inserted: page={} slot={}",
        rid.page_id().0,
        rid.slot_id().0
    );
    Ok(())
}
