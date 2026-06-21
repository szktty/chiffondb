use anyhow::{Context, Result};
use chiffondb_core::schema::{parser, store, validator};
use chiffondb_core::storage::file::DatabaseFile;
use std::path::Path;

pub fn run_apply(db: &Path, schema_path: &Path) -> Result<()> {
    let src = std::fs::read_to_string(schema_path)
        .with_context(|| format!("failed to read schema file: {}", schema_path.display()))?;

    let ast = parser::parse(&src).with_context(|| "failed to parse schema")?;

    validator::validate(&ast).with_context(|| "schema validation failed")?;

    let mut db =
        DatabaseFile::open(db).with_context(|| format!("failed to open DB: {}", db.display()))?;

    store::save_schema(&mut db, &ast).with_context(|| "failed to save schema")?;

    println!("Schema applied.");
    Ok(())
}

pub fn run_show(db: &Path) -> Result<()> {
    let mut db =
        DatabaseFile::open(db).with_context(|| format!("failed to open DB: {}", db.display()))?;

    let ast = store::load_schema(&mut db).with_context(|| "failed to load schema")?;

    for def in &ast.definitions {
        match def {
            chiffondb_core::schema::ast::Definition::Node(n) => {
                println!("node {} {{", n.name);
                for f in &n.fields {
                    println!("    {}: {:?}", f.name, f.type_expr);
                }
                println!("}}");
            }
            chiffondb_core::schema::ast::Definition::Edge(e) => {
                println!("edge {} {{", e.name);
                println!("    from: {:?}", e.from);
                println!("    to: {:?}", e.to);
                println!("}}");
            }
        }
    }
    Ok(())
}
