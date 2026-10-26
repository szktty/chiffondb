//! End-to-end check of the UniFFI surface, exercised as the Kotlin client will:
//! open a database, write a node, read it back.

use chiffondb_uniffi::Connection;

const SCHEMA: &str = r#"
node Memo {
  title: String
}
"#;

#[test]
fn insert_then_read_back() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::open_in_memory()?;
    conn.apply_schema(SCHEMA.to_string())?;

    let rid = conn.insert_node("Memo".to_string(), r#"{"title":"hello"}"#.to_string())?;

    let props = conn.get_node_properties(rid)?;
    assert!(props.contains("hello"), "unexpected properties: {props}");
    assert_eq!(conn.count_nodes(None)?, 1);

    let listed = conn.list_nodes(None)?;
    assert!(listed.contains("hello"), "unexpected listing: {listed}");

    conn.shutdown()?;
    Ok(())
}

#[test]
fn errors_cross_the_boundary_as_results() {
    // A missing file must surface as an error rather than a panic; the Kotlin
    // side sees this as a thrown exception.
    let err = Connection::open("/nonexistent/path/to.chiffon".to_string());
    assert!(err.is_err());
}

const GRAPH_SCHEMA: &str = r#"
node Memo {
  title: String
}

edge REFERS {
  from: Memo
  to: Memo
  props: {
    note: String
  }
}
"#;

#[test]
fn edges_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::open_in_memory()?;
    conn.apply_schema(GRAPH_SCHEMA.to_string())?;

    let a = conn.insert_node("Memo".to_string(), r#"{"title":"a"}"#.to_string())?;
    let b = conn.insert_node("Memo".to_string(), r#"{"title":"b"}"#.to_string())?;

    let edge = conn.insert_edge(
        "REFERS".to_string(),
        a,
        b,
        r#"{"note":"see also"}"#.to_string(),
    )?;

    let listed = conn.list_edges(None)?;
    assert!(listed.contains("see also"), "unexpected listing: {listed}");

    // Endpoints must survive the round trip; the client rebuilds the graph from them.
    // The keys are "from"/"to", unlike list_edges which uses "src_rid"/"dst_rid".
    let endpoints = conn.get_edge_endpoints(edge)?;
    assert!(
        endpoints.contains("from"),
        "unexpected endpoints: {endpoints}"
    );
    assert!(
        endpoints.contains("to"),
        "unexpected endpoints: {endpoints}"
    );

    assert!(conn.edge_exists(edge)?);
    conn.delete_edge(edge)?;
    assert!(!conn.edge_exists(edge)?);

    Ok(())
}

#[test]
fn nodes_can_be_updated_and_deleted() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::open_in_memory()?;
    conn.apply_schema(GRAPH_SCHEMA.to_string())?;

    let rid = conn.insert_node("Memo".to_string(), r#"{"title":"before"}"#.to_string())?;
    conn.update_node_properties(rid, r#"{"title":"after"}"#.to_string())?;
    assert!(conn.get_node_properties(rid)?.contains("after"));

    assert!(conn.node_exists(rid)?);
    conn.delete_node(rid)?;
    assert!(!conn.node_exists(rid)?);

    Ok(())
}

#[test]
fn transactions_roll_back() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::open_in_memory()?;
    conn.apply_schema(GRAPH_SCHEMA.to_string())?;

    conn.begin_transaction()?;
    conn.insert_node("Memo".to_string(), r#"{"title":"discarded"}"#.to_string())?;
    conn.rollback_transaction()?;

    assert_eq!(conn.count_nodes(None)?, 0);
    Ok(())
}

#[test]
fn field_rename_and_type_change() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::open_in_memory()?;
    conn.apply_schema("node 人物 {\n  _id: String\n  名前: String\n  年齢: String\n}".to_string())?;
    let rid = conn.insert_node(
        "人物".to_string(),
        r#"{"_id":"1","名前":"山田","年齢":"30"}"#.to_string(),
    )?;

    let outcome = conn.change_node_field_type(
        "人物".to_string(),
        "年齢".to_string(),
        "Int".to_string(),
        false,
    )?;
    assert!(outcome.contains(r#""applied":true"#), "{outcome}");
    conn.rename_node_field("人物".to_string(), "名前".to_string(), "氏名".to_string())?;

    let props = conn.get_node_properties(rid)?;
    assert!(
        props.contains(r#""年齢":30"#) && props.contains(r#""氏名":"山田""#),
        "{props}"
    );
    conn.patch_node_properties(rid, r#"{"氏名":"田中"}"#.to_string())?;
    conn.apply_schema(conn.get_schema_text()?)?;
    Ok(())
}

#[test]
fn dynamic_labels_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::open_in_memory()?;
    conn.apply_schema("node Node {\n  title: String\n}\n".to_string())?;

    let a = conn.insert_node("Node".to_string(), r#"{"title":"a"}"#.to_string())?;
    conn.add_node_label_dynamic(a.clone(), "Idea".to_string())?;
    let labels = conn.get_node_labels(a.clone())?;
    assert!(labels.contains("Node") && labels.contains("Idea"), "{labels}");
    assert_eq!(conn.count_nodes(Some("Idea".to_string()))?, 1);

    conn.remove_node_label(a, "Idea".to_string())?;
    assert_eq!(conn.count_nodes(Some("Idea".to_string()))?, 0);

    let b = conn.insert_node_with_dynamic_labels(
        "Node".to_string(),
        vec!["Place".to_string()],
        r#"{"title":"b"}"#.to_string(),
    )?;
    assert!(conn.get_node_labels(b)?.contains("Place"));

    conn.shutdown()?;
    Ok(())
}
