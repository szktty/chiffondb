use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use chiffondb_core::db::Database;
use serde_json::Value;

/// Imports from a CSV or JSON file.
pub fn run(db: &Path, format: &str, id_prop: &str, input: &str) -> Result<()> {
    let mut db =
        Database::open(db).with_context(|| format!("failed to open DB: {}", db.display()))?;

    let content = read_input(input)?;

    let (nodes, edges) = match format {
        "csv" => parse_csv(&content)?,
        "json" => parse_json(&content)?,
        _ => bail!("unsupported format: {}. Use 'csv' or 'json'", format),
    };

    let mut node_id_to_rid: HashMap<String, chiffondb_core::storage::page::NodeRid> =
        HashMap::new();

    // Index existing nodes in the DB by id_prop (needed for edge-only imports)
    if let Ok(existing) = db.list_nodes(None) {
        for (rid, props) in existing {
            if let Some(Value::String(id_str)) = props.get(id_prop) {
                node_id_to_rid.insert(id_str.clone(), rid);
            }
        }
    }

    // Insert nodes first, building the id_prop -> RecordId map
    let mut node_count = 0usize;
    for (type_name, props) in nodes {
        let id_val = props.get(id_prop).cloned();
        let rid = db
            .insert_node_by_name(&type_name, props)
            .with_context(|| format!("failed to insert node of type '{type_name}'"))?;
        if let Some(Value::String(id_str)) = id_val {
            node_id_to_rid.insert(id_str, rid);
        }
        node_count += 1;
    }

    // Insert edges
    let mut edge_count = 0usize;
    for (type_name, from_id, to_id, props) in edges {
        let from_rid = node_id_to_rid
            .get(&from_id)
            .copied()
            .with_context(|| format!("node with {id_prop}='{from_id}' not found"))?;
        let to_rid = node_id_to_rid
            .get(&to_id)
            .copied()
            .with_context(|| format!("node with {id_prop}='{to_id}' not found"))?;
        db.insert_edge_by_name(&type_name, from_rid, to_rid, props)
            .with_context(|| format!("failed to insert edge of type '{type_name}'"))?;
        edge_count += 1;
    }

    db.flush().with_context(|| "failed to flush DB")?;

    println!("Imported: {node_count} nodes, {edge_count} edges");
    Ok(())
}

fn read_input(input: &str) -> Result<String> {
    if input == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .with_context(|| "failed to read stdin")?;
        Ok(buf)
    } else {
        std::fs::read_to_string(input).with_context(|| format!("failed to read file: {input}"))
    }
}

type NodeRecord = (String, HashMap<String, Value>);
type EdgeRecord = (String, String, String, HashMap<String, Value>);

/// Parses CSV into a list of nodes and a list of edges.
/// `:TYPE` is required. Rows with `:FROM` and `:TO` are treated as edges.
fn parse_csv(content: &str) -> Result<(Vec<NodeRecord>, Vec<EdgeRecord>)> {
    let mut reader = csv::Reader::from_reader(content.as_bytes());
    let headers: Vec<String> = reader
        .headers()
        .with_context(|| "failed to read CSV headers")?
        .iter()
        .map(|s| s.to_string())
        .collect();

    let type_col = headers
        .iter()
        .position(|h| h == ":TYPE")
        .with_context(|| "CSV must have a ':TYPE' column")?;
    let from_col = headers.iter().position(|h| h == ":FROM");
    let to_col = headers.iter().position(|h| h == ":TO");

    // Indices of property columns (excluding :TYPE / :FROM / :TO)
    let prop_cols: Vec<(usize, &str)> = headers
        .iter()
        .enumerate()
        .filter(|(_, h)| h.as_str() != ":TYPE" && h.as_str() != ":FROM" && h.as_str() != ":TO")
        .map(|(i, h)| (i, h.as_str()))
        .collect();

    let is_edge = from_col.is_some() && to_col.is_some();

    let mut nodes: Vec<NodeRecord> = Vec::new();
    let mut edges: Vec<EdgeRecord> = Vec::new();

    for result in reader.records() {
        let record = result.with_context(|| "failed to read CSV record")?;
        let type_name = record
            .get(type_col)
            .with_context(|| "missing :TYPE value")?
            .to_string();
        if type_name.is_empty() {
            bail!("empty :TYPE value in CSV row");
        }

        let props: HashMap<String, Value> = prop_cols
            .iter()
            .filter_map(|&(i, key)| record.get(i).map(|v| (key.to_string(), infer_csv_value(v))))
            .collect();

        if is_edge {
            let from_id = record
                .get(from_col.unwrap())
                .with_context(|| "missing :FROM value")?
                .to_string();
            let to_id = record
                .get(to_col.unwrap())
                .with_context(|| "missing :TO value")?
                .to_string();
            edges.push((type_name, from_id, to_id, props));
        } else {
            nodes.push((type_name, props));
        }
    }

    Ok((nodes, edges))
}

