use std::collections::HashMap;

use serde_json::Value;

use crate::error::GraphError;

#[derive(Debug, Clone, PartialEq)]
pub enum CypherResult {
    Rows(Vec<HashMap<String, Value>>),
    Created { nodes: usize, edges: usize },
    Updated(usize),
    Deleted(usize),
    Empty,
}

impl CypherResult {
    pub fn to_json_string(&self) -> Result<String, GraphError> {
        let v = match self {
            CypherResult::Rows(rows) => serde_json::to_value(rows),
            CypherResult::Created { nodes, edges } => serde_json::to_value(
                serde_json::json!({"created": {"nodes": nodes, "edges": edges}}),
            ),
            CypherResult::Updated(n) => serde_json::to_value(serde_json::json!({"updated": n})),
            CypherResult::Deleted(n) => serde_json::to_value(serde_json::json!({"deleted": n})),
            CypherResult::Empty => serde_json::to_value(serde_json::json!({})),
        };
        v.and_then(|v| serde_json::to_string(&v))
            .map_err(|e| GraphError::InvalidCommand(e.to_string()))
    }
}
