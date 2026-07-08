#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaAst {
    pub definitions: Vec<Definition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Definition {
    Node(NodeDef),
    Edge(EdgeDef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeDef {
    pub name: String,
    pub type_params: Vec<String>,
    pub extends: Option<String>,
    pub fields: Vec<FieldDef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeDef {
    pub name: String,
    pub generic_params: Vec<BoundParam>,
    pub from: TypeExpr,
    pub to: TypeExpr,
    pub props: Vec<FieldDef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundParam {
    pub name: String,
    pub bound: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDef {
    pub name: String,
    pub type_expr: TypeExpr,
    /// `@index`: a tier-2 property index is maintained for this field (Phase 5).
    pub indexed: bool,
    /// `@unique`: reserved for the tier-3 unique constraint (Phase 6); parsed but not yet
    /// enforced.
    pub unique: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeExpr {
    Int,
    Float,
    Boolean,
    DateTime,
    String,
    Json,
    Blob,
    Vector(u32),
    List(Box<TypeExpr>),
    Map(Box<TypeExpr>, Box<TypeExpr>),
    NodeRef(std::string::String),
    EdgeRef(std::string::String),
    Named(std::string::String),
}
