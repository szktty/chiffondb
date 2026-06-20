use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;

use chiffondb_core::db::{Database, DatabaseGraphView};
use chiffondb_core::traversal::command::{CollectResult, TraversalCommand};
use chiffondb_core::traversal::executor::execute;

pub fn run(db: &Path, json_arg: &str) -> Result<()> {
    let json_str = if json_arg == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        std::fs::read_to_string(json_arg)
            .with_context(|| format!("failed to read query file: {json_arg}"))?
    };

    let cmd: TraversalCommand =
        serde_json::from_str(&json_str).with_context(|| "failed to parse traversal command")?;

    let mut db =
        Database::open(db).with_context(|| format!("failed to open DB: {}", db.display()))?;

    let view = DatabaseGraphView::load(&mut db).with_context(|| "failed to load graph")?;

    let result = execute(&view, &cmd).map_err(|e| anyhow::anyhow!("{}", e))?;

    let output = match result {
        CollectResult::Rows(rows) => serde_json::json!({
            "success": true,
            "results": rows,
        }),
        CollectResult::Count(n) => serde_json::json!({
            "success": true,
            "count": n,
        }),
        CollectResult::Exists(b) => serde_json::json!({
            "success": true,
            "exists": b,
        }),
        CollectResult::Aggregate(map) => serde_json::json!({
            "success": true,
            "aggregate": map,
        }),
        CollectResult::Groups(groups) => serde_json::json!({
            "success": true,
            "groups": groups,
        }),
        CollectResult::Path(paths) => serde_json::json!({
            "success": true,
            "paths": paths,
        }),
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
