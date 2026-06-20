/// Schema migration policy validation (spec Section 5.6).
///
/// Allowed changes:
///   - Adding node/edge types
///   - Adding fields
///   - Removing fields (existing data is retained but ignored on read)
///
/// Prohibited changes:
///   - Changing a field's type
///   - Renaming a type (removing the old type and adding a new one is allowed, but renaming
///     a type while keeping the same type_id is not)
///   - Changing an edge's from/to types
use std::collections::HashMap;

use crate::error::GraphError;
use crate::schema::ast::{Definition, SchemaAst, TypeExpr};

/// Validates whether the migration from the existing schema (`old`) to the new schema (`new`) is allowed.
/// Returns `GraphError::SchemaError` if a prohibited change is detected.
pub fn validate_migration(old: &SchemaAst, new: &SchemaAst) -> Result<(), GraphError> {
    let old_nodes = node_map(old);
    let old_edges = edge_map(old);
    let new_nodes = node_map(new);
    let new_edges = edge_map(new);

    // Detect field type changes in existing node types
    for (name, new_def) in &new_nodes {
        if let Some(old_def) = old_nodes.get(name) {
            let old_fields: HashMap<&str, &TypeExpr> =
                old_def.iter().map(|(k, v)| (k.as_str(), *v)).collect();
            for (field_name, new_type) in new_def {
                if let Some(old_type) = old_fields.get(field_name.as_str()) {
                    if !type_compatible(old_type, new_type) {
                        return Err(GraphError::SchemaError(format!(
                            "node '{}' field '{}': type change is not allowed (was {}, now {})",
                            name,
                            field_name,
                            type_name(old_type),
                            type_name(new_type)
                        )));
                    }
                }
            }
        }
    }

    // Detect from/to changes in existing edge types
    for (name, (new_from, new_to, new_props)) in &new_edges {
        if let Some((old_from, old_to, old_props)) = old_edges.get(name) {
            if !type_compatible(old_from, new_from) {
                return Err(GraphError::SchemaError(format!(
                    "edge '{}': changing 'from' type is not allowed",
                    name
                )));
            }
            if !type_compatible(old_to, new_to) {
                return Err(GraphError::SchemaError(format!(
                    "edge '{}': changing 'to' type is not allowed",
                    name
                )));
            }
            // Detect field type changes in edge properties
            let old_pmap: HashMap<&str, &TypeExpr> =
                old_props.iter().map(|(k, v)| (k.as_str(), *v)).collect();
            for (field_name, new_type) in new_props {
                if let Some(old_type) = old_pmap.get(field_name.as_str()) {
                    if !type_compatible(old_type, new_type) {
                        return Err(GraphError::SchemaError(format!(
                            "edge '{}' prop '{}': type change is not allowed (was {}, now {})",
                            name,
                            field_name,
                            type_name(old_type),
                            type_name(new_type)
                        )));
                    }
                }
            }
        }
    }

    Ok(())
}

