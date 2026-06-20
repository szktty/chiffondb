use std::collections::HashMap;

use serde_json::Value;

use crate::error::GraphError;
use crate::schema::ast::{FieldDef, TypeExpr};

/// Validates a property map against a set of field definitions.
/// Rejects undefined fields and values whose JSON type does not match the schema,
/// so that data which contradicts the schema can never be stored.
pub fn validate_properties(
    type_label: &str,
    fields: &[FieldDef],
    properties: &HashMap<String, Value>,
) -> Result<(), GraphError> {
    for key in properties.keys() {
        if !fields.iter().any(|f| &f.name == key) {
            return Err(GraphError::ValidationError(format!(
                "{type_label}: unknown field '{key}'"
            )));
        }
    }

    for field in fields {
        if let Some(value) = properties.get(&field.name) {
            if !value_matches_type(value, &field.type_expr) {
                return Err(GraphError::ValidationError(format!(
                    "{type_label}: field '{}' expected {}, got {}",
                    field.name,
                    describe_type(&field.type_expr),
                    describe_value(value),
                )));
            }
        }
    }

    Ok(())
}

/// Returns whether a JSON value is acceptable for the given schema type.
/// `null` is accepted for any type (treated as an absent optional value).
fn value_matches_type(value: &Value, ty: &TypeExpr) -> bool {
    match (ty, value) {
        (_, Value::Null) => true,
        (TypeExpr::Json, _) => true,
        (TypeExpr::Int, Value::Number(n)) => n.is_i64() || n.is_u64(),
        (TypeExpr::Float, Value::Number(_)) => true,
        (TypeExpr::Boolean, Value::Bool(_)) => true,
        (TypeExpr::String | TypeExpr::DateTime, Value::String(_)) => true,
        // Blobs are carried as base64 strings or raw byte arrays.
        (TypeExpr::Blob, Value::String(_) | Value::Array(_)) => true,
        (TypeExpr::Vector(_), Value::Array(items)) => {
            items.iter().all(|v| matches!(v, Value::Number(_)))
        }
        (TypeExpr::List(inner), Value::Array(items)) => {
            items.iter().all(|v| value_matches_type(v, inner))
        }
        (TypeExpr::Map(_, val_ty), Value::Object(map)) => {
            map.values().all(|v| value_matches_type(v, val_ty))
        }
        // References are stored as the target node/edge id (a string).
        (TypeExpr::NodeRef(_) | TypeExpr::EdgeRef(_) | TypeExpr::Named(_), Value::String(_)) => {
            true
        }
        _ => false,
    }
}

fn describe_type(ty: &TypeExpr) -> String {
    match ty {
        TypeExpr::Int => "Int".to_string(),
        TypeExpr::Float => "Float".to_string(),
        TypeExpr::Boolean => "Boolean".to_string(),
        TypeExpr::DateTime => "DateTime".to_string(),
        TypeExpr::String => "String".to_string(),
        TypeExpr::Json => "Json".to_string(),
        TypeExpr::Blob => "Blob".to_string(),
        TypeExpr::Vector(n) => format!("Vector({n})"),
        TypeExpr::List(inner) => format!("List<{}>", describe_type(inner)),
        TypeExpr::Map(k, v) => format!("Map<{}, {}>", describe_type(k), describe_type(v)),
        TypeExpr::NodeRef(s) => format!("NodeRef<{s}>"),
        TypeExpr::EdgeRef(s) => format!("EdgeRef<{s}>"),
        TypeExpr::Named(s) => s.clone(),
    }
}

fn describe_value(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields() -> Vec<FieldDef> {
        vec![
            FieldDef {
                name: "id".to_string(),
                type_expr: TypeExpr::String,
            },
            FieldDef {
                name: "age".to_string(),
                type_expr: TypeExpr::Int,
            },
            FieldDef {
                name: "active".to_string(),
                type_expr: TypeExpr::Boolean,
            },
        ]
    }

    #[test]
    fn accepts_matching_properties() {
        let props = HashMap::from([
            ("id".to_string(), json!("u1")),
            ("age".to_string(), json!(30)),
            ("active".to_string(), json!(true)),
        ]);
        assert!(validate_properties("User", &fields(), &props).is_ok());
    }

    #[test]
    fn rejects_unknown_field() {
        let props = HashMap::from([("ghost".to_string(), json!("x"))]);
        let err = validate_properties("User", &fields(), &props).unwrap_err();
        assert!(matches!(err, GraphError::ValidationError(_)));
    }

    #[test]
    fn rejects_type_mismatch() {
        let props = HashMap::from([("age".to_string(), json!("not a number"))]);
        let err = validate_properties("User", &fields(), &props).unwrap_err();
        assert!(matches!(err, GraphError::ValidationError(_)));
    }

    #[test]
    fn allows_missing_optional_and_null() {
        let props = HashMap::from([("age".to_string(), json!(null))]);
        assert!(validate_properties("User", &fields(), &props).is_ok());
        // A subset of fields is allowed (others are simply absent).
        let props = HashMap::from([("id".to_string(), json!("u1"))]);
        assert!(validate_properties("User", &fields(), &props).is_ok());
    }
}
