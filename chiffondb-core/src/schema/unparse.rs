use crate::schema::ast::{BoundParam, Definition, EdgeDef, FieldDef, NodeDef, SchemaAst, TypeExpr};

/// Converts a SchemaAst back into schema DSL source text.
/// This is the inverse of `parser::parse`: `parse(unparse(ast))` is guaranteed to be
/// semantically equivalent to the original AST (formatting and comments are not preserved).
pub fn unparse(ast: &SchemaAst) -> String {
    ast.definitions
        .iter()
        .map(unparse_definition)
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n"
}

fn unparse_definition(def: &Definition) -> String {
    match def {
        Definition::Node(n) => unparse_node_def(n),
        Definition::Edge(e) => unparse_edge_def(e),
    }
}

fn unparse_node_def(def: &NodeDef) -> String {
    let mut header = format!("node {}", def.name);
    if !def.type_params.is_empty() {
        header.push('<');
        header.push_str(&def.type_params.join(", "));
        header.push('>');
    }
    if let Some(extends) = &def.extends {
        header.push_str(" extends ");
        header.push_str(extends);
    }
    format!("{header} {{\n{}}}", unparse_fields(&def.fields, 1))
}

fn unparse_edge_def(def: &EdgeDef) -> String {
    let mut header = format!("edge {}", def.name);
    if !def.generic_params.is_empty() {
        header.push('<');
        header.push_str(
            &def.generic_params
                .iter()
                .map(unparse_bound_param)
                .collect::<Vec<_>>()
                .join(", "),
        );
        header.push('>');
    }

    let mut body = format!(
        "  from: {}\n  to: {}\n",
        unparse_type_expr(&def.from),
        unparse_type_expr(&def.to)
    );
    if !def.props.is_empty() {
        body.push_str("  props: {\n");
        body.push_str(&unparse_fields(&def.props, 2));
        body.push_str("  }\n");
    }

    format!("{header} {{\n{body}}}")
}

fn unparse_bound_param(param: &BoundParam) -> String {
    match &param.bound {
        Some(bound) => format!("{}: {}", param.name, bound),
        None => param.name.clone(),
    }
}

fn unparse_fields(fields: &[FieldDef], indent: usize) -> String {
    let pad = "  ".repeat(indent);
    fields
        .iter()
        .map(|f| format!("{pad}{}: {}\n", f.name, unparse_type_expr(&f.type_expr)))
        .collect()
}

fn unparse_type_expr(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Int => "Int".to_string(),
        TypeExpr::Float => "Float".to_string(),
        TypeExpr::Boolean => "Boolean".to_string(),
        TypeExpr::DateTime => "DateTime".to_string(),
        TypeExpr::String => "String".to_string(),
        TypeExpr::Json => "Json".to_string(),
        TypeExpr::Blob => "Blob".to_string(),
        TypeExpr::Vector(n) => format!("Vector<{n}>"),
        TypeExpr::List(inner) => format!("List<{}>", unparse_type_expr(inner)),
        TypeExpr::Map(k, v) => format!("Map<{}, {}>", unparse_type_expr(k), unparse_type_expr(v)),
        TypeExpr::NodeRef(name) => format!("NodeRef<{name}>"),
        TypeExpr::EdgeRef(name) => format!("EdgeRef<{name}>"),
        TypeExpr::Named(name) => name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::parser::parse;

    fn roundtrip(src: &str) {
        let ast = parse(src).expect("parse should succeed");
        let unparsed = unparse(&ast);
        let reparsed = parse(&unparsed).unwrap_or_else(|e| {
            panic!("unparsed text should reparse: {e}\n--- unparsed ---\n{unparsed}")
        });
        assert_eq!(
            ast, reparsed,
            "roundtrip mismatch\n--- unparsed ---\n{unparsed}"
        );
    }

    #[test]
    fn unparse_simple_node_and_edge() {
        roundtrip(
            r#"
            node Person {
              id: String
              name: String
            }

            node Company {
              id: String
              name: String
            }

            edge WORKS_AT {
              from: Person
              to: Company
              props: {
                role: String
              }
            }
            "#,
        );
    }

    #[test]
    fn unparse_node_without_props_block() {
        roundtrip(
            r#"
            node User { id: String name: String }
            node Project { id: String title: String isArchived: Boolean }
            edge OWNS { from: User to: Project }
            "#,
        );
    }

    #[test]
    fn unparse_extends_and_type_params() {
        roundtrip(
            r#"
            node Base {
              id: String
            }

            node Derived<T> extends Base {
              value: T
            }
            "#,
        );
    }

    #[test]
    fn unparse_generic_edge_with_bound_param() {
        roundtrip(
            r#"
            node Item {
              id: String
            }

            edge LINKS<T: Item> {
              from: Item
              to: Item
              props: {
                weight: Float
              }
            }
            "#,
        );
    }

    #[test]
    fn unparse_collection_and_ref_types() {
        roundtrip(
            r#"
            node Doc {
              tags: List<String>
              meta: Map<String, Json>
              embedding: Vector<128>
              author: NodeRef<Person>
              wrote: EdgeRef<WORKS_AT>
            }

            node Person {
              id: String
            }

            edge WORKS_AT {
              from: Person
              to: Doc
            }
            "#,
        );
    }
}
