use anyhow::{Context, Result};
use chiffondb_core::storage::file::DatabaseFile;
use std::path::Path;

pub fn run(db: &Path) -> Result<()> {
    let mut db_file =
        DatabaseFile::open(db).with_context(|| format!("failed to open DB: {}", db.display()))?;

    let total_pages = db_file
        .page_count()
        .with_context(|| "failed to read page count")?;

    let header = &db_file.header;
    let topo_start = header.topology_segment_start;
    let prop_start = header.property_segment_start;
    let vec_start = header.vector_segment_start;

    let topo_pages = prop_start.saturating_sub(topo_start);
    let prop_pages = if vec_start > 0 {
        vec_start.saturating_sub(prop_start)
    } else {
        total_pages.saturating_sub(prop_start)
    };
    let vec_pages = if vec_start > 0 {
        total_pages.saturating_sub(vec_start)
    } else {
        0
    };

    println!("File:            {}", db.display());
    println!("Version:         {}", header.version);
    println!("Page size:       4096 bytes");
    println!("Total pages:     {}", total_pages);
    println!("Topology pages:  {}", topo_pages);
    println!("Property pages:  {}", prop_pages);
    println!("Vector pages:    {} (reserved)", vec_pages);
    Ok(())
}