/// Infers a JSON Value from a CSV cell string.
/// true/false -> Boolean, integer -> Int, decimal -> Float,
/// ISO 8601 -> DateTime (UNIX ms), anything else -> String.
fn infer_csv_value(s: &str) -> Value {
    if s == "true" {
        return Value::Bool(true);
    }
    if s == "false" {
        return Value::Bool(false);
    }
    if let Ok(i) = s.parse::<i64>() {
        return Value::Number(i.into());
    }
    if let Ok(f) = s.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return Value::Number(n);
        }
    }
    // ISO 8601 -> UNIX milliseconds
    if let Ok(dt) = chrono_parse_iso8601(s) {
        return Value::Number(dt.into());
    }
    Value::String(s.to_string())
}

/// Converts an ISO 8601 string into UNIX milliseconds (a simple,
/// chrono-free implementation). Returns Err on failure.
fn chrono_parse_iso8601(s: &str) -> Result<i64, ()> {
    // Only the YYYY-MM-DDThh:mm:ssZ format is supported
    if s.len() < 19 {
        return Err(());
    }
    let year: i64 = s[0..4].parse().map_err(|_| ())?;
    let month: u32 = s[5..7].parse().map_err(|_| ())?;
    let day: u32 = s[8..10].parse().map_err(|_| ())?;
    let hour: i64 = s[11..13].parse().map_err(|_| ())?;
    let min: i64 = s[14..16].parse().map_err(|_| ())?;
    let sec: i64 = s[17..19].parse().map_err(|_| ())?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(());
    }
    // Simple Gregorian-date -> Unix-seconds conversion (leap seconds ignored)
    let days = days_from_epoch(year, month, day)?;
    let unix_sec = days * 86400 + hour * 3600 + min * 60 + sec;
    Ok(unix_sec * 1000)
}

fn days_from_epoch(year: i64, month: u32, day: u32) -> Result<i64, ()> {
    // Computation based on the Julian Day Number
    let m = month as i64;
    let d = day as i64;
    let y = if m <= 2 { year - 1 } else { year };
    let m2 = if m <= 2 { m + 12 } else { m };
    let a = y / 100;
    let b = 2 - a + a / 4;
    let jd =
        ((365.25 * (y + 4716) as f64) as i64) + ((30.6001 * (m2 + 1) as f64) as i64) + d + b - 1524;
    // The Julian Day of the Unix epoch (1970-01-01) is 2440588
    Ok(jd - 2440588)
}

