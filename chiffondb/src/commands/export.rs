use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::Path;

use chiffondb_core::db::Database;

/// Exports to a CSV or JSON file.
pub fn run(db: &Path, format: &str, type_name: Option<&str>, out: Option<&str>) -> Result<()> {
    let mut db =
        Database::open(db).with_context(|| format!("failed to open DB: {}", db.display()))?;

    let content = match format {
        "csv" => {
            let t = type_name.with_context(|| "--type is required for CSV export")?;
            export_csv(&mut db, t)?
        }
        "json" => export_json(&mut db, type_name)?,
        _ => bail!("unsupported format: {}. Use 'csv' or 'json'", format),
    };

    match out {
        Some(path) => {
            std::fs::write(path, &content).with_context(|| format!("failed to write to {path}"))?;
        }
        None => {
            std::io::stdout()
                .write_all(content.as_bytes())
                .with_context(|| "failed to write to stdout")?;
        }
    }

    Ok(())
}

/// Returns nodes or edges of the given type in CSV format.
/// For edges, :FROM / :TO columns are added.
fn export_csv(db: &mut Database, type_name: &str) -> Result<String> {
    // Try it as a node type first
    let registry = db
        .load_schema_registry()
        .with_context(|| "failed to load schema")?;

    let is_node = registry.node_type_id(type_name).is_some();
    let is_edge = registry.edge_type_id(type_name).is_some();

    if !is_node && !is_edge {
        bail!("unknown type: '{type_name}'");
    }

    let mut wtr = csv::Writer::from_writer(Vec::new());

    if is_node {
        let nodes = db
            .list_nodes(Some(type_name))
            .with_context(|| format!("failed to list nodes of type '{type_name}'"))?;

        if nodes.is_empty() {
            // Write only the header
            wtr.write_record([":TYPE"])
                .with_context(|| "failed to write CSV")?;
        } else {
            // Determine the header from the property keys of the first record
            let mut keys: Vec<String> = nodes[0].1.keys().cloned().collect();
            keys.sort();
            let mut header = vec![":TYPE".to_string()];
            header.extend(keys.clone());
            wtr.write_record(&header)
                .with_context(|| "failed to write CSV header")?;

            for (_, props) in &nodes {
                let mut row = vec![type_name.to_string()];
                for key in &keys {
                    let val = props.get(key).map(json_value_to_csv).unwrap_or_default();
                    row.push(val);
                }
                wtr.write_record(&row)
                    .with_context(|| "failed to write CSV row")?;
            }
        }
    } else {
        // Edge
        let edges = db
            .list_edges(Some(type_name))
            .with_context(|| format!("failed to list edges of type '{type_name}'"))?;

        // Build a map to resolve the `id` property of each edge's from/to node
        let node_rid_to_id: std::collections::HashMap<_, _> = db
            .list_nodes(None)
            .with_context(|| "failed to list nodes for edge mapping")?
            .into_iter()
            .filter_map(|(rid, props)| {
                props
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|id| (rid, id.to_string()))
            })
            .collect();

        if edges.is_empty() {
            wtr.write_record([":TYPE", ":FROM", ":TO"])
                .with_context(|| "failed to write CSV")?;
        } else {
            let mut keys: Vec<String> = edges[0].1.keys().cloned().collect();
            keys.sort();
            let mut header = vec![":TYPE".to_string(), ":FROM".to_string(), ":TO".to_string()];
            header.extend(keys.clone());
            wtr.write_record(&header)
                .with_context(|| "failed to write CSV header")?;

            for (eid, props) in &edges {
                let (from_node, to_node) = db
                    .get_edge_endpoints(*eid)
                    .with_context(|| format!("failed to read edge {:?}", eid))?;
                let from_id = node_rid_to_id.get(&from_node).cloned().unwrap_or_else(|| {
                    format!("{}:{}", from_node.page_id().0, from_node.slot_id().0)
                });
                let to_id = node_rid_to_id
                    .get(&to_node)
                    .cloned()
                    .unwrap_or_else(|| format!("{}:{}", to_node.page_id().0, to_node.slot_id().0));

                let mut row = vec![type_name.to_string(), from_id, to_id];
                for key in &keys {
                    let val = props.get(key).map(json_value_to_csv).unwrap_or_default();
                    row.push(val);
                }
                wtr.write_record(&row)
                    .with_context(|| "failed to write CSV row")?;
            }
        }
    }

    wtr.flush().with_context(|| "failed to flush CSV")?;
    let bytes = wtr
        .into_inner()
        .with_context(|| "failed to get CSV bytes")?;
    String::from_utf8(bytes).with_context(|| "CSV output is not valid UTF-8")
}

