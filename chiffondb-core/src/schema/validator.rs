use std::collections::HashSet;

use crate::error::GraphError;
use crate::schema::ast::{Definition, SchemaAst, TypeExpr};

/// Validates the type consistency of a schema.
pub fn validate(ast: &SchemaAst) -> Result<(), GraphError> {
    let node_names: HashSet<&str> = ast
        .definitions
        .iter()
        .filter_map(|d| match d {
            Definition::Node(n) => Some(n.name.as_str()),
            _ => None,
        })
        .collect();

    for def in &ast.definitions {
        match def {
            Definition::Node(node) => {
                // Check that the extends target exists
                if let Some(base) = &node.extends {
                    if !node_names.contains(base.as_str()) {
                        return Err(GraphError::SchemaError(format!(
                            "node '{}' extends unknown type '{}'",
                            node.name, base
                        )));
                    }
                }
            }
            Definition::Edge(edge) => {
                // Check that the generic upper-bound type exists
                for param in &edge.generic_params {
                    if let Some(bound) = &param.bound {
                        if !node_names.contains(bound.as_str()) {
                            return Err(GraphError::SchemaError(format!(
                                "edge '{}' generic bound '{}' references unknown type",
                                edge.name, bound
                            )));
                        }
                    }
                }

                // Check that the node types specified in from/to exist in the schema
                // (type parameter names are excluded)
                let param_names: HashSet<&str> = edge
                    .generic_params
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect();

                validate_edge_endpoint(&edge.name, "from", &edge.from, &node_names, &param_names)?;
                validate_edge_endpoint(&edge.name, "to", &edge.to, &node_names, &param_names)?;
            }
        }
    }
    Ok(())
}

fn validate_edge_endpoint(
    edge_name: &str,
    side: &str,
    type_expr: &TypeExpr,
    node_names: &HashSet<&str>,
    param_names: &HashSet<&str>,
) -> Result<(), GraphError> {
    match type_expr {
        TypeExpr::Named(name) => {
            if !param_names.contains(name.as_str()) && !node_names.contains(name.as_str()) {
                return Err(GraphError::SchemaError(format!(
                    "edge '{}' {} references unknown node type '{}'",
                    edge_name, side, name
                )));
            }
            Ok(())
        }
        // Also validate NodeRef<T>
        TypeExpr::NodeRef(name) => {
            if !node_names.contains(name.as_str()) {
                return Err(GraphError::SchemaError(format!(
                    "edge '{}' {} NodeRef references unknown node type '{}'",
                    edge_name, side, name
                )));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::parser::parse;

    #[test]
    fn valid_schema_passes() {
        let src = r#"
            node User { id: String }
            node Project { id: String }
            edge OWNS<T: User> {
                from: T
                to: Project
                props: { role: String }
            }
        "#;
        let ast = parse(src).unwrap();
        assert!(validate(&ast).is_ok());
    }

    #[test]
    fn edge_from_unknown_node_fails() {
        let src = r#"
            node Project { id: String }
            edge OWNS {
                from: Ghost
                to: Project
            }
        "#;
        let ast = parse(src).unwrap();
        assert!(validate(&ast).is_err());
    }

    #[test]
    fn edge_to_unknown_node_fails() {
        let src = r#"
            node User { id: String }
            edge OWNS {
                from: User
                to: Ghost
            }
        "#;
        let ast = parse(src).unwrap();
        assert!(validate(&ast).is_err());
    }

    #[test]
    fn generic_bound_unknown_node_fails() {
        let src = r#"
            node Project { id: String }
            edge OWNS<T: Ghost> {
                from: T
                to: Project
            }
        "#;
        let ast = parse(src).unwrap();
        assert!(validate(&ast).is_err());
    }

    #[test]
    fn extends_unknown_node_fails() {
        let src = r#"
            node Admin extends Ghost { role: String }
        "#;
        let ast = parse(src).unwrap();
        assert!(validate(&ast).is_err());
    }

    #[test]
    fn generic_param_as_from_is_allowed() {
        let src = r#"
            node User { id: String }
            node Project { id: String }
            edge OWNS<T: User> {
                from: T
                to: Project
            }
        "#;
        let ast = parse(src).unwrap();
        assert!(validate(&ast).is_ok());
    }
}