/// Parses a JSON array into a list of nodes and a list of edges.
/// Elements with `"from"` / `"to"` keys are treated as edges.
fn parse_json(content: &str) -> Result<(Vec<NodeRecord>, Vec<EdgeRecord>)> {
    let items: Vec<Value> =
        serde_json::from_str(content).with_context(|| "failed to parse JSON: expected an array")?;

    let mut nodes: Vec<NodeRecord> = Vec::new();
    let mut edges: Vec<EdgeRecord> = Vec::new();

    for (i, item) in items.iter().enumerate() {
        let obj = item
            .as_object()
            .with_context(|| format!("item[{i}] must be a JSON object"))?;
        let type_name = obj
            .get("type")
            .and_then(|v| v.as_str())
            .with_context(|| format!("item[{i}] missing 'type' key"))?
            .to_string();
        let props: HashMap<String, Value> = obj
            .get("props")
            .and_then(|v| v.as_object())
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();

        if obj.contains_key("from") && obj.contains_key("to") {
            let from_id = obj["from"]
                .as_str()
                .with_context(|| format!("item[{i}] 'from' must be a string"))?
                .to_string();
            let to_id = obj["to"]
                .as_str()
                .with_context(|| format!("item[{i}] 'to' must be a string"))?
                .to_string();
            edges.push((type_name, from_id, to_id, props));
        } else {
            nodes.push((type_name, props));
        }
    }

    Ok((nodes, edges))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chiffondb_core::db::Database;
    use serde_json::json;
    use tempfile::NamedTempFile;

    const SCHEMA: &str = r#"
        node User { id: String  name: String  score: Float  active: Boolean  createdAt: DateTime }
        node Project { id: String  title: String }
        edge FOLLOWS { from: User  to: User  props: { since: String } }
        edge OWNS { from: User  to: Project  props: { role: String } }
    "#;

    fn make_db() -> std::path::PathBuf {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.into_temp_path().to_path_buf();
        std::fs::remove_file(&path).ok();
        let mut db = Database::create(&path).unwrap();
        db.apply_schema(SCHEMA).unwrap();
        db.flush().unwrap();
        path
    }

    // ---- CSV parsing tests ----

    #[test]
    fn csv_parse_nodes() {
        let csv = ":TYPE,id,name\nUser,u1,Alice\nUser,u2,Bob\n";
        let (nodes, edges) = parse_csv(csv).unwrap();
        assert_eq!(nodes.len(), 2);
        assert!(edges.is_empty());
        assert_eq!(nodes[0].0, "User");
        assert_eq!(nodes[0].1["id"], json!("u1"));
        assert_eq!(nodes[0].1["name"], json!("Alice"));
    }

    #[test]
    fn csv_parse_edges() {
        let csv = ":TYPE,:FROM,:TO,role\nOWNS,u1,p1,owner\n";
        let (nodes, edges) = parse_csv(csv).unwrap();
        assert!(nodes.is_empty());
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].0, "OWNS");
        assert_eq!(edges[0].1, "u1");
        assert_eq!(edges[0].2, "p1");
        assert_eq!(edges[0].3["role"], json!("owner"));
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn csv_type_inference() {
        let csv = ":TYPE,id,score,active,createdAt\nUser,u1,3.14,true,2024-01-01T00:00:00Z\n";
        let (nodes, _) = parse_csv(csv).unwrap();
        let props = &nodes[0].1;
        assert_eq!(props["score"], json!(3.14));
        assert_eq!(props["active"], json!(true));
        // DateTime is an integer of UNIX milliseconds
        assert!(props["createdAt"].is_number());
        let ms = props["createdAt"].as_i64().unwrap();
        assert!(ms > 0);
    }

    #[test]
    fn csv_missing_type_column_returns_error() {
        let csv = "id,name\nu1,Alice\n";
        assert!(parse_csv(csv).is_err());
    }

    // ---- JSON parsing tests ----

    #[test]
    fn json_parse_nodes_and_edges() {
        let json_str = r#"[
            {"type":"User","props":{"id":"u1","name":"Alice"}},
            {"type":"Project","props":{"id":"p1","title":"Alpha"}},
            {"type":"OWNS","from":"u1","to":"p1","props":{"role":"owner"}}
        ]"#;
        let (nodes, edges) = parse_json(json_str).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].0, "OWNS");
        assert_eq!(edges[0].1, "u1");
        assert_eq!(edges[0].2, "p1");
    }

    #[test]
    fn json_missing_type_key_returns_error() {
        let json_str = r#"[{"props":{"id":"u1"}}]"#;
        assert!(parse_json(json_str).is_err());
    }

    #[test]
    fn json_not_array_returns_error() {
        assert!(parse_json(r#"{"type":"User"}"#).is_err());
    }

    // ---- Integration tests (actually write to the DB) ----

    #[test]
    fn csv_import_nodes_into_db() {
        let path = make_db();
        let csv = ":TYPE,id,name\nUser,u1,Alice\nUser,u2,Bob\n";
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), csv).unwrap();

        run(&path, "csv", "id", tmp.path().to_str().unwrap()).unwrap();

        let mut db = Database::open(&path).unwrap();
        let nodes = db.list_nodes(Some("User")).unwrap();
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn csv_import_edges_into_db() {
        let path = make_db();

        // Import nodes first
        let node_csv = ":TYPE,id,name\nUser,u1,Alice\nUser,u2,Bob\n";
        let tmp_nodes = NamedTempFile::new().unwrap();
        std::fs::write(tmp_nodes.path(), node_csv).unwrap();
        run(&path, "csv", "id", tmp_nodes.path().to_str().unwrap()).unwrap();

        // Import edges
        let edge_csv = ":TYPE,:FROM,:TO,since\nFOLLOWS,u1,u2,2024-01-01\n";
        let tmp_edges = NamedTempFile::new().unwrap();
        std::fs::write(tmp_edges.path(), edge_csv).unwrap();
        run(&path, "csv", "id", tmp_edges.path().to_str().unwrap()).unwrap();

        let mut db = Database::open(&path).unwrap();
        let edges = db.list_edges(Some("FOLLOWS")).unwrap();
        assert_eq!(edges.len(), 1);
    }

    #[test]
    fn json_import_nodes_and_edges_into_db() {
        let path = make_db();
        let json_str = r#"[
            {"type":"User","props":{"id":"u1","name":"Alice"}},
            {"type":"Project","props":{"id":"p1","title":"Alpha"}},
            {"type":"OWNS","from":"u1","to":"p1","props":{"role":"owner"}}
        ]"#;
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), json_str).unwrap();

        run(&path, "json", "id", tmp.path().to_str().unwrap()).unwrap();

        let mut db = Database::open(&path).unwrap();
        let nodes = db.list_nodes(Some("User")).unwrap();
        assert_eq!(nodes.len(), 1);
        let edges = db.list_edges(Some("OWNS")).unwrap();
        assert_eq!(edges.len(), 1);
    }

    #[test]
    fn csv_unknown_type_returns_error() {
        let path = make_db();
        let csv = ":TYPE,id\nUnknownType,u1\n";
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), csv).unwrap();
        assert!(run(&path, "csv", "id", tmp.path().to_str().unwrap()).is_err());
    }
}