/// Returns all nodes and edges as a JSON array (nodes first, edges second).
fn export_json(db: &mut Database, type_name: Option<&str>) -> Result<String> {
    let mut items: Vec<serde_json::Value> = Vec::new();

    // Output only nodes filtered by type_name (all nodes when no filter)
    let filtered_nodes = db
        .list_nodes(type_name)
        .with_context(|| "failed to list nodes")?;

    // The id->rid map (for resolving an edge's from/to) is built from the
    // filtered nodes; note that edges may reference nodes outside the filter.
    let node_rid_to_id: std::collections::HashMap<_, _> = filtered_nodes
        .iter()
        .filter_map(|(rid, props)| {
            props
                .get("id")
                .and_then(|v| v.as_str())
                .map(|id| (*rid, id.to_string()))
        })
        .collect();

    for (rid, props) in &filtered_nodes {
        let type_label = db.get_node_type_name(*rid).unwrap_or_default();
        items.push(serde_json::json!({
            "type": type_label,
            "rid": { "page": rid.page_id().0, "slot": rid.slot_id().0 },
            "props": props,
        }));
    }

    // Edges (empty when type_name is a node type, since no edge matches it)
    let edges = db
        .list_edges(type_name)
        .with_context(|| "failed to list edges")?;
    for (eid, props) in edges {
        let type_label = db.get_edge_type_name(eid).unwrap_or_default();
        let (from_node, to_node) = db
            .get_edge_endpoints(eid)
            .with_context(|| format!("failed to read edge {:?}", eid))?;
        let from_id = node_rid_to_id.get(&from_node).cloned().unwrap_or_default();
        let to_id = node_rid_to_id.get(&to_node).cloned().unwrap_or_default();

        items.push(serde_json::json!({
            "type": type_label,
            "rid": { "page": eid.page_id().0, "slot": eid.slot_id().0 },
            "from": from_id,
            "to":   to_id,
            "props": props,
        }));
    }

    serde_json::to_string_pretty(&items).with_context(|| "failed to serialize JSON")
}

/// Converts a serde_json::Value into a CSV cell string.
fn json_value_to_csv(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chiffondb_core::db::Database;
    use serde_json::json;
    use std::collections::HashMap;
    use tempfile::NamedTempFile;

    const SCHEMA: &str = r#"
        node User { id: String  name: String }
        node Project { id: String  title: String }
        edge OWNS { from: User  to: Project  props: { role: String } }
    "#;

    fn make_db_with_data() -> std::path::PathBuf {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.into_temp_path().to_path_buf();
        std::fs::remove_file(&path).ok();
        let mut db = Database::create(&path).unwrap();
        db.apply_schema(SCHEMA).unwrap();
        db.flush().unwrap();

        let u = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        let p = db
            .insert_node_by_name(
                "Project",
                HashMap::from([
                    ("id".to_string(), json!("p1")),
                    ("title".to_string(), json!("Alpha")),
                ]),
            )
            .unwrap();
        db.insert_edge_by_name(
            "OWNS",
            u,
            p,
            HashMap::from([("role".to_string(), json!("owner"))]),
        )
        .unwrap();
        db.flush().unwrap();

        path
    }

    #[test]
    fn export_csv_nodes() {
        let path = make_db_with_data();
        let mut db = Database::open(&path).unwrap();
        let csv = export_csv(&mut db, "User").unwrap();
        assert!(csv.contains(":TYPE"));
        assert!(csv.contains("User"));
        assert!(csv.contains("Alice"));
        assert!(csv.contains("u1"));
    }

    #[test]
    fn export_csv_edges_has_from_to() {
        let path = make_db_with_data();
        let mut db = Database::open(&path).unwrap();
        let csv = export_csv(&mut db, "OWNS").unwrap();
        assert!(csv.contains(":FROM"));
        assert!(csv.contains(":TO"));
        assert!(csv.contains("OWNS"));
        assert!(csv.contains("u1"));
        assert!(csv.contains("p1"));
    }

    #[test]
    fn export_csv_unknown_type_returns_error() {
        let path = make_db_with_data();
        let mut db = Database::open(&path).unwrap();
        assert!(export_csv(&mut db, "Unknown").is_err());
    }

    #[test]
    fn export_json_all() {
        let path = make_db_with_data();
        let mut db = Database::open(&path).unwrap();
        let json_str = export_json(&mut db, None).unwrap();
        let items: Vec<serde_json::Value> = serde_json::from_str(&json_str).unwrap();
        // 2 nodes + 1 edge
        assert_eq!(items.len(), 3);
        // Nodes come first
        assert_eq!(items[0]["type"], json!("User"));
        assert_eq!(items[1]["type"], json!("Project"));
        // Edges come after
        assert_eq!(items[2]["type"], json!("OWNS"));
        assert_eq!(items[2]["from"], json!("u1"));
        assert_eq!(items[2]["to"], json!("p1"));
    }

    #[test]
    fn export_json_filtered_by_type() {
        let path = make_db_with_data();
        let mut db = Database::open(&path).unwrap();
        let json_str = export_json(&mut db, Some("User")).unwrap();
        let items: Vec<serde_json::Value> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], json!("User"));
    }

    #[test]
    fn run_export_csv_to_file() {
        let path = make_db_with_data();
        let out = NamedTempFile::new().unwrap();
        run(
            &path,
            "csv",
            Some("User"),
            Some(out.path().to_str().unwrap()),
        )
        .unwrap();
        let content = std::fs::read_to_string(out.path()).unwrap();
        assert!(content.contains("Alice"));
    }

    #[test]
    fn run_export_json_to_file() {
        let path = make_db_with_data();
        let out = NamedTempFile::new().unwrap();
        run(&path, "json", None, Some(out.path().to_str().unwrap())).unwrap();
        let content = std::fs::read_to_string(out.path()).unwrap();
        let items: Vec<serde_json::Value> = serde_json::from_str(&content).unwrap();
        assert_eq!(items.len(), 3);
    }
}
