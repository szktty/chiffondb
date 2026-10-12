use pest::iterators::Pair;
use pest::Parser;
use pest_derive::Parser;

use crate::error::GraphError;
use crate::schema::ast::*;

#[derive(Parser)]
#[grammar = "schema/schema.pest"]
struct SchemaParser;

pub fn parse(input: &str) -> Result<SchemaAst, GraphError> {
    let pairs = SchemaParser::parse(Rule::schema, input)
        .map_err(|e| GraphError::SchemaError(e.to_string()))?;

    let mut definitions = Vec::new();
    for pair in pairs {
        if pair.as_rule() == Rule::schema {
            for inner in pair.into_inner() {
                match inner.as_rule() {
                    Rule::definition => {
                        let def = parse_definition(inner)?;
                        definitions.push(def);
                    }
                    Rule::EOI => {}
                    _ => {}
                }
            }
        }
    }
    Ok(SchemaAst { definitions })
}

/// Returns true when `name` can be written in schema source without backtick quoting.
pub(crate) fn is_bare_name(name: &str) -> bool {
    SchemaParser::parse(Rule::bare_name, name).is_ok()
}

/// Resolves an `ident` (or a rule wrapping one, such as `type_param`) to the name it denotes,
/// stripping backtick quotes and undoubling escaped backticks.
fn ident_name(pair: Pair<Rule>) -> Result<String, GraphError> {
    match pair.as_rule() {
        Rule::plain_ident => Ok(pair.as_str().to_string()),
        Rule::quoted_body => Ok(pair.as_str().replace("``", "`")),
        _ => {
            let rule = pair.as_rule();
            let inner = pair.into_inner().next().ok_or_else(|| {
                GraphError::SchemaError(format!("expected an identifier, found {rule:?}"))
            })?;
            ident_name(inner)
        }
    }
}

fn next_ident_name<'a>(
    inner: &mut impl Iterator<Item = Pair<'a, Rule>>,
) -> Result<String, GraphError> {
    let pair = inner
        .next()
        .ok_or_else(|| GraphError::SchemaError("expected an identifier".to_string()))?;
    ident_name(pair)
}

fn parse_definition(pair: Pair<Rule>) -> Result<Definition, GraphError> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::node_def => Ok(Definition::Node(parse_node_def(inner)?)),
        Rule::edge_def => Ok(Definition::Edge(parse_edge_def(inner)?)),
        _ => Err(GraphError::SchemaError(format!(
            "unexpected rule: {:?}",
            inner.as_rule()
        ))),
    }
}

fn parse_node_def(pair: Pair<Rule>) -> Result<NodeDef, GraphError> {
    let mut inner = pair.into_inner();
    let name = next_ident_name(&mut inner)?;

    let mut type_params = Vec::new();
    let mut extends = None;
    let mut fields = Vec::new();

    for part in inner {
        match part.as_rule() {
            Rule::type_params => {
                for p in part.into_inner() {
                    if p.as_rule() == Rule::type_param {
                        type_params.push(ident_name(p)?);
                    }
                }
            }
            Rule::extends_clause => {
                extends = Some(next_ident_name(&mut part.into_inner())?);
            }
            Rule::field_def => {
                fields.push(parse_field_def(part)?);
            }
            _ => {}
        }
    }

    Ok(NodeDef {
        name,
        type_params,
        extends,
        fields,
    })
}

fn parse_edge_def(pair: Pair<Rule>) -> Result<EdgeDef, GraphError> {
    let mut inner = pair.into_inner();
    let name = next_ident_name(&mut inner)?;

    let mut generic_params = Vec::new();
    let mut from = None;
    let mut to = None;
    let mut props = Vec::new();

    for part in inner {
        match part.as_rule() {
            Rule::generic_params => {
                for p in part.into_inner() {
                    if p.as_rule() == Rule::bound_param {
                        generic_params.push(parse_bound_param(p)?);
                    }
                }
            }
            Rule::edge_from => {
                from = Some(parse_type_expr(part.into_inner().next().unwrap())?);
            }
            Rule::edge_to => {
                to = Some(parse_type_expr(part.into_inner().next().unwrap())?);
            }
            Rule::props_block => {
                for f in part.into_inner() {
                    if f.as_rule() == Rule::field_def {
                        props.push(parse_field_def(f)?);
                    }
                }
            }
            _ => {}
        }
    }

    Ok(EdgeDef {
        name,
        generic_params,
        from: from.ok_or_else(|| GraphError::SchemaError("missing from".to_string()))?,
        to: to.ok_or_else(|| GraphError::SchemaError("missing to".to_string()))?,
        props,
    })
}

