use anyhow::{Context, Result};
use chiffondb_core::storage::file::DatabaseFile;
use std::path::Path;

pub fn run(db: &Path) -> Result<()> {
    let mut db_file =
        DatabaseFile::open(db).with_context(|| format!("failed to open DB: {}", db.display()))?;

    let total_pages = db_file
        .page_count()
        .with_context(|| "failed to read page count")?;

    // Pages no longer live in fixed segments; report the directory-backed logical page counts
    // (node/edge/property), which are what actually bound the data now.
    let header = &db_file.header;
    let version = header.version;
    let node_pages = header.node_page_count;
    let edge_pages = header.edge_page_count;
    let property_pages = header.property_dir_len;

    println!("File:            {}", db.display());
    println!("Version:         {}", version);
    println!("Page size:       4096 bytes");
    println!("Total pages:     {}", total_pages);
    println!("Node pages:      {}", node_pages);
    println!("Edge pages:      {}", edge_pages);
    println!("Property pages:  {}", property_pages);
    Ok(())
}
