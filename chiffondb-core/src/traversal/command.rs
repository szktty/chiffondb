use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TraversalCommand {
    pub version: u32,
    /// Binding variable map: key → value. Referenced via `{ "$bind": "key" }` syntax.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub bindings: std::collections::HashMap<String, serde_json::Value>,
    pub start: StartSpec,
    pub steps: Vec<TraversalStep>,
    pub collect: CollectSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StartSpec {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TraversalStep {
    pub action: TraversalAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// For HasLabel action: list of labels to match (true if any matches).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Filter>,
    /// For Or/Not compound filters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compound_filter: Option<CompoundFilter>,
    /// For Limit action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    /// For Skip action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<usize>,
    /// For OrderBy action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_by: Option<OrderBySpec>,
    /// For Repeat action: the steps to iterate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_steps: Option<Vec<TraversalStep>>,
    /// For Repeat action: maximum iteration depth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<usize>,
    /// Custom filter (batch mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_filter: Option<CustomFilter>,
    /// Substring match filter applied to all string properties.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub any_key_contains: Option<String>,
}

impl Default for TraversalStep {
    fn default() -> Self {
        Self {
            action: TraversalAction::Filter,
            label: None,
            labels: None,
            filter: None,
            compound_filter: None,
            count: None,
            skip: None,
            order_by: None,
            repeat_steps: None,
            max_depth: None,
            custom_filter: None,
            any_key_contains: None,
        }
    }
}

/// A property accessor: either a flat top-level key or a path into a nested `Json` value.
///
/// Serialized untagged so existing flat-string JSON keeps working: a bare string
/// (`"name"`) deserializes to `Flat`, an object (`{"path":["props","name"]}`) to `Path`.
/// `Path` resolution only reaches nested *scalar* values (it walks `Value::Object`
/// levels); array indexing and JSON containment are out of scope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum PropertyPath {
    /// Top-level property key (backward-compatible plain string).
    Flat(String),
    /// Path into a nested object value.
    Path { path: Vec<String> },
}

impl PropertyPath {
    /// Resolves the path against a property map, returning the referenced value if present.
    /// Walks nested objects for `Path`; returns `None` if any segment is missing or a
    /// non-object is reached before the final segment.
    pub fn resolve<'a>(
        &self,
        props: &'a std::collections::HashMap<String, serde_json::Value>,
    ) -> Option<&'a serde_json::Value> {
        match self {
            PropertyPath::Flat(key) => props.get(key),
            PropertyPath::Path { path } => {
                let (first, rest) = path.split_first()?;
                let mut current = props.get(first)?;
                for segment in rest {
                    current = current.as_object()?.get(segment)?;
                }
                Some(current)
            }
        }
    }

    /// A human-readable name for this path, used as a default output key.
    /// `Flat` is the key itself; `Path` joins segments with `.`.
    pub fn display_name(&self) -> String {
        match self {
            PropertyPath::Flat(key) => key.clone(),
            PropertyPath::Path { path } => path.join("."),
        }
    }
}

impl From<&str> for PropertyPath {
    fn from(s: &str) -> Self {
        PropertyPath::Flat(s.to_string())
    }
}