type NodeFieldMap<'a> = HashMap<&'a str, Vec<(String, &'a TypeExpr)>>;
type EdgeEntryMap<'a> = HashMap<&'a str, (&'a TypeExpr, &'a TypeExpr, Vec<(String, &'a TypeExpr)>)>;

/// Builds a map of node name → field name → TypeExpr.
fn node_map(ast: &SchemaAst) -> NodeFieldMap<'_> {
    let mut map = HashMap::new();
    for def in &ast.definitions {
        if let Definition::Node(n) = def {
            let fields = n
                .fields
                .iter()
                .map(|f| (f.name.clone(), &f.type_expr))
                .collect();
            map.insert(n.name.as_str(), fields);
        }
    }
    map
}

/// Builds a map of edge name → (from TypeExpr, to TypeExpr, props).
fn edge_map(ast: &SchemaAst) -> EdgeEntryMap<'_> {
    let mut map = HashMap::new();
    for def in &ast.definitions {
        if let Definition::Edge(e) = def {
            let props = e
                .props
                .iter()
                .map(|f| (f.name.clone(), &f.type_expr))
                .collect();
            map.insert(e.name.as_str(), (&e.from, &e.to, props));
        }
    }
    map
}

/// Returns true if the types are compatible (same type is compatible; any change is not).
fn type_compatible(old: &TypeExpr, new: &TypeExpr) -> bool {
    std::mem::discriminant(old) == std::mem::discriminant(new)
        && match (old, new) {
            (TypeExpr::Vector(a), TypeExpr::Vector(b)) => a == b,
            (TypeExpr::List(a), TypeExpr::List(b)) => type_compatible(a, b),
            (TypeExpr::Map(ak, av), TypeExpr::Map(bk, bv)) => {
                type_compatible(ak, bk) && type_compatible(av, bv)
            }
            (TypeExpr::Named(a), TypeExpr::Named(b)) => a == b,
            (TypeExpr::NodeRef(a), TypeExpr::NodeRef(b)) => a == b,
            (TypeExpr::EdgeRef(a), TypeExpr::EdgeRef(b)) => a == b,
            _ => true, // Primitive types are compatible when their discriminants match
        }
}

fn type_name(t: &TypeExpr) -> &'static str {
    match t {
        TypeExpr::Int => "Int",
        TypeExpr::Float => "Float",
        TypeExpr::Boolean => "Boolean",
        TypeExpr::DateTime => "DateTime",
        TypeExpr::String => "String",
        TypeExpr::Json => "Json",
        TypeExpr::Blob => "Blob",
        TypeExpr::Vector(_) => "Vector",
        TypeExpr::List(_) => "List",
        TypeExpr::Map(_, _) => "Map",
        TypeExpr::NodeRef(_) => "NodeRef",
        TypeExpr::EdgeRef(_) => "EdgeRef",
        TypeExpr::Named(_) => "Named",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::parser::parse;

    fn schema(src: &str) -> SchemaAst {
        parse(src).unwrap()
    }

    #[test]
    fn add_field_is_allowed() {
        let old = schema("node User { id: String }");
        let new = schema("node User { id: String  name: String }");
        assert!(validate_migration(&old, &new).is_ok());
    }

    #[test]
    fn remove_field_is_allowed() {
        let old = schema("node User { id: String  name: String }");
        let new = schema("node User { id: String }");
        assert!(validate_migration(&old, &new).is_ok());
    }

    #[test]
    fn add_node_type_is_allowed() {
        let old = schema("node User { id: String }");
        let new = schema("node User { id: String }  node Admin { role: String }");
        assert!(validate_migration(&old, &new).is_ok());
    }

    #[test]
    fn field_type_change_is_rejected() {
        let old = schema("node User { id: String }");
        let new = schema("node User { id: Int }");
        let err = validate_migration(&old, &new).unwrap_err();
        assert!(err.to_string().contains("type change"));
    }

    #[test]
    fn edge_from_change_is_rejected() {
        let old = schema(
            "node User { id: String }  node Admin { id: String }  node Project { id: String }
             edge OWNS { from: User  to: Project }",
        );
        let new = schema(
            "node User { id: String }  node Admin { id: String }  node Project { id: String }
             edge OWNS { from: Admin  to: Project }",
        );
        let err = validate_migration(&old, &new).unwrap_err();
        assert!(err.to_string().contains("'from'"));
    }

    #[test]
    fn edge_to_change_is_rejected() {
        let old = schema(
            "node User { id: String }  node Project { id: String }  node Doc { id: String }
             edge OWNS { from: User  to: Project }",
        );
        let new = schema(
            "node User { id: String }  node Project { id: String }  node Doc { id: String }
             edge OWNS { from: User  to: Doc }",
        );
        let err = validate_migration(&old, &new).unwrap_err();
        assert!(err.to_string().contains("'to'"));
    }

    #[test]
    fn edge_prop_type_change_is_rejected() {
        let old = schema(
            "node User { id: String }  node Project { id: String }
             edge OWNS { from: User  to: Project  props: { role: String } }",
        );
        let new = schema(
            "node User { id: String }  node Project { id: String }
             edge OWNS { from: User  to: Project  props: { role: Int } }",
        );
        let err = validate_migration(&old, &new).unwrap_err();
        assert!(err.to_string().contains("type change"));
    }

    #[test]
    fn completely_new_schema_is_allowed() {
        // When no schema has been applied yet, old is empty so migration is always allowed
        let old = SchemaAst {
            definitions: vec![],
        };
        let new = schema("node User { id: String }");
        assert!(validate_migration(&old, &new).is_ok());
    }
}
