use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

use chiffondb_core::db::Database;
use serde_json::Value;

pub fn run_add(db: &Path, type_name: &str, props_json: &str) -> Result<()> {
    let props: HashMap<String, Value> =
        serde_json::from_str(props_json).with_context(|| "failed to parse --props JSON")?;

    let mut db =
        Database::open(db).with_context(|| format!("failed to open DB: {}", db.display()))?;

    let rid = db
        .insert_node_by_name(type_name, props)
        .with_context(|| format!("failed to insert node of type '{type_name}'"))?;

    db.flush().with_context(|| "failed to flush DB")?;

    println!(
        "Node inserted: page={} slot={}",
        rid.page_id().0,
        rid.slot_id().0
    );
    Ok(())
}