impl From<String> for PropertyPath {
    fn from(s: String) -> Self {
        PropertyPath::Flat(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrderBySpec {
    pub key: PropertyPath,
    pub direction: SortDirection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum TraversalAction {
    OutEdges,
    InEdges,
    BothEdges,
    OutNodes,
    InNodes,
    BothNodes,
    Filter,
    HasLabel,
    Dedup,
    Limit,
    Skip,
    OrderBy,
    /// Repeats repeat_steps up to max_depth times (BFS-style expansion).
    Repeat,
}

/// The comparison target for a filter: literal, property reference, or binding variable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum FilterValue {
    /// Binding variable reference: `{ "$bind": "key" }` form (tried before other variants).
    Binding {
        #[serde(rename = "$bind")]
        bind: String,
    },
    /// Reference to another property on the same node/edge: `{ "type": "property", "key": "minAge" }` form.
    /// `key` accepts the same path forms as `Filter.property`.
    PropertyRef {
        r#type: PropertyRefMarker,
        key: PropertyPath,
    },
    /// Fixed literal value (fallback when other variants do not match).
    Literal(serde_json::Value),
}

/// Discriminant tag for `FilterValue::PropertyRef`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PropertyRefMarker {
    Property,
}

impl FilterValue {
    /// Resolves the value and returns it.
    /// - `Literal`: returned as-is.
    /// - `PropertyRef`: looks up the field from `props`.
    /// - `Binding`: resolves from `bindings`.
    pub fn resolve<'a>(
        &'a self,
        props: Option<&'a std::collections::HashMap<String, serde_json::Value>>,
        bindings: &'a std::collections::HashMap<String, serde_json::Value>,
    ) -> Option<std::borrow::Cow<'a, serde_json::Value>> {
        match self {
            FilterValue::Literal(v) => Some(std::borrow::Cow::Borrowed(v)),
            FilterValue::PropertyRef { key, .. } => {
                key.resolve(props?).map(std::borrow::Cow::Borrowed)
            }
            FilterValue::Binding { bind } => bindings.get(bind).map(std::borrow::Cow::Borrowed),
        }
    }
}

/// Resolves a `{ "$bind": "key" }` object inside a `serde_json::Value` using the bindings map.
/// Returns the value unchanged if it is not a binding reference.
pub fn resolve_binding<'a>(
    value: &'a serde_json::Value,
    bindings: &'a std::collections::HashMap<String, serde_json::Value>,
) -> std::borrow::Cow<'a, serde_json::Value> {
    if let serde_json::Value::Object(map) = value {
        if map.len() == 1 {
            if let Some(serde_json::Value::String(key)) = map.get("$bind") {
                if let Some(resolved) = bindings.get(key) {
                    return std::borrow::Cow::Borrowed(resolved);
                }
            }
        }
    }
    std::borrow::Cow::Borrowed(value)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Filter {
    pub property: PropertyPath,
    pub operator: FilterOperator,
    #[serde(default = "default_filter_value")]
    pub value: FilterValue,
}

fn default_filter_value() -> FilterValue {
    FilterValue::Literal(serde_json::Value::Null)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FilterOperator {
    Equals,
    NotEquals,
    GreaterThan,
    LessThan,
    GreaterThanOrEquals,
    LessThanOrEquals,
    Contains,
    StartsWith,
    EndsWith,
    /// Field exists and is not null.
    Exists,
    /// Field does not exist or its value is null.
    IsNull,
    /// Regular-expression match (value is a regex string).
    Matches,
    /// Compared against another property on the same node/edge (value is a PropertyRef).
    PropertyEquals,
    PropertyGreaterThan,
    PropertyLessThan,
}

/// Compound filter (And / Or / Not).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "operator")]
pub enum CompoundFilter {
    /// True if all child filters are true.
    And { filters: Vec<FilterExpr> },
    /// True if any child filter is true.
    Or { filters: Vec<FilterExpr> },
    /// True if the child filter is false.
    Not { filter: Box<FilterExpr> },
}

/// A filter expression: either a simple filter or a compound filter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum FilterExpr {
    Simple(Filter),
    Compound(CompoundFilter),
}

/// Custom filter (batch mode).
/// Passes candidate property lists to Dart/external code and receives the passing RecordIds back.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomFilter {
    /// Filter identifier (name of the function registered on the Dart side).
    pub callback_id: String,
}

/// Specifies how to collect the traversal result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum CollectSpec {
    /// Returns node property maps. `properties` is the list of keys to include (empty = all).
    Nodes {
        #[serde(default)]
        properties: Vec<String>,
        /// If set, also include properties from the edge that connects to each node.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        with_edges: Option<EdgeCollectSpec>,
    },
    /// Returns edge property maps. `properties` is the list of keys to include (empty = all).
    Edges {
        #[serde(default)]
        properties: Vec<String>,
    },
    /// Returns a count of the current cursor items.
    Count,
    /// Returns true if the cursor is non-empty, false otherwise.
    Exists,
    /// Returns property maps for each node along the full path.
    Path {
        /// Per-label property selection. Key = label name, value = list of property keys (empty = all).
        #[serde(default)]
        node_specs: Vec<PathNodeSpec>,
    },
    /// Computes aggregate functions over the current node cursor (no grouping).
    Aggregate { functions: Vec<AggregateFunction> },
    /// Groups nodes by a property and computes aggregate functions per group.
    GroupBy {
        /// Property key to group by. Prefix with `edge:` to group by the connecting edge's property.
        key: GroupByKey,
        functions: Vec<AggregateFunction>,
    },
}

