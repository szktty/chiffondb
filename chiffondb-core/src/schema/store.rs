use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::error::GraphError;
use crate::schema::ast::{BoundParam, Definition, EdgeDef, FieldDef, NodeDef, SchemaAst, TypeExpr};
use crate::storage::file::DatabaseFile;
use crate::storage::page::PAGE_SIZE;
use crate::storage::property::{pages_needed, read_blob_chain, write_blob_chain};

// ---- Intermediate representation for serde ----
// Mirror types for directly serializing the AST.

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
struct SchemaDto {
    definitions: Vec<DefinitionDto>,
    /// Persisted name→type_id assignments for node types.
    /// Without this, ids would be reassigned from definition order on every load,
    /// silently changing the type of already-stored nodes when a type is added or removed.
    #[serde(default)]
    node_type_ids: Vec<(String, u16)>,
    /// Persisted name→type_id assignments for edge types.
    #[serde(default)]
    edge_type_ids: Vec<(String, u16)>,
    /// Next id to hand out for a newly introduced node type.
    /// Monotonically increasing so a deleted type's id is never reused.
    #[serde(default)]
    next_node_id: u16,
    /// Next id to hand out for a newly introduced edge type.
    #[serde(default)]
    next_edge_id: u16,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(tag = "kind")]
enum DefinitionDto {
    Node(NodeDefDto),
    Edge(EdgeDefDto),
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
struct NodeDefDto {
    name: String,
    type_params: Vec<String>,
    extends: Option<String>,
    fields: Vec<FieldDefDto>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
struct EdgeDefDto {
    name: String,
    generic_params: Vec<BoundParamDto>,
    from: TypeExprDto,
    to: TypeExprDto,
    props: Vec<FieldDefDto>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
struct BoundParamDto {
    name: String,
    bound: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
struct FieldDefDto {
    name: String,
    type_expr: TypeExprDto,
    #[serde(default)]
    indexed: bool,
    #[serde(default)]
    unique: bool,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(tag = "t", content = "v")]
enum TypeExprDto {
    Int,
    Float,
    Boolean,
    DateTime,
    String,
    Json,
    Blob,
    Vector(u32),
    List(Box<TypeExprDto>),
    Map(Box<TypeExprDto>, Box<TypeExprDto>),
    NodeRef(String),
    EdgeRef(String),
    Named(String),
}

// ---- AST ↔ DTO conversion ----

fn ast_to_dto(ast: &SchemaAst) -> SchemaDto {
    SchemaDto {
        definitions: ast.definitions.iter().map(def_to_dto).collect(),
        node_type_ids: Vec::new(),
        edge_type_ids: Vec::new(),
        next_node_id: 1,
        next_edge_id: 1,
    }
}

fn def_to_dto(def: &Definition) -> DefinitionDto {
    match def {
        Definition::Node(n) => DefinitionDto::Node(NodeDefDto {
            name: n.name.clone(),
            type_params: n.type_params.clone(),
            extends: n.extends.clone(),
            fields: n.fields.iter().map(field_to_dto).collect(),
        }),
        Definition::Edge(e) => DefinitionDto::Edge(EdgeDefDto {
            name: e.name.clone(),
            generic_params: e
                .generic_params
                .iter()
                .map(|p| BoundParamDto {
                    name: p.name.clone(),
                    bound: p.bound.clone(),
                })
                .collect(),
            from: type_to_dto(&e.from),
            to: type_to_dto(&e.to),
            props: e.props.iter().map(field_to_dto).collect(),
        }),
    }
}

fn field_to_dto(f: &FieldDef) -> FieldDefDto {
    FieldDefDto {
        name: f.name.clone(),
        type_expr: type_to_dto(&f.type_expr),
        indexed: f.indexed,
        unique: f.unique,
    }
}

fn type_to_dto(t: &TypeExpr) -> TypeExprDto {
    match t {
        TypeExpr::Int => TypeExprDto::Int,
        TypeExpr::Float => TypeExprDto::Float,
        TypeExpr::Boolean => TypeExprDto::Boolean,
        TypeExpr::DateTime => TypeExprDto::DateTime,
        TypeExpr::String => TypeExprDto::String,
        TypeExpr::Json => TypeExprDto::Json,
        TypeExpr::Blob => TypeExprDto::Blob,
        TypeExpr::Vector(n) => TypeExprDto::Vector(*n),
        TypeExpr::List(e) => TypeExprDto::List(Box::new(type_to_dto(e))),
        TypeExpr::Map(k, v) => TypeExprDto::Map(Box::new(type_to_dto(k)), Box::new(type_to_dto(v))),
        TypeExpr::NodeRef(s) => TypeExprDto::NodeRef(s.clone()),
        TypeExpr::EdgeRef(s) => TypeExprDto::EdgeRef(s.clone()),
        TypeExpr::Named(s) => TypeExprDto::Named(s.clone()),
    }
}

fn dto_to_ast(dto: SchemaDto) -> SchemaAst {
    SchemaAst {
        definitions: dto.definitions.into_iter().map(dto_to_def).collect(),
    }
}

fn dto_to_def(dto: DefinitionDto) -> Definition {
    match dto {
        DefinitionDto::Node(n) => Definition::Node(NodeDef {
            name: n.name,
            type_params: n.type_params,
            extends: n.extends,
            fields: n.fields.into_iter().map(dto_to_field).collect(),
        }),
        DefinitionDto::Edge(e) => Definition::Edge(EdgeDef {
            name: e.name,
            generic_params: e
                .generic_params
                .into_iter()
                .map(|p| BoundParam {
                    name: p.name,
                    bound: p.bound,
                })
                .collect(),
            from: dto_to_type(e.from),
            to: dto_to_type(e.to),
            props: e.props.into_iter().map(dto_to_field).collect(),
        }),
    }
}

fn dto_to_field(f: FieldDefDto) -> FieldDef {
    FieldDef {
        name: f.name,
        type_expr: dto_to_type(f.type_expr),
        indexed: f.indexed,
        unique: f.unique,
    }
}

fn dto_to_type(t: TypeExprDto) -> TypeExpr {
    match t {
        TypeExprDto::Int => TypeExpr::Int,
        TypeExprDto::Float => TypeExpr::Float,
        TypeExprDto::Boolean => TypeExpr::Boolean,
        TypeExprDto::DateTime => TypeExpr::DateTime,
        TypeExprDto::String => TypeExpr::String,
        TypeExprDto::Json => TypeExpr::Json,
        TypeExprDto::Blob => TypeExpr::Blob,
        TypeExprDto::Vector(n) => TypeExpr::Vector(n),
        TypeExprDto::List(e) => TypeExpr::List(Box::new(dto_to_type(*e))),
        TypeExprDto::Map(k, v) => {
            TypeExpr::Map(Box::new(dto_to_type(*k)), Box::new(dto_to_type(*v)))
        }
        TypeExprDto::NodeRef(s) => TypeExpr::NodeRef(s),
        TypeExprDto::EdgeRef(s) => TypeExpr::EdgeRef(s),
        TypeExprDto::Named(s) => TypeExpr::Named(s),
    }
}

// ---- DB persistence ----

/// Saves the schema AST to the schema pages in the database file.
/// If a schema already exists, validates the migration policy and increments schema_version.
pub fn save_schema(db: &mut DatabaseFile, ast: &SchemaAst) -> Result<(), GraphError> {
    // Inherit existing type_id assignments so that adding/removing a type never
    // shifts the id of a type that is still present.
    let previous = if db.header.schema_root != 0 {
        match load_schema_dto(db) {
            Ok(old) => {
                crate::schema::migration::validate_migration(&dto_to_ast(old.clone()), ast)?;
                Some(old)
            }
            Err(_) => None,
        }
    } else {
        None
    };

    let mut dto = ast_to_dto(ast);
    assign_type_ids(&mut dto, previous.as_ref());
    write_schema_dto(db, &dto)
}

/// Serializes a DTO to a fresh schema page chain and bumps schema_version.
fn write_schema_dto(db: &mut DatabaseFile, dto: &SchemaDto) -> Result<(), GraphError> {
    let bytes = serde_json::to_vec(dto).map_err(|e| GraphError::SchemaError(e.to_string()))?;

    let needed = pages_needed(bytes.len()).max(1);
    let mut pages = vec![[0u8; PAGE_SIZE]; needed];
    write_blob_chain(&mut pages, &bytes)?;

    let first_page_id = db.append_page(&pages[0])?;
    for page in pages.iter().skip(1) {
        db.append_page(page)?;
    }

    db.header.schema_root = first_page_id;
    db.header.schema_version += 1;
    db.write_header()?;
    db.flush()
}

/// The kind of type being registered dynamically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicTypeKind {
    Node,
    Edge,
}

/// Registers a label/type id without applying a full schema, for apps that grow a
/// schemaless set of labels on top of a fixed meta-schema. Returns `(id, created)`:
/// the assigned id, and whether it was newly minted (`true`) or already existed (`false`).
///
/// The name is added to the persisted type_id assignments only; no AST definition is
/// created. `save_schema` preserves such definition-less assignments (see `assign_type_ids`),
/// so a later schema migration will not drop dynamically registered labels.
pub fn register_dynamic_type(
    db: &mut DatabaseFile,
    kind: DynamicTypeKind,
    name: &str,
) -> Result<(u16, bool), GraphError> {
    let mut dto = load_schema_dto(db)?;
    let (assignments, next_id) = match kind {
        DynamicTypeKind::Node => (&mut dto.node_type_ids, &mut dto.next_node_id),
        DynamicTypeKind::Edge => (&mut dto.edge_type_ids, &mut dto.next_edge_id),
    };

    if let Some((_, id)) = assignments.iter().find(|(n, _)| n == name) {
        return Ok((*id, false));
    }

    let id = (*next_id).max(1);
    // u16 id space: fail rather than wrap (wrapping to 0 → max(1) would re-issue id 1 and
    // confuse it with an existing type).
    *next_id = id
        .checked_add(1)
        .ok_or_else(|| GraphError::SchemaError("type id space exhausted (u16)".to_string()))?;
    assignments.push((name.to_string(), id));
    write_schema_dto(db, &dto)?;
    Ok((id, true))
}

/// Reads the schema AST from the database file.
pub fn load_schema(db: &mut DatabaseFile) -> Result<SchemaAst, GraphError> {
    Ok(dto_to_ast(load_schema_dto(db)?))
}

/// Reads the raw schema DTO (definitions plus persisted type_id assignments).
fn load_schema_dto(db: &mut DatabaseFile) -> Result<SchemaDto, GraphError> {
    let first_page_id = db.header.schema_root;
    if first_page_id == 0 {
        return Err(GraphError::SchemaError("no schema stored".to_string()));
    }

    // Read the entire page chain. Bound the walk by the total page count so a corrupt/hostile
    // `next` (e.g. a cycle, or `next=0` re-reading the same page) cannot loop forever or grow
    // `pages` without limit.
    let page_count = db.page_count()?;
    let mut pages: Vec<[u8; PAGE_SIZE]> = Vec::new();
    let mut page_id = first_page_id;
    loop {
        let page = db.read_page(page_id)?;
        let next = u32::from_le_bytes(page[0..4].try_into().unwrap());
        pages.push(page);
        if next == 0xFFFF_FFFF {
            break;
        }
        if pages.len() as u32 > page_count {
            return Err(GraphError::StorageCorrupted(page_id));
        }
        // next is a relative index within the chain; convert to an absolute page ID.
        page_id = first_page_id
            .checked_add(next)
            .ok_or(GraphError::StorageCorrupted(page_id))?;
    }

    let bytes = read_blob_chain(&pages)?;
    serde_json::from_slice(&bytes).map_err(|e| GraphError::SchemaError(e.to_string()))
}

/// Loads the persisted (name, type_id) assignments for nodes and edges.
/// Used to build a `SchemaRegistry` whose ids are stable across schema changes.
#[allow(clippy::type_complexity)]
pub fn load_type_assignments(
    db: &mut DatabaseFile,
) -> Result<(Vec<(String, u16)>, Vec<(String, u16)>), GraphError> {
    let dto = load_schema_dto(db)?;
    Ok((dto.node_type_ids, dto.edge_type_ids))
}

/// Assigns type_ids to the freshly built DTO, inheriting any ids present in `previous`
/// and minting new ones from a monotonic counter so a removed type's id is never reused.
fn assign_type_ids(dto: &mut SchemaDto, previous: Option<&SchemaDto>) {
    let (prev_nodes, prev_edges, mut next_node, mut next_edge) = match previous {
        Some(p) => {
            let nodes: HashMap<&str, u16> = p
                .node_type_ids
                .iter()
                .map(|(n, i)| (n.as_str(), *i))
                .collect();
            let edges: HashMap<&str, u16> = p
                .edge_type_ids
                .iter()
                .map(|(n, i)| (n.as_str(), *i))
                .collect();
            (nodes, edges, p.next_node_id.max(1), p.next_edge_id.max(1))
        }
        None => (HashMap::new(), HashMap::new(), 1u16, 1u16),
    };

    let mut node_ids = Vec::new();
    let mut edge_ids = Vec::new();
    for def in &dto.definitions {
        match def {
            DefinitionDto::Node(n) => {
                let id = prev_nodes.get(n.name.as_str()).copied().unwrap_or_else(|| {
                    let id = next_node;
                    next_node += 1;
                    id
                });
                node_ids.push((n.name.clone(), id));
            }
            DefinitionDto::Edge(e) => {
                let id = prev_edges.get(e.name.as_str()).copied().unwrap_or_else(|| {
                    let id = next_edge;
                    next_edge += 1;
                    id
                });
                edge_ids.push((e.name.clone(), id));
            }
        }
    }

    // Preserve dynamically registered labels: assignments that exist in the previous DTO
    // but have no AST definition in the new schema. Without this, a later save_schema would
    // drop labels registered via register_dynamic_type.
    if let Some(p) = previous {
        let defined_nodes: HashSet<String> = node_ids.iter().map(|(n, _)| n.clone()).collect();
        for (name, id) in &p.node_type_ids {
            if !defined_nodes.contains(name) {
                node_ids.push((name.clone(), *id));
            }
        }
        let defined_edges: HashSet<String> = edge_ids.iter().map(|(n, _)| n.clone()).collect();
        for (name, id) in &p.edge_type_ids {
            if !defined_edges.contains(name) {
                edge_ids.push((name.clone(), *id));
            }
        }
    }

    dto.node_type_ids = node_ids;
    dto.edge_type_ids = edge_ids;
    dto.next_node_id = next_node;
    dto.next_edge_id = next_edge;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::parser::parse;
    use crate::storage::file::DatabaseFile;
    use tempfile::NamedTempFile;

    fn make_db() -> (DatabaseFile, std::path::PathBuf) {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.into_temp_path().to_path_buf();
        std::fs::remove_file(&path).ok();
        let db = DatabaseFile::create(&path).unwrap();
        (db, path)
    }

    #[test]
    fn save_and_load_schema_roundtrip() {
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
        let (mut db, path) = make_db();

        save_schema(&mut db, &ast).unwrap();
        drop(db);

        let mut db2 = DatabaseFile::open(&path).unwrap();
        let loaded = load_schema(&mut db2).unwrap();
        assert_eq!(ast, loaded);
    }

    #[test]
    fn load_schema_without_save_returns_error() {
        let (mut db, _path) = make_db();
        assert!(load_schema(&mut db).is_err());
    }

    #[test]
    fn schema_version_increments_on_save() {
        let (mut db, _) = make_db();
        assert_eq!(db.header.schema_version, 0);

        let ast = parse("node User { id: String }").unwrap();
        save_schema(&mut db, &ast).unwrap();
        assert_eq!(db.header.schema_version, 1);

        // Adding a field (an allowed change) still increments the version
        let ast2 = parse("node User { id: String  name: String }").unwrap();
        save_schema(&mut db, &ast2).unwrap();
        assert_eq!(db.header.schema_version, 2);
    }

    #[test]
    fn field_type_change_is_rejected_on_save() {
        let (mut db, _) = make_db();
        let ast1 = parse("node User { id: String }").unwrap();
        save_schema(&mut db, &ast1).unwrap();

        let ast2 = parse("node User { id: Int }").unwrap();
        assert!(save_schema(&mut db, &ast2).is_err());
        // Version must not change
        assert_eq!(db.header.schema_version, 1);
    }

    #[test]
    fn schema_version_persists_across_reopen() {
        let (mut db, path) = make_db();
        let ast = parse("node User { id: String }").unwrap();
        save_schema(&mut db, &ast).unwrap();
        drop(db);

        let db2 = DatabaseFile::open(&path).unwrap();
        assert_eq!(db2.header.schema_version, 1);
    }

    #[test]
    fn register_dynamic_type_mints_and_reuses() {
        let (mut db, _) = make_db();
        save_schema(&mut db, &parse("node User { id: String }").unwrap()).unwrap();

        // Unknown name is minted; its id comes after the schema-defined User (id 1).
        let (id1, created1) =
            register_dynamic_type(&mut db, DynamicTypeKind::Node, "Person").unwrap();
        assert!(created1);
        assert_eq!(id1, 2);

        // Second unknown name gets the next id.
        let (id2, created2) = register_dynamic_type(&mut db, DynamicTypeKind::Node, "VIP").unwrap();
        assert!(created2);
        assert_eq!(id2, 3);

        // Re-registering returns the existing id without minting.
        let (id3, created3) =
            register_dynamic_type(&mut db, DynamicTypeKind::Node, "Person").unwrap();
        assert!(!created3);
        assert_eq!(id3, id1);

        // An existing schema type name resolves to its existing id, created=false.
        let (id_user, created_user) =
            register_dynamic_type(&mut db, DynamicTypeKind::Node, "User").unwrap();
        assert!(!created_user);
        assert_eq!(id_user, 1);
    }

    #[test]
    fn register_dynamic_type_survives_save_schema() {
        let (mut db, _) = make_db();
        save_schema(&mut db, &parse("node User { id: String }").unwrap()).unwrap();
        let (person_id, _) =
            register_dynamic_type(&mut db, DynamicTypeKind::Node, "Person").unwrap();

        // A later schema migration must keep the dynamic label and its id.
        save_schema(
            &mut db,
            &parse("node User { id: String  name: String }").unwrap(),
        )
        .unwrap();

        let (id_again, created) =
            register_dynamic_type(&mut db, DynamicTypeKind::Node, "Person").unwrap();
        assert!(!created);
        assert_eq!(id_again, person_id);
    }
}