fn parse_bound_param(pair: Pair<Rule>) -> Result<BoundParam, GraphError> {
    let mut inner = pair.into_inner();
    let name = next_ident_name(&mut inner)?;
    let bound = inner.next().map(ident_name).transpose()?;
    Ok(BoundParam { name, bound })
}

fn parse_field_def(pair: Pair<Rule>) -> Result<FieldDef, GraphError> {
    let mut inner = pair.into_inner();
    let name = next_ident_name(&mut inner)?;
    let type_expr = parse_type_expr(inner.next().unwrap())?;
    // Remaining pairs are annotations (`@index`, `@unique`). Each `annotation` rule wraps an
    // `ident` naming the annotation.
    let mut indexed = false;
    let mut unique = false;
    for ann in inner {
        if ann.as_rule() != Rule::annotation {
            continue;
        }
        let ident = ann.into_inner().next().unwrap().as_str();
        match ident {
            "index" => indexed = true,
            "unique" => unique = true,
            other => {
                return Err(GraphError::SchemaError(format!(
                    "unknown field annotation '@{other}' on '{name}'"
                )))
            }
        }
    }
    Ok(FieldDef {
        name,
        type_expr,
        indexed,
        unique,
    })
}

fn parse_type_expr(pair: Pair<Rule>) -> Result<TypeExpr, GraphError> {
    let inner = pair.into_inner().next().unwrap();
    match inner.as_rule() {
        Rule::primitive_type => Ok(match inner.as_str() {
            "Int" => TypeExpr::Int,
            "Float" => TypeExpr::Float,
            "Boolean" => TypeExpr::Boolean,
            "DateTime" => TypeExpr::DateTime,
            "String" => TypeExpr::String,
            "Json" => TypeExpr::Json,
            "Blob" => TypeExpr::Blob,
            other => {
                return Err(GraphError::SchemaError(format!(
                    "unknown primitive: {other}"
                )))
            }
        }),
        Rule::vector_type => {
            let n: u32 = inner.into_inner().next().unwrap().as_str().parse().unwrap();
            Ok(TypeExpr::Vector(n))
        }
        Rule::list_type => {
            let elem = parse_type_expr(inner.into_inner().next().unwrap())?;
            Ok(TypeExpr::List(Box::new(elem)))
        }
        Rule::map_type => {
            let mut parts = inner.into_inner();
            let k = parse_type_expr(parts.next().unwrap())?;
            let v = parse_type_expr(parts.next().unwrap())?;
            Ok(TypeExpr::Map(Box::new(k), Box::new(v)))
        }
        Rule::node_ref_type => {
            let name = next_ident_name(&mut inner.into_inner())?;
            Ok(TypeExpr::NodeRef(name))
        }
        Rule::edge_ref_type => {
            let name = next_ident_name(&mut inner.into_inner())?;
            Ok(TypeExpr::EdgeRef(name))
        }
        Rule::ident => Ok(TypeExpr::Named(ident_name(inner)?)),
        other => Err(GraphError::SchemaError(format!(
            "unexpected type rule: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Normal cases ----

    #[test]
    fn parse_simple_node() {
        let src = r#"
            node User {
                id: String
                age: Int
                active: Boolean
            }
        "#;
        let ast = parse(src).unwrap();
        assert_eq!(ast.definitions.len(), 1);
        let Definition::Node(node) = &ast.definitions[0] else {
            panic!()
        };
        assert_eq!(node.name, "User");
        assert_eq!(node.fields.len(), 3);
        assert_eq!(node.fields[0].name, "id");
        assert_eq!(node.fields[0].type_expr, TypeExpr::String);
    }

    #[test]
    fn parse_field_annotations() {
        let src = r#"
            node User {
                email: String @index
                handle: String @unique
                name: String
            }
        "#;
        let ast = parse(src).unwrap();
        let Definition::Node(node) = &ast.definitions[0] else {
            panic!()
        };
        assert!(node.fields[0].indexed && !node.fields[0].unique);
        assert!(node.fields[1].unique && !node.fields[1].indexed);
        assert!(!node.fields[2].indexed && !node.fields[2].unique);
    }

    #[test]
    fn parse_rejects_unknown_annotation() {
        let src = "node User { id: String @bogus }";
        assert!(parse(src).is_err());
    }

    #[test]
    fn parse_node_with_extends() {
        let src = r#"
            node AdminUser extends User {
                role: String
            }
        "#;
        let ast = parse(src).unwrap();
        let Definition::Node(node) = &ast.definitions[0] else {
            panic!()
        };
        assert_eq!(node.extends, Some("User".to_string()));
    }

    #[test]
    fn parse_node_with_type_params() {
        let src = r#"
            node Pair<K, V> {
                key: K
                value: V
            }
        "#;
        let ast = parse(src).unwrap();
        let Definition::Node(node) = &ast.definitions[0] else {
            panic!()
        };
        assert_eq!(node.type_params, vec!["K", "V"]);
        assert_eq!(node.fields[0].type_expr, TypeExpr::Named("K".to_string()));
    }

    #[test]
    fn parse_edge_without_generics() {
        let src = r#"
            edge AUTHORED {
                from: User
                to: Document
                props: {
                    score: Float
                }
            }
        "#;
        let ast = parse(src).unwrap();
        let Definition::Edge(edge) = &ast.definitions[0] else {
            panic!()
        };
        assert_eq!(edge.name, "AUTHORED");
        assert_eq!(edge.from, TypeExpr::Named("User".to_string()));
        assert_eq!(edge.to, TypeExpr::Named("Document".to_string()));
        assert_eq!(edge.props.len(), 1);
        assert_eq!(edge.props[0].type_expr, TypeExpr::Float);
    }

    #[test]
    fn parse_edge_with_bound_generics() {
        let src = r#"
            edge OWNS<T: User> {
                from: T
                to: Project
                props: {
                    role: String
                    grantedAt: DateTime
                }
            }
        "#;
        let ast = parse(src).unwrap();
        let Definition::Edge(edge) = &ast.definitions[0] else {
            panic!()
        };
        assert_eq!(edge.generic_params.len(), 1);
        assert_eq!(edge.generic_params[0].name, "T");
        assert_eq!(edge.generic_params[0].bound, Some("User".to_string()));
        assert_eq!(edge.from, TypeExpr::Named("T".to_string()));
    }

    #[test]
    fn parse_vector_type() {
        let src = r#"
            node Doc {
                embedding: Vector<384>
            }
        "#;
        let ast = parse(src).unwrap();
        let Definition::Node(node) = &ast.definitions[0] else {
            panic!()
        };
        assert_eq!(node.fields[0].type_expr, TypeExpr::Vector(384));
    }

    #[test]
    fn parse_collection_types() {
        let src = r#"
            node Tag {
                labels: List<String>
                meta: Map<String, Int>
            }
        "#;
        let ast = parse(src).unwrap();
        let Definition::Node(node) = &ast.definitions[0] else {
            panic!()
        };
        assert_eq!(
            node.fields[0].type_expr,
            TypeExpr::List(Box::new(TypeExpr::String))
        );
        assert_eq!(
            node.fields[1].type_expr,
            TypeExpr::Map(Box::new(TypeExpr::String), Box::new(TypeExpr::Int))
        );
    }

    #[test]
    fn parse_full_sample_schema() {
        let src = r#"
            node User {
                id: String
                name: String
                isActive: Boolean
                createdAt: DateTime
            }
            node Project {
                id: String
                title: String
                isArchived: Boolean
            }
            node Document {
                id: String
                content: String
                embedding: Vector<384>
            }
            edge OWNS<T: User> {
                from: T
                to: Project
                props: {
                    role: String
                    grantedAt: DateTime
                }
            }
            edge AUTHORED {
                from: User
                to: Document
                props: {
                    contributionScore: Float
                }
            }
        "#;
        let ast = parse(src).unwrap();
        assert_eq!(ast.definitions.len(), 5);
    }

    // ---- Error cases ----

    #[test]
    fn parse_error_missing_closing_brace() {
        let src = "node User { id: String";
        assert!(parse(src).is_err());
    }

    #[test]
    fn parse_error_invalid_token() {
        let src = "node 123Invalid { }";
        assert!(parse(src).is_err());
    }

    #[test]
    fn parse_error_empty_generic() {
        // Generic parameter list is empty
        let src = "edge FOO<> { from: A to: B }";
        assert!(parse(src).is_err());
    }

    // ---- Identifiers ----

    fn node_named(src: &str) -> NodeDef {
        match parse(src).unwrap().definitions.into_iter().next() {
            Some(Definition::Node(n)) => n,
            other => panic!("expected a node definition, got {other:?}"),
        }
    }

    #[test]
    fn parse_unicode_identifiers() {
        let n = node_named("node 人物 { 名前: String よみ: String カナ_2: Int }");
        assert_eq!(n.name, "人物");
        let fields: Vec<_> = n.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(fields, ["名前", "よみ", "カナ_2"]);
    }

    #[test]
    fn parse_unicode_identifiers_everywhere_a_name_is_allowed() {
        let src = r#"
            node 基底 { id: String }
            node 人物<型> extends 基底 { 値: 型 上司: NodeRef<人物> 所属: EdgeRef<勤務> }
            edge 勤務<相手: 基底> { from: 人物 to: 相手 props: { 役職: String @index } }
        "#;
        let ast = parse(src).unwrap();
        let Definition::Node(person) = &ast.definitions[1] else {
            panic!("expected node");
        };
        assert_eq!(person.type_params, ["型"]);
        assert_eq!(person.extends.as_deref(), Some("基底"));
        assert_eq!(person.fields[0].type_expr, TypeExpr::Named("型".into()));
        assert_eq!(person.fields[1].type_expr, TypeExpr::NodeRef("人物".into()));
        assert_eq!(person.fields[2].type_expr, TypeExpr::EdgeRef("勤務".into()));
        let Definition::Edge(works) = &ast.definitions[2] else {
            panic!("expected edge");
        };
        assert_eq!(works.name, "勤務");
        assert_eq!(works.generic_params[0].name, "相手");
        assert_eq!(works.generic_params[0].bound.as_deref(), Some("基底"));
        assert_eq!(works.from, TypeExpr::Named("人物".into()));
        assert!(works.props[0].indexed);
    }

    #[test]
    fn parse_quoted_identifiers() {
        let n = node_named(
            "node `My Type` { `first name`: String `売上（税込）`: Int `a-b`: Int `1abc`: Int `a.b`: Int }",
        );
        assert_eq!(n.name, "My Type");
        let fields: Vec<_> = n.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(fields, ["first name", "売上（税込）", "a-b", "1abc", "a.b"]);
    }

    #[test]
    fn parse_quoted_identifier_escapes_backtick_by_doubling() {
        let n = node_named("node T { `a``b`: Int ```x```: Int }");
        let fields: Vec<_> = n.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(fields, ["a`b", "`x`"]);
    }

    #[test]
    fn parse_quoted_identifier_keeps_inner_whitespace_and_comment_markers() {
        let n = node_named("node T { ` a  b `: Int `x // y`: Int }");
        let fields: Vec<_> = n.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(fields, [" a  b ", "x // y"]);
    }

    #[test]
    fn parse_quoted_plain_name_equals_bare_name() {
        assert_eq!(
            parse("node `Person` { `name`: String }").unwrap(),
            parse("node Person { name: String }").unwrap()
        );
    }

    #[test]
    fn parse_rejects_malformed_identifiers() {
        for src in [
            "node T { first name: String }",
            "node T { a-b: String }",
            "node T { a.b: String }",
            "node T { 1abc: String }",
            "node T { \"first name\": String }",
            "node T { ``: String }",
            "node T { `a\nb`: String }",
            "node T { `open: String }",
            "node My Type { }",
            "node T { x: String @`index` }",
        ] {
            assert!(parse(src).is_err(), "should be rejected: {src:?}");
        }
    }

    #[test]
    fn parse_keywords_remain_usable_as_field_names() {
        let n = node_named("node T { type: String from: String to: String node: Int props: Int edge: Int extends: Int }");
        let fields: Vec<_> = n.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            fields,
            ["type", "from", "to", "node", "props", "edge", "extends"]
        );
    }

    #[test]
    fn parse_user_type_whose_name_starts_with_a_primitive_name() {
        let n = node_named("node T { a: Integer b: StringValue c: Int d: List<Blobby> e: Int型 }");
        let types: Vec<_> = n.fields.iter().map(|f| f.type_expr.clone()).collect();
        assert_eq!(
            types,
            [
                TypeExpr::Named("Integer".into()),
                TypeExpr::Named("StringValue".into()),
                TypeExpr::Int,
                TypeExpr::List(Box::new(TypeExpr::Named("Blobby".into()))),
                TypeExpr::Named("Int型".into()),
            ]
        );
    }

    #[test]
    fn parse_quoted_primitive_name_is_a_user_type() {
        let n = node_named("node T { a: `Int` }");
        assert_eq!(n.fields[0].type_expr, TypeExpr::Named("Int".into()));
    }

    #[test]
    fn names_are_not_unicode_normalized() {
        // "が" precomposed (U+304C) vs. "か" + combining dakuten (U+3099).
        let n = node_named("node T { \u{304C}: Int \u{304B}\u{3099}: Int }");
        assert_eq!(n.fields[0].name, "\u{304C}");
        assert_eq!(n.fields[1].name, "\u{304B}\u{3099}");
        assert_ne!(n.fields[0].name, n.fields[1].name);
    }
}