/// Specifies which edge properties to include alongside node results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EdgeCollectSpec {
    /// Edge label to match. If None, the most recently traversed edge is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Property keys to include (empty = all).
    #[serde(default)]
    pub properties: Vec<String>,
}

/// Specifies which properties to collect for a node type along a path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PathNodeSpec {
    pub label: String,
    /// Property keys to include (empty = all).
    #[serde(default)]
    pub properties: Vec<String>,
}

/// An aggregate function applied to a property.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AggregateFunction {
    pub func: AggregateFn,
    /// Property key to aggregate. Not required for `Count`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<PropertyPath>,
    /// Output key in the result map. Defaults to `func_property` (e.g. `sum_score`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

impl AggregateFunction {
    pub fn output_key(&self) -> String {
        if let Some(alias) = &self.alias {
            return alias.clone();
        }
        match &self.property {
            Some(p) => format!("{}_{}", self.func.as_str(), p.display_name()),
            None => self.func.as_str().to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum AggregateFn {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggregateFn {
    pub fn as_str(&self) -> &'static str {
        match self {
            AggregateFn::Count => "count",
            AggregateFn::Sum => "sum",
            AggregateFn::Avg => "avg",
            AggregateFn::Min => "min",
            AggregateFn::Max => "max",
        }
    }
}

/// The key to group by.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "source")]
pub enum GroupByKey {
    /// Group by a node property.
    Node { property: PropertyPath },
    /// Group by a property of the connecting edge.
    Edge { property: PropertyPath },
}

/// The kind of traversal result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum CollectResult {
    /// A list of property maps (Nodes collect).
    Rows(Vec<std::collections::HashMap<String, serde_json::Value>>),
    /// A count.
    Count(usize),
    /// Exists check result.
    Exists(bool),
    /// Aggregate result: a single map of output_key → value.
    Aggregate(std::collections::HashMap<String, serde_json::Value>),
    /// GroupBy result: list of { group_key, aggregates } maps.
    Groups(Vec<std::collections::HashMap<String, serde_json::Value>>),
    /// Path result: list of per-hop node property maps.
    Path(Vec<Vec<std::collections::HashMap<String, serde_json::Value>>>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserialize_nodes_collect() {
        let json = r#"{
            "version": 1,
            "start": {
                "type": "Node",
                "label": "User",
                "key": "id",
                "value": "user_123"
            },
            "steps": [
                { "action": "OutEdges", "label": "OWNS", "filter": null },
                { "action": "InNodes", "label": "Project", "filter": {
                    "property": "isArchived",
                    "operator": "Equals",
                    "value": false
                }}
            ],
            "collect": {
                "type": "Nodes",
                "properties": ["id", "title"]
            }
        }"#;
        let cmd: TraversalCommand = serde_json::from_str(json).unwrap();
        assert_eq!(cmd.version, 1);
        assert_eq!(cmd.start.label, "User");
        assert_eq!(cmd.steps.len(), 2);
        assert_eq!(cmd.steps[0].action, TraversalAction::OutEdges);
        assert!(matches!(
            &cmd.collect,
            CollectSpec::Nodes { properties, .. } if properties == &["id", "title"]
        ));
    }

    #[test]
    fn deserialize_count_collect() {
        let json = r#"{
            "version": 1,
            "start": { "type": "AllNodes", "label": "User", "key": "", "value": null },
            "steps": [],
            "collect": { "type": "Count" }
        }"#;
        let cmd: TraversalCommand = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd.collect, CollectSpec::Count));
    }

    #[test]
    fn deserialize_aggregate_collect() {
        let json = r#"{
            "version": 1,
            "start": { "type": "AllNodes", "label": "Document", "key": "", "value": null },
            "steps": [],
            "collect": {
                "type": "Aggregate",
                "functions": [
                    { "func": "Sum", "property": "score" },
                    { "func": "Avg", "property": "score" },
                    { "func": "Count" }
                ]
            }
        }"#;
        let cmd: TraversalCommand = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd.collect, CollectSpec::Aggregate { .. }));
    }

    #[test]
    fn deserialize_group_by_collect() {
        let json = r#"{
            "version": 1,
            "start": { "type": "AllNodes", "label": "Project", "key": "", "value": null },
            "steps": [],
            "collect": {
                "type": "GroupBy",
                "key": { "source": "Node", "property": "status" },
                "functions": [
                    { "func": "Count" }
                ]
            }
        }"#;
        let cmd: TraversalCommand = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd.collect, CollectSpec::GroupBy { .. }));
    }

    #[test]
    fn aggregate_function_output_key() {
        let f = AggregateFunction {
            func: AggregateFn::Sum,
            property: Some("score".into()),
            alias: None,
        };
        assert_eq!(f.output_key(), "sum_score");

        let f = AggregateFunction {
            func: AggregateFn::Count,
            property: None,
            alias: None,
        };
        assert_eq!(f.output_key(), "count");

        let f = AggregateFunction {
            func: AggregateFn::Avg,
            property: Some("score".into()),
            alias: Some("avg".into()),
        };
        assert_eq!(f.output_key(), "avg");
    }

    #[test]
    fn property_path_serde_roundtrip() {
        // Bare string deserializes to Flat (backward compatible).
        let flat: PropertyPath = serde_json::from_str(r#""name""#).unwrap();
        assert_eq!(flat, PropertyPath::Flat("name".to_string()));
        assert_eq!(serde_json::to_string(&flat).unwrap(), r#""name""#);

        // Object with `path` deserializes to Path.
        let path: PropertyPath = serde_json::from_str(r#"{"path":["props","name"]}"#).unwrap();
        assert_eq!(
            path,
            PropertyPath::Path {
                path: vec!["props".to_string(), "name".to_string()]
            }
        );
        assert_eq!(
            serde_json::to_string(&path).unwrap(),
            r#"{"path":["props","name"]}"#
        );
    }

    #[test]
    fn property_path_resolve() {
        use std::collections::HashMap;
        let props: HashMap<String, serde_json::Value> = HashMap::from([
            ("name".to_string(), json!("Alice")),
            (
                "props".to_string(),
                json!({"year": 1867, "addr": {"city": "Kyoto"}}),
            ),
        ]);

        // Flat resolves top-level.
        assert_eq!(
            PropertyPath::from("name").resolve(&props),
            Some(&json!("Alice"))
        );
        // Path resolves nested scalar.
        let p = PropertyPath::Path {
            path: vec!["props".to_string(), "year".to_string()],
        };
        assert_eq!(p.resolve(&props), Some(&json!(1867)));
        // Deeply nested.
        let p = PropertyPath::Path {
            path: vec!["props".to_string(), "addr".to_string(), "city".to_string()],
        };
        assert_eq!(p.resolve(&props), Some(&json!("Kyoto")));
        // Missing segment returns None.
        let p = PropertyPath::Path {
            path: vec!["props".to_string(), "missing".to_string()],
        };
        assert_eq!(p.resolve(&props), None);
        // Non-object before final segment returns None.
        let p = PropertyPath::Path {
            path: vec!["name".to_string(), "x".to_string()],
        };
        assert_eq!(p.resolve(&props), None);
    }

    #[test]
    fn filter_deserializes_nested_path_property() {
        let json = r#"{
            "version": 1,
            "start": { "type": "AllNodes", "label": "Entity", "key": "", "value": null },
            "steps": [
                { "action": "Filter", "label": "Entity", "filter": {
                    "property": {"path": ["props", "year"]},
                    "operator": "Equals",
                    "value": 1867
                }}
            ],
            "collect": { "type": "Count" }
        }"#;
        let cmd: TraversalCommand = serde_json::from_str(json).unwrap();
        let f = cmd.steps[0].filter.as_ref().unwrap();
        assert_eq!(
            f.property,
            PropertyPath::Path {
                path: vec!["props".to_string(), "year".to_string()]
            }
        );
    }
}
