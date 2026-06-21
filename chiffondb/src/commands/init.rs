use anyhow::{Context, Result};
use chiffondb_core::db::Database;
use std::path::Path;

pub fn run(db: &Path) -> Result<()> {
    Database::create(db).with_context(|| format!("failed to create DB at {}", db.display()))?;
    println!("Created: {}", db.display());
    Ok(())
}
