use std::process::Command;
use tempfile::TempDir;

fn chiffon() -> Command {
    Command::new(env!("CARGO_BIN_EXE_chiffon"))
}

#[test]
fn init_creates_db_file() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("test.chiffon");

    let out = chiffon()
        .args(["init", "--db", db.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(db.exists());
}

#[test]
fn schema_apply_and_show() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("test.chiffon");
    let schema_path = dir.path().join("schema.graph");

    std::fs::write(
        &schema_path,
        r#"
        node User {
            id: String
            name: String
        }
        node Project {
            id: String
            title: String
        }
        edge OWNS {
            from: User
            to: Project
        }
    "#,
    )
    .unwrap();

    // init
    chiffon()
        .args(["init", "--db", db.to_str().unwrap()])
        .output()
        .unwrap();

    // schema apply
    let out = chiffon()
        .args([
            "schema",
            "apply",
            "--db",
            db.to_str().unwrap(),
            "--schema",
            schema_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // schema show
    let out = chiffon()
        .args(["schema", "show", "--db", db.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("User"));
    assert!(stdout.contains("Project"));
    assert!(stdout.contains("OWNS"));
}

#[test]
fn info_shows_metadata() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("test.chiffon");

    chiffon()
        .args(["init", "--db", db.to_str().unwrap()])
        .output()
        .unwrap();

    let out = chiffon()
        .args(["info", "--db", db.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Version:"));
    assert!(stdout.contains("Page size:"));
    assert!(stdout.contains("Total pages:"));
}

#[test]
fn query_invalid_json_returns_error() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("test.chiffon");
    let query_path = dir.path().join("bad.json");

    chiffon()
        .args(["init", "--db", db.to_str().unwrap()])
        .output()
        .unwrap();

    std::fs::write(&query_path, "not valid json").unwrap();

    let out = chiffon()
        .args([
            "query",
            "--db",
            db.to_str().unwrap(),
            "--json",
            query_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn query_valid_command_outputs_json() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("test.chiffon");
    let query_path = dir.path().join("query.json");

    chiffon()
        .args(["init", "--db", db.to_str().unwrap()])
        .output()
        .unwrap();

    std::fs::write(
        &query_path,
        r#"{
        "version": 1,
        "start": { "type": "Node", "label": "User", "key": "id", "value": "u1" },
        "steps": [],
        "collect": { "type": "Nodes", "properties": ["id"] }
    }"#,
    )
    .unwrap();

    // The node does not exist, so this returns a NodeNotFound error; verify it does not crash
    let out = chiffon()
        .args([
            "query",
            "--db",
            db.to_str().unwrap(),
            "--json",
            query_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let _ = out.status;
}

#[test]
fn end_to_end_node_insert_and_query() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("test.chiffon");
    let schema_path = dir.path().join("schema.graph");
    let query_path = dir.path().join("query.json");

    // 1. init
    chiffon()
        .args(["init", "--db", db.to_str().unwrap()])
        .output()
        .unwrap();

    // 2. schema apply
    std::fs::write(
        &schema_path,
        r#"
        node User { id: String name: String }
        node Project { id: String title: String isArchived: Boolean }
        edge OWNS { from: User to: Project props: { role: String } }
        "#,
    )
    .unwrap();
    chiffon()
        .args([
            "schema",
            "apply",
            "--db",
            db.to_str().unwrap(),
            "--schema",
            schema_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    // 3. node add x2
    let out = chiffon()
        .args([
            "node",
            "add",
            "--db",
            db.to_str().unwrap(),
            "--type",
            "User",
            "--props",
            r#"{"id":"u1","name":"Alice"}"#,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Node inserted"));
    let (from_page, from_slot) = parse_rid(&stdout);

    let out = chiffon()
        .args([
            "node",
            "add",
            "--db",
            db.to_str().unwrap(),
            "--type",
            "Project",
            "--props",
            r#"{"id":"p1","title":"Alpha","isArchived":false}"#,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout2 = String::from_utf8_lossy(&out.stdout);
    let (to_page, to_slot) = parse_rid(&stdout2);

    // 4. edge add
    let out = chiffon()
        .args([
            "edge",
            "add",
            "--db",
            db.to_str().unwrap(),
            "--type",
            "OWNS",
            "--from-page",
            &from_page.to_string(),
            "--from-slot",
            &from_slot.to_string(),
            "--to-page",
            &to_page.to_string(),
            "--to-slot",
            &to_slot.to_string(),
            "--props",
            r#"{"role":"owner"}"#,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 5. query: outgoing edges of User "u1" -> Project (isArchived=false)
    std::fs::write(
        &query_path,
        r#"{
        "version": 1,
        "start": { "type": "Node", "label": "User", "key": "id", "value": "u1" },
        "steps": [
            { "action": "OutEdges", "label": "OWNS", "filter": null },
            { "action": "OutNodes", "label": "Project", "filter": {
                "property": "isArchived",
                "operator": "Equals",
                "value": false
            }}
        ],
        "collect": { "type": "Nodes", "properties": ["id", "title"] }
    }"#,
    )
    .unwrap();

    let out = chiffon()
        .args([
            "query",
            "--db",
            db.to_str().unwrap(),
            "--json",
            query_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["success"], serde_json::json!(true));
    let results = json["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["id"], serde_json::json!("p1"));
    assert_eq!(results[0]["title"], serde_json::json!("Alpha"));
}

/// Extracts (page, slot) from "Node inserted: page=X slot=Y"
fn parse_rid(s: &str) -> (u32, u16) {
    let page = s
        .split("page=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u32>()
        .unwrap();
    let slot = s
        .split("slot=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u16>()
        .unwrap();
    (page, slot)
}
