use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::error::GraphError;
use crate::traversal::command::{
    resolve_binding, AggregateFn, AggregateFunction, CollectResult, CollectSpec, CompoundFilter,
    CustomFilter, EdgeCollectSpec, Filter, FilterExpr, FilterOperator, FilterValue, GroupByKey,
    OrderBySpec, PathNodeSpec, PropertyPath, SortDirection, TraversalAction, TraversalCommand,
    TraversalStep,
};

// ---- In-memory graph model ----
// To keep the Step 8 executor loosely coupled from the storage layer,
// the graph is abstracted via a trait. Tests use an in-memory implementation.

pub type NodeId = u64;
pub type EdgeId = u64;

#[derive(Debug, Clone)]
pub struct NodeData {
    pub id: NodeId,
    pub label: String,
    pub extra_labels: Vec<String>,
    pub properties: HashMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct EdgeData {
    pub id: EdgeId,
    pub label: String,
    pub from: NodeId,
    pub to: NodeId,
    pub properties: HashMap<String, Value>,
}

/// Graph access trait. The executor interacts with the graph through this interface.
pub trait GraphAccess {
    /// Looks up a node by label, property key, and value; returns its NodeId.
    fn find_node(&self, label: &str, key: &str, value: &Value) -> Option<NodeId>;
    /// Returns all nodes matching the given label (empty string = all nodes).
    /// Default implementation returns an empty list.
    fn all_nodes_of_label(&self, label: &str) -> Vec<NodeId> {
        let _ = label;
        vec![]
    }
    /// Returns all edge IDs matching the given label (empty string = all edges).
    /// Default implementation returns an empty list.
    fn all_edges_of_label(&self, label: &str) -> Vec<EdgeId> {
        let _ = label;
        vec![]
    }
    /// Returns the properties of a node.
    fn node_properties(&self, id: NodeId) -> Option<Cow<'_, HashMap<String, Value>>>;
    /// Returns the primary label of a node.
    ///
    /// Returns an owned `String` rather than a borrow: implementations may derive the label
    /// on demand from on-disk topology (no node-count-sized in-memory map to borrow from).
    fn node_label(&self, id: NodeId) -> Option<String>;
    /// Returns all labels of a node (primary + additional). Default implementation returns node_label only.
    fn node_labels(&self, id: NodeId) -> Vec<String> {
        self.node_label(id).into_iter().collect()
    }
    /// Returns the list of outgoing edge IDs for a node.
    fn out_edges(&self, node_id: NodeId, label: Option<&str>) -> Vec<EdgeId>;
    /// Returns the list of incoming edge IDs for a node.
    fn in_edges(&self, node_id: NodeId, label: Option<&str>) -> Vec<EdgeId>;
    /// Returns the destination node ID of an edge.
    fn edge_to_node(&self, edge_id: EdgeId) -> Option<NodeId>;
    /// Returns the source node ID of an edge.
    fn edge_from_node(&self, edge_id: EdgeId) -> Option<NodeId>;
    /// Returns the primary label of an edge.
    ///
    /// Returns an owned `String` (see `node_label`).
    fn edge_label(&self, id: EdgeId) -> Option<String>;
    /// Returns the properties of an edge.
    fn edge_properties(&self, id: EdgeId) -> Option<Cow<'_, HashMap<String, Value>>>;
    /// Custom filter (batch mode): accepts candidate IDs with their properties, returns the passing IDs.
    /// Default implementation passes all candidates (no custom filter registered).
    fn apply_custom_filter(
        &self,
        callback_id: &str,
        candidates: Vec<(NodeId, HashMap<String, Value>)>,
    ) -> Vec<NodeId> {
        let _ = callback_id;
        candidates.into_iter().map(|(id, _)| id).collect()
    }
}

// ---- Traversal result ----

/// A Cursor is an ordered list (Vec is used to support Dedup, Limit, Skip, and OrderBy).
/// Duplicates are allowed; they are removed explicitly by the Dedup action.
#[derive(Debug, Clone, PartialEq)]
pub enum Cursor {
    Nodes(Vec<NodeId>),
    Edges(Vec<EdgeId>),
}

/// Executes a traversal command and returns the result.
pub fn execute<G: GraphAccess>(
    graph: &G,
    cmd: &TraversalCommand,
) -> Result<CollectResult, GraphError> {
    let bindings = &cmd.bindings;

    // 1. start: build the initial cursor
    let start_value = resolve_binding(&cmd.start.value, bindings);
    let mut cursor = if cmd.start.kind == "AllEdges" {
        Cursor::Edges(graph.all_edges_of_label(&cmd.start.label))
    } else if cmd.start.kind == "AllNodes" {
        Cursor::Nodes(graph.all_nodes_of_label(&cmd.start.label))
    } else {
        let node = graph
            .find_node(&cmd.start.label, &cmd.start.key, &start_value)
            .ok_or_else(|| GraphError::NodeNotFound(start_value.to_string()))?;
        Cursor::Nodes(vec![node])
    };

    // 2. apply steps in sequence (pass binding variables to each step)
    for step in &cmd.steps {
        cursor = apply_step(graph, cursor, step, bindings)?;
    }

    // 3. collect
    collect(graph, cursor, &cmd.collect)
}

fn apply_step<G: GraphAccess>(
    graph: &G,
    cursor: Cursor,
    step: &TraversalStep,
    bindings: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<Cursor, GraphError> {
    let label = step.label.as_deref();

    let next = match step.action {
        TraversalAction::OutEdges => {
            let nodes = require_nodes(cursor)?;
            let edges: Vec<EdgeId> = nodes
                .iter()
                .flat_map(|&n| graph.out_edges(n, label))
                .collect();
            Cursor::Edges(edges)
        }
        TraversalAction::InEdges => {
            let nodes = require_nodes(cursor)?;
            let edges: Vec<EdgeId> = nodes
                .iter()
                .flat_map(|&n| graph.in_edges(n, label))
                .collect();
            Cursor::Edges(edges)
        }
        TraversalAction::BothEdges => {
            let nodes = require_nodes(cursor)?;
            let edges: Vec<EdgeId> = nodes
                .iter()
                .flat_map(|&n| {
                    graph
                        .out_edges(n, label)
                        .into_iter()
                        .chain(graph.in_edges(n, label))
                })
                .collect();
            Cursor::Edges(edges)
        }
        TraversalAction::OutNodes => {
            let edges = require_edges(cursor)?;
            let nodes: Vec<NodeId> = edges
                .iter()
                .filter_map(|&e| graph.edge_to_node(e))
                .filter(|&n| match label {
                    None => true,
                    Some(l) => graph.node_label(n).as_deref() == Some(l),
                })
                .collect();
            Cursor::Nodes(nodes)
        }
        TraversalAction::InNodes => {
            let edges = require_edges(cursor)?;
            let nodes: Vec<NodeId> = edges
                .iter()
                .filter_map(|&e| graph.edge_from_node(e))
                .filter(|&n| match label {
                    None => true,
                    Some(l) => graph.node_label(n).as_deref() == Some(l),
                })
                .collect();
            Cursor::Nodes(nodes)
        }
        TraversalAction::BothNodes => {
            let edges = require_edges(cursor)?;
            let nodes: Vec<NodeId> = edges
                .iter()
                .flat_map(|&e| {
                    graph
                        .edge_to_node(e)
                        .into_iter()
                        .chain(graph.edge_from_node(e))
                })
                .filter(|&n| match label {
                    None => true,
                    Some(l) => graph.node_label(n).as_deref() == Some(l),
                })
                .collect();
            Cursor::Nodes(nodes)
        }
        TraversalAction::Filter => cursor,
        TraversalAction::HasLabel => {
            let target_labels: Vec<&str> = step
                .labels
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|s| s.as_str())
                .collect();
            let nodes = require_nodes(cursor)?;
            let filtered = nodes
                .into_iter()
                .filter(|&n| {
                    let node_lbls = graph.node_labels(n);
                    target_labels
                        .iter()
                        .any(|tl| node_lbls.iter().any(|nl| nl == tl))
                })
                .collect();
            Cursor::Nodes(filtered)
        }
        TraversalAction::Dedup => match cursor {
            Cursor::Nodes(nodes) => {
                let mut seen = HashSet::new();
                let deduped = nodes.into_iter().filter(|n| seen.insert(*n)).collect();
                Cursor::Nodes(deduped)
            }
            Cursor::Edges(edges) => {
                let mut seen = HashSet::new();
                let deduped = edges.into_iter().filter(|e| seen.insert(*e)).collect();
                Cursor::Edges(deduped)
            }
        },
        TraversalAction::Limit => {
            let n = step.count.unwrap_or(usize::MAX);
            match cursor {
                Cursor::Nodes(nodes) => Cursor::Nodes(nodes.into_iter().take(n).collect()),
                Cursor::Edges(edges) => Cursor::Edges(edges.into_iter().take(n).collect()),
            }
        }
        TraversalAction::Skip => {
            let n = step.skip.unwrap_or(0);
            match cursor {
                Cursor::Nodes(nodes) => Cursor::Nodes(nodes.into_iter().skip(n).collect()),
                Cursor::Edges(edges) => Cursor::Edges(edges.into_iter().skip(n).collect()),
            }
        }
        TraversalAction::OrderBy => {
            let spec = step.order_by.as_ref().ok_or_else(|| {
                GraphError::InvalidCommand("OrderBy requires order_by spec".to_string())
            })?;
            apply_order_by(graph, cursor, spec)?
        }
        TraversalAction::Repeat => {
            let inner_steps = step.repeat_steps.as_deref().ok_or_else(|| {
                GraphError::InvalidCommand("Repeat requires repeat_steps".to_string())
            })?;
            let max = step.max_depth.unwrap_or(1);
            apply_repeat(graph, cursor, inner_steps, max, bindings)?
        }
    };

    // Apply simple filter
    let next = if let Some(filter) = &step.filter {
        apply_filter(graph, next, filter, bindings)?
    } else {
        next
    };

    // Apply compound filter
    let next = if let Some(cf) = &step.compound_filter {
        apply_compound_filter(graph, next, cf, bindings)?
    } else {
        next
    };

    // Apply custom filter
    let next = if let Some(cf) = &step.custom_filter {
        apply_custom_filter(graph, next, cf)?
    } else {
        next
    };

    // Apply substring match filter to all string properties
    if let Some(substr) = &step.any_key_contains {
        apply_any_key_contains(graph, next, substr)
    } else {
        Ok(next)
    }
}

/// Repeat: iterates inner_steps up to max_depth times.
/// Accumulates elements reached in each iteration and deduplicates at the end.
fn apply_repeat<G: GraphAccess>(
    graph: &G,
    cursor: Cursor,
    inner_steps: &[TraversalStep],
    max_depth: usize,
    bindings: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<Cursor, GraphError> {
    // Register initial cursor IDs as visited and add them to the accumulator
    let mut accumulated: Vec<NodeId> = match &cursor {
        Cursor::Nodes(ns) => ns.clone(),
        Cursor::Edges(_) => {
            return Err(GraphError::InvalidCommand(
                "Repeat must start from a Nodes cursor".to_string(),
            ))
        }
    };
    let mut frontier = cursor;

    for _ in 0..max_depth {
        // Apply inner_steps to advance to the next frontier
        let mut next = frontier.clone();
        for step in inner_steps {
            next = apply_step(graph, next, step, bindings)?;
        }

        let new_nodes = match &next {
            Cursor::Nodes(ns) => ns.clone(),
            Cursor::Edges(_) => break, // stop if cursor changed to Edges
        };

        if new_nodes.is_empty() {
            break;
        }

        accumulated.extend_from_slice(&new_nodes);
        frontier = Cursor::Nodes(new_nodes);
    }

    // Deduplicate and return
    let mut seen = HashSet::new();
    let deduped = accumulated
        .into_iter()
        .filter(|n| seen.insert(*n))
        .collect();
    Ok(Cursor::Nodes(deduped))
}

fn apply_order_by<G: GraphAccess>(
    graph: &G,
    cursor: Cursor,
    spec: &OrderBySpec,
) -> Result<Cursor, GraphError> {
    let asc = spec.direction == SortDirection::Asc;
    match cursor {
        Cursor::Nodes(mut nodes) => {
            nodes.sort_by(|&a, &b| {
                let va = graph
                    .node_properties(a)
                    .as_deref()
                    .and_then(|p| spec.key.resolve(p))
                    .cloned();
                let vb = graph
                    .node_properties(b)
                    .as_deref()
                    .and_then(|p| spec.key.resolve(p))
                    .cloned();
                let ord = cmp_option_values(va.as_ref(), vb.as_ref());
                if asc {
                    ord
                } else {
                    ord.reverse()
                }
            });
            Ok(Cursor::Nodes(nodes))
        }
        Cursor::Edges(mut edges) => {
            edges.sort_by(|&a, &b| {
                let va = graph
                    .edge_properties(a)
                    .as_deref()
                    .and_then(|p| spec.key.resolve(p))
                    .cloned();
                let vb = graph
                    .edge_properties(b)
                    .as_deref()
                    .and_then(|p| spec.key.resolve(p))
                    .cloned();
                let ord = cmp_option_values(va.as_ref(), vb.as_ref());
                if asc {
                    ord
                } else {
                    ord.reverse()
                }
            });
            Ok(Cursor::Edges(edges))
        }
    }
}

fn cmp_option_values(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => cmp_values(a, b).unwrap_or(std::cmp::Ordering::Equal),
    }
}

fn apply_filter<G: GraphAccess>(
    graph: &G,
    cursor: Cursor,
    filter: &Filter,
    bindings: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<Cursor, GraphError> {
    match cursor {
        Cursor::Nodes(nodes) => {
            let filtered = nodes
                .into_iter()
                .filter(|&n| {
                    let props = graph.node_properties(n);
                    eval_filter_on_props(
                        props.as_deref(),
                        &filter.property,
                        &filter.operator,
                        &filter.value,
                        bindings,
                    )
                })
                .collect::<Vec<_>>();
            Ok(Cursor::Nodes(filtered))
        }
        Cursor::Edges(edges) => {
            let filtered = edges
                .into_iter()
                .filter(|&e| {
                    let props = graph.edge_properties(e);
                    eval_filter_on_props(
                        props.as_deref(),
                        &filter.property,
                        &filter.operator,
                        &filter.value,
                        bindings,
                    )
                })
                .collect::<Vec<_>>();
            Ok(Cursor::Edges(filtered))
        }
    }
}

fn eval_filter_on_props(
    props: Option<&HashMap<String, Value>>,
    property: &PropertyPath,
    op: &FilterOperator,
    expected: &FilterValue,
    bindings: &std::collections::HashMap<String, Value>,
) -> bool {
    match op {
        FilterOperator::Exists => {
            props.is_some_and(|p| property.resolve(p).is_some_and(|v| !v.is_null()))
        }
        FilterOperator::IsNull => props
            .and_then(|p| property.resolve(p))
            .is_none_or(|v| v.is_null()),
        _ => {
            let actual = props.and_then(|p| property.resolve(p));
            let resolved = expected.resolve(props, bindings);
            match (actual, resolved) {
                (Some(a), Some(e)) => eval_filter(a, op, &e),
                _ => false,
            }
        }
    }
}

fn eval_filter(actual: &Value, op: &FilterOperator, expected: &Value) -> bool {
    match op {
        FilterOperator::Equals | FilterOperator::PropertyEquals => actual == expected,
        FilterOperator::NotEquals => actual != expected,
        FilterOperator::Contains => match (actual, expected) {
            (Value::String(a), Value::String(e)) => a.contains(e.as_str()),
            _ => false,
        },
        FilterOperator::StartsWith => match (actual, expected) {
            (Value::String(a), Value::String(e)) => a.starts_with(e.as_str()),
            _ => false,
        },
        FilterOperator::EndsWith => match (actual, expected) {
            (Value::String(a), Value::String(e)) => a.ends_with(e.as_str()),
            _ => false,
        },
        FilterOperator::Matches => match (actual, expected) {
            (Value::String(a), Value::String(pattern)) => {
                regex::Regex::new(pattern).is_ok_and(|re| re.is_match(a))
            }
            _ => false,
        },
        FilterOperator::GreaterThan | FilterOperator::PropertyGreaterThan => {
            cmp_values(actual, expected) == Some(std::cmp::Ordering::Greater)
        }
        FilterOperator::LessThan | FilterOperator::PropertyLessThan => {
            cmp_values(actual, expected) == Some(std::cmp::Ordering::Less)
        }
        FilterOperator::GreaterThanOrEquals => matches!(
            cmp_values(actual, expected),
            Some(std::cmp::Ordering::Greater) | Some(std::cmp::Ordering::Equal)
        ),
        FilterOperator::LessThanOrEquals => matches!(
            cmp_values(actual, expected),
            Some(std::cmp::Ordering::Less) | Some(std::cmp::Ordering::Equal)
        ),
        // Exists/IsNull are handled earlier in eval_filter_on_props
        FilterOperator::Exists => !actual.is_null(),
        FilterOperator::IsNull => actual.is_null(),
    }
}

fn eval_filter_expr<G: GraphAccess>(
    graph: &G,
    id: u64,
    expr: &FilterExpr,
    is_node: bool,
    bindings: &std::collections::HashMap<String, Value>,
) -> bool {
    match expr {
        FilterExpr::Simple(f) => {
            let props = if is_node {
                graph.node_properties(id)
            } else {
                graph.edge_properties(id)
            };
            eval_filter_on_props(
                props.as_deref(),
                &f.property,
                &f.operator,
                &f.value,
                bindings,
            )
        }
        FilterExpr::Compound(cf) => eval_compound(graph, id, cf, is_node, bindings),
    }
}

fn eval_compound<G: GraphAccess>(
    graph: &G,
    id: u64,
    cf: &CompoundFilter,
    is_node: bool,
    bindings: &std::collections::HashMap<String, Value>,
) -> bool {
    match cf {
        CompoundFilter::And { filters } => filters
            .iter()
            .all(|f| eval_filter_expr(graph, id, f, is_node, bindings)),
        CompoundFilter::Or { filters } => filters
            .iter()
            .any(|f| eval_filter_expr(graph, id, f, is_node, bindings)),
        CompoundFilter::Not { filter } => !eval_filter_expr(graph, id, filter, is_node, bindings),
    }
}

fn apply_custom_filter<G: GraphAccess>(
    graph: &G,
    cursor: Cursor,
    cf: &CustomFilter,
) -> Result<Cursor, GraphError> {
    match cursor {
        Cursor::Nodes(nodes) => {
            let candidates: Vec<(NodeId, HashMap<String, Value>)> = nodes
                .iter()
                .map(|&n| {
                    let props = graph
                        .node_properties(n)
                        .map(Cow::into_owned)
                        .unwrap_or_default();
                    (n, props)
                })
                .collect();
            let passed = graph.apply_custom_filter(&cf.callback_id, candidates);
            let passed_set: HashSet<NodeId> = passed.into_iter().collect();
            Ok(Cursor::Nodes(
                nodes
                    .into_iter()
                    .filter(|n| passed_set.contains(n))
                    .collect(),
            ))
        }
        Cursor::Edges(_) => Err(GraphError::InvalidCommand(
            "custom_filter is only supported for Nodes cursor".to_string(),
        )),
    }
}

fn apply_compound_filter<G: GraphAccess>(
    graph: &G,
    cursor: Cursor,
    cf: &CompoundFilter,
    bindings: &std::collections::HashMap<String, Value>,
) -> Result<Cursor, GraphError> {
    match cursor {
        Cursor::Nodes(nodes) => {
            let filtered = nodes
                .into_iter()
                .filter(|&n| eval_compound(graph, n, cf, true, bindings))
                .collect();
            Ok(Cursor::Nodes(filtered))
        }
        Cursor::Edges(edges) => {
            let filtered = edges
                .into_iter()
                .filter(|&e| eval_compound(graph, e, cf, false, bindings))
                .collect();
            Ok(Cursor::Edges(filtered))
        }
    }
}

/// Applies a substring-match filter to all string properties.
/// Only String/Json properties are checked; Int, Boolean, etc. are ignored.
fn apply_any_key_contains<G: GraphAccess>(
    graph: &G,
    cursor: Cursor,
    substr: &str,
) -> Result<Cursor, GraphError> {
    let substr_lower = substr.to_lowercase();
    match cursor {
        Cursor::Nodes(nodes) => {
            let filtered = nodes
                .into_iter()
                .filter(|&n| {
                    graph.node_properties(n).is_some_and(|props| {
                        props
                            .values()
                            .any(|v| value_contains_string(v, &substr_lower))
                    })
                })
                .collect();
            Ok(Cursor::Nodes(filtered))
        }
        Cursor::Edges(edges) => {
            let filtered = edges
                .into_iter()
                .filter(|&e| {
                    graph.edge_properties(e).is_some_and(|props| {
                        props
                            .values()
                            .any(|v| value_contains_string(v, &substr_lower))
                    })
                })
                .collect();
            Ok(Cursor::Edges(filtered))
        }
    }
}

/// Performs a case-insensitive substring check, recursing into JSON objects and arrays.
/// Only string *values* are matched; object keys are not searched. This lets keyword search
/// reach strings nested inside a `Json`-typed property (e.g. an aggregated `props` map).
fn value_contains_string(v: &Value, substr_lower: &str) -> bool {
    match v {
        Value::String(s) => s.to_lowercase().contains(substr_lower),
        Value::Array(items) => items
            .iter()
            .any(|item| value_contains_string(item, substr_lower)),
        Value::Object(map) => map
            .values()
            .any(|val| value_contains_string(val, substr_lower)),
        _ => false,
    }
}

fn cmp_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn collect<G: GraphAccess>(
    graph: &G,
    cursor: Cursor,
    spec: &CollectSpec,
) -> Result<CollectResult, GraphError> {
    match spec {
        CollectSpec::Count => {
            let count = match &cursor {
                Cursor::Nodes(ns) => ns.len(),
                Cursor::Edges(es) => es.len(),
            };
            Ok(CollectResult::Count(count))
        }

        CollectSpec::Exists => {
            let exists = match &cursor {
                Cursor::Nodes(ns) => !ns.is_empty(),
                Cursor::Edges(es) => !es.is_empty(),
            };
            Ok(CollectResult::Exists(exists))
        }

        CollectSpec::Nodes {
            properties,
            with_edges,
        } => collect_nodes(graph, cursor, properties, with_edges.as_ref()),

        CollectSpec::Edges { properties } => collect_edges(graph, cursor, properties),

        CollectSpec::Aggregate { functions } => {
            let node_ids = cursor_to_node_ids(graph, cursor);
            Ok(CollectResult::Aggregate(compute_aggregates(
                graph, &node_ids, functions,
            )))
        }

        CollectSpec::GroupBy { key, functions } => collect_group_by(graph, cursor, key, functions),

        CollectSpec::Path { node_specs } => collect_path(graph, cursor, node_specs),
    }
}

fn collect_nodes<G: GraphAccess>(
    graph: &G,
    cursor: Cursor,
    properties: &[String],
    with_edges: Option<&EdgeCollectSpec>,
) -> Result<CollectResult, GraphError> {
    let mut results = Vec::new();

    match cursor {
        Cursor::Nodes(node_ids) => {
            for node_id in node_ids {
                if let Some(props) = graph.node_properties(node_id) {
                    let row = pick_properties(&props, properties);
                    results.push(row);
                }
            }
        }
        Cursor::Edges(edge_ids) => {
            for edge_id in edge_ids {
                let Some(node_id) = graph.edge_to_node(edge_id) else {
                    continue;
                };
                let Some(props) = graph.node_properties(node_id) else {
                    continue;
                };
                let mut row = pick_properties(&props, properties);

                if let Some(edge_spec) = with_edges {
                    if let Some(label_filter) = &edge_spec.label {
                        if graph.edge_label(edge_id).as_deref() != Some(label_filter.as_str()) {
                            results.push(row);
                            continue;
                        }
                    }
                    if let Some(edge_props) = graph.edge_properties(edge_id) {
                        let prefix = "edge_";
                        let edge_picked = pick_properties(&edge_props, &edge_spec.properties);
                        for (k, v) in edge_picked {
                            row.insert(format!("{}{}", prefix, k), v);
                        }
                    }
                }
                results.push(row);
            }
        }
    }

    // Handle with_edges when cursor was Nodes (look up edges that led to these nodes is not
    // possible without context; with_edges on Nodes cursor is a no-op for edge props)
    Ok(CollectResult::Rows(results))
}

fn collect_edges<G: GraphAccess>(
    graph: &G,
    cursor: Cursor,
    properties: &[String],
) -> Result<CollectResult, GraphError> {
    let edge_ids = match cursor {
        Cursor::Edges(es) => es,
        Cursor::Nodes(_) => return Ok(CollectResult::Rows(vec![])),
    };
    let mut results = Vec::new();
    for edge_id in edge_ids {
        if let Some(props) = graph.edge_properties(edge_id) {
            results.push(pick_properties(&props, properties));
        }
    }
    Ok(CollectResult::Rows(results))
}

fn collect_group_by<G: GraphAccess>(
    graph: &G,
    cursor: Cursor,
    key: &GroupByKey,
    functions: &[AggregateFunction],
) -> Result<CollectResult, GraphError> {
    // Build (group_value → Vec<NodeId>) map
    let node_ids = cursor_to_node_ids(graph, cursor);

    let mut groups: std::collections::BTreeMap<String, Vec<NodeId>> =
        std::collections::BTreeMap::new();

    for node_id in node_ids {
        let group_val = match key {
            GroupByKey::Node { property } => graph
                .node_properties(node_id)
                .as_deref()
                .and_then(|p| property.resolve(p))
                .map(value_to_group_key)
                .unwrap_or_else(|| "null".to_string()),
            GroupByKey::Edge { .. } => "null".to_string(),
        };
        groups.entry(group_val).or_default().push(node_id);
    }

    let mut result = Vec::new();
    for (group_val, ids) in groups {
        let mut row = HashMap::new();
        row.insert("_group".to_string(), serde_json::Value::String(group_val));
        let agg = compute_aggregates(graph, &ids, functions);
        row.extend(agg);
        result.push(row);
    }

    Ok(CollectResult::Groups(result))
}

fn collect_path<G: GraphAccess>(
    graph: &G,
    cursor: Cursor,
    node_specs: &[PathNodeSpec],
) -> Result<CollectResult, GraphError> {
    let node_ids = cursor_to_node_ids(graph, cursor);
    let mut paths = Vec::new();

    for node_id in node_ids {
        let Some(props) = graph.node_properties(node_id) else {
            continue;
        };
        let label = graph.node_label(node_id).unwrap_or_default();
        let selected_props = node_specs
            .iter()
            .find(|s| s.label == label)
            .map(|s| s.properties.as_slice())
            .unwrap_or(&[]);
        let row = pick_properties(&props, selected_props);
        paths.push(vec![row]);
    }

    Ok(CollectResult::Path(paths))
}

fn cursor_to_node_ids<G: GraphAccess>(graph: &G, cursor: Cursor) -> Vec<NodeId> {
    match cursor {
        Cursor::Nodes(ns) => ns,
        Cursor::Edges(es) => es.iter().filter_map(|&e| graph.edge_to_node(e)).collect(),
    }
}

fn pick_properties(props: &HashMap<String, Value>, keys: &[String]) -> HashMap<String, Value> {
    if keys.is_empty() {
        return props.clone();
    }
    keys.iter()
        .filter_map(|k| props.get(k).map(|v| (k.clone(), v.clone())))
        .collect()
}

fn compute_aggregates<G: GraphAccess>(
    graph: &G,
    node_ids: &[NodeId],
    functions: &[AggregateFunction],
) -> HashMap<String, Value> {
    let mut result = HashMap::new();

    for func in functions {
        let output_key = func.output_key();
        // Resolves this function's property path on a node, if a property is set.
        let resolve_on = |n: NodeId| -> Option<Value> {
            let prop = func.property.as_ref()?;
            let props = graph.node_properties(n)?;
            prop.resolve(&props).cloned()
        };
        let value = match &func.func {
            AggregateFn::Count => Value::Number(node_ids.len().into()),
            AggregateFn::Sum => {
                let sum: f64 = node_ids
                    .iter()
                    .filter_map(|&n| resolve_on(n)?.as_f64())
                    .sum();
                json_number(sum)
            }
            AggregateFn::Avg => {
                let values: Vec<f64> = node_ids
                    .iter()
                    .filter_map(|&n| resolve_on(n)?.as_f64())
                    .collect();
                if values.is_empty() {
                    Value::Null
                } else {
                    json_number(values.iter().sum::<f64>() / values.len() as f64)
                }
            }
            AggregateFn::Min => node_ids
                .iter()
                .filter_map(|&n| resolve_on(n))
                .min_by(|a, b| cmp_values(a, b).unwrap_or(std::cmp::Ordering::Equal))
                .unwrap_or(Value::Null),
            AggregateFn::Max => node_ids
                .iter()
                .filter_map(|&n| resolve_on(n))
                .max_by(|a, b| cmp_values(a, b).unwrap_or(std::cmp::Ordering::Equal))
                .unwrap_or(Value::Null),
        };
        result.insert(output_key, value);
    }

    result
}

fn value_to_group_key(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        _ => v.to_string(),
    }
}

fn json_number(f: f64) -> Value {
    serde_json::Number::from_f64(f)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn require_nodes(cursor: Cursor) -> Result<Vec<NodeId>, GraphError> {
    match cursor {
        Cursor::Nodes(ns) => Ok(ns),
        Cursor::Edges(_) => Err(GraphError::InvalidCommand(
            "expected node cursor, got edge cursor".to_string(),
        )),
    }
}

fn require_edges(cursor: Cursor) -> Result<Vec<EdgeId>, GraphError> {
    match cursor {
        Cursor::Edges(es) => Ok(es),
        Cursor::Nodes(_) => Err(GraphError::InvalidCommand(
            "expected edge cursor, got node cursor".to_string(),
        )),
    }
}

// ---- In-memory graph implementation (for tests) ----

#[derive(Default)]
pub struct InMemoryGraph {
    nodes: HashMap<NodeId, NodeData>,
    edges: HashMap<EdgeId, EdgeData>,
    next_node_id: NodeId,
    next_edge_id: EdgeId,
}

impl InMemoryGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_node(&mut self, label: &str, properties: HashMap<String, Value>) -> NodeId {
        self.add_node_with_labels(label, vec![], properties)
    }

    pub fn add_node_with_labels(
        &mut self,
        label: &str,
        extra_labels: Vec<&str>,
        properties: HashMap<String, Value>,
    ) -> NodeId {
        let id = self.next_node_id;
        self.next_node_id += 1;
        self.nodes.insert(
            id,
            NodeData {
                id,
                label: label.to_string(),
                extra_labels: extra_labels.into_iter().map(|s| s.to_string()).collect(),
                properties,
            },
        );
        id
    }

    pub fn add_edge(
        &mut self,
        label: &str,
        from: NodeId,
        to: NodeId,
        properties: HashMap<String, Value>,
    ) -> EdgeId {
        let id = self.next_edge_id;
        self.next_edge_id += 1;
        self.edges.insert(
            id,
            EdgeData {
                id,
                label: label.to_string(),
                from,
                to,
                properties,
            },
        );
        id
    }
}

impl GraphAccess for InMemoryGraph {
    fn find_node(&self, label: &str, key: &str, value: &Value) -> Option<NodeId> {
        self.nodes
            .values()
            .find(|n| n.label == label && n.properties.get(key) == Some(value))
            .map(|n| n.id)
    }

    fn node_properties(&self, id: NodeId) -> Option<Cow<'_, HashMap<String, Value>>> {
        self.nodes.get(&id).map(|n| Cow::Borrowed(&n.properties))
    }

    fn node_label(&self, id: NodeId) -> Option<String> {
        self.nodes.get(&id).map(|n| n.label.clone())
    }

    fn node_labels(&self, id: NodeId) -> Vec<String> {
        match self.nodes.get(&id) {
            None => vec![],
            Some(n) => {
                let mut labels: Vec<String> = vec![n.label.clone()];
                labels.extend(n.extra_labels.iter().cloned());
                labels
            }
        }
    }

    fn all_nodes_of_label(&self, label: &str) -> Vec<NodeId> {
        if label.is_empty() {
            return self.nodes.keys().copied().collect();
        }
        self.nodes
            .values()
            .filter(|n| n.label == label || n.extra_labels.iter().any(|l| l == label))
            .map(|n| n.id)
            .collect()
    }

    fn all_edges_of_label(&self, label: &str) -> Vec<EdgeId> {
        if label.is_empty() {
            return self.edges.keys().copied().collect();
        }
        self.edges
            .values()
            .filter(|e| e.label == label)
            .map(|e| e.id)
            .collect()
    }

    fn out_edges(&self, node_id: NodeId, label: Option<&str>) -> Vec<EdgeId> {
        self.edges
            .values()
            .filter(|e| e.from == node_id && label.is_none_or(|l| e.label == l))
            .map(|e| e.id)
            .collect()
    }

    fn in_edges(&self, node_id: NodeId, label: Option<&str>) -> Vec<EdgeId> {
        self.edges
            .values()
            .filter(|e| e.to == node_id && label.is_none_or(|l| e.label == l))
            .map(|e| e.id)
            .collect()
    }

    fn edge_to_node(&self, edge_id: EdgeId) -> Option<NodeId> {
        self.edges.get(&edge_id).map(|e| e.to)
    }

    fn edge_from_node(&self, edge_id: EdgeId) -> Option<NodeId> {
        self.edges.get(&edge_id).map(|e| e.from)
    }

    fn edge_label(&self, id: EdgeId) -> Option<String> {
        self.edges.get(&id).map(|e| e.label.clone())
    }

    fn edge_properties(&self, id: EdgeId) -> Option<Cow<'_, HashMap<String, Value>>> {
        self.edges.get(&id).map(|e| Cow::Borrowed(&e.properties))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traversal::command::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn make_graph() -> InMemoryGraph {
        let mut g = InMemoryGraph::new();
        let u1 = g.add_node(
            "User",
            HashMap::from([("id".into(), json!("u1")), ("name".into(), json!("Alice"))]),
        );
        let u2 = g.add_node(
            "User",
            HashMap::from([("id".into(), json!("u2")), ("name".into(), json!("Bob"))]),
        );
        let p1 = g.add_node(
            "Project",
            HashMap::from([
                ("id".into(), json!("p1")),
                ("title".into(), json!("Alpha")),
                ("isArchived".into(), json!(false)),
            ]),
        );
        let p2 = g.add_node(
            "Project",
            HashMap::from([
                ("id".into(), json!("p2")),
                ("title".into(), json!("Beta")),
                ("isArchived".into(), json!(true)),
            ]),
        );
        g.add_edge(
            "OWNS",
            u1,
            p1,
            HashMap::from([("role".into(), json!("owner"))]),
        );
        g.add_edge(
            "OWNS",
            u1,
            p2,
            HashMap::from([("role".into(), json!("member"))]),
        );
        g.add_edge(
            "OWNS",
            u2,
            p1,
            HashMap::from([("role".into(), json!("viewer"))]),
        );
        g
    }

    fn cmd(steps: Vec<TraversalStep>, props: Vec<&str>) -> TraversalCommand {
        TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "Node".into(),
                label: "User".into(),
                key: "id".into(),
                value: json!("u1"),
            },
            steps,
            collect: CollectSpec::Nodes {
                properties: props.into_iter().map(|s| s.to_string()).collect(),
                with_edges: None,
            },
        }
    }

    fn step(action: TraversalAction, label: &str) -> TraversalStep {
        TraversalStep {
            action,
            label: Some(label.to_string()),
            labels: None,
            filter: None,
            compound_filter: None,
            repeat_steps: None,
            max_depth: None,
            custom_filter: None,
            any_key_contains: None,
            count: None,
            skip: None,
            order_by: None,
        }
    }

    fn step_with_filter(action: TraversalAction, label: &str, f: Filter) -> TraversalStep {
        TraversalStep {
            action,
            label: Some(label.to_string()),
            labels: None,
            filter: Some(f),
            compound_filter: None,
            repeat_steps: None,
            max_depth: None,
            custom_filter: None,
            any_key_contains: None,
            count: None,
            skip: None,
            order_by: None,
        }
    }

    fn rows(result: CollectResult) -> Vec<HashMap<String, Value>> {
        match result {
            CollectResult::Rows(r) => r,
            other => panic!("expected Rows, got {:?}", other),
        }
    }

    fn count(result: CollectResult) -> usize {
        match result {
            CollectResult::Count(n) => n,
            other => panic!("expected Count, got {:?}", other),
        }
    }

    #[test]
    fn traverse_out_edges_then_out_nodes() {
        let g = make_graph();
        let result = rows(
            execute(
                &g,
                &cmd(
                    vec![
                        step(TraversalAction::OutEdges, "OWNS"),
                        step(TraversalAction::OutNodes, "Project"),
                    ],
                    vec!["id"],
                ),
            )
            .unwrap(),
        );
        let ids: HashSet<&str> = result.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, HashSet::from(["p1", "p2"]));
    }

    #[test]
    fn filter_removes_archived_projects() {
        let g = make_graph();
        let filter = Filter {
            property: "isArchived".into(),
            operator: FilterOperator::Equals,
            value: FilterValue::Literal(json!(false)),
        };
        let result = rows(
            execute(
                &g,
                &cmd(
                    vec![
                        step(TraversalAction::OutEdges, "OWNS"),
                        step_with_filter(TraversalAction::OutNodes, "Project", filter),
                    ],
                    vec!["id"],
                ),
            )
            .unwrap(),
        );
        let ids: HashSet<&str> = result.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, HashSet::from(["p1"]));
    }

    #[test]
    fn traverse_in_edges_then_in_nodes() {
        let g = make_graph();
        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "Node".into(),
                label: "Project".into(),
                key: "id".into(),
                value: json!("p1"),
            },
            steps: vec![
                step(TraversalAction::InEdges, "OWNS"),
                step(TraversalAction::InNodes, "User"),
            ],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        let ids: HashSet<&str> = result.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, HashSet::from(["u1", "u2"]));
    }

    #[test]
    fn start_node_not_found_returns_error() {
        let g = make_graph();
        let c = cmd(vec![], vec!["id"]);
        let bad = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            start: StartSpec {
                value: json!("nonexistent"),
                ..c.start
            },
            ..c
        };
        assert!(execute(&g, &bad).is_err());
    }

    #[test]
    fn has_label_matches_primary_label() {
        let mut g = InMemoryGraph::new();
        let u1 = g.add_node(
            "User",
            HashMap::from([("id".into(), json!("u1")), ("name".into(), json!("Alice"))]),
        );
        let _u2 = g.add_node(
            "Project",
            HashMap::from([("id".into(), json!("p1")), ("title".into(), json!("Alpha"))]),
        );

        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "Node".into(),
                label: "User".into(),
                key: "id".into(),
                value: json!("u1"),
            },
            steps: vec![TraversalStep {
                action: TraversalAction::HasLabel,
                label: None,
                labels: Some(vec!["User".to_string()]),
                filter: None,
                compound_filter: None,
                repeat_steps: None,
                max_depth: None,
                custom_filter: None,
                any_key_contains: None,
                count: None,
                skip: None,
                order_by: None,
            }],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], json!("u1"));
        let _ = u1;
    }

    #[test]
    fn has_label_matches_extra_label() {
        let mut g = InMemoryGraph::new();
        g.add_node_with_labels(
            "User",
            vec!["Admin"],
            HashMap::from([("id".into(), json!("u1")), ("name".into(), json!("Alice"))]),
        );
        g.add_node(
            "User",
            HashMap::from([("id".into(), json!("u2")), ("name".into(), json!("Bob"))]),
        );

        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "Node".into(),
                label: "User".into(),
                key: "id".into(),
                value: json!("u1"),
            },
            steps: vec![TraversalStep {
                action: TraversalAction::HasLabel,
                label: None,
                labels: Some(vec!["Admin".to_string()]),
                filter: None,
                compound_filter: None,
                repeat_steps: None,
                max_depth: None,
                custom_filter: None,
                any_key_contains: None,
                count: None,
                skip: None,
                order_by: None,
            }],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], json!("u1"));
    }

    #[test]
    fn has_label_filters_out_non_matching() {
        let mut g = InMemoryGraph::new();
        g.add_node(
            "User",
            HashMap::from([("id".into(), json!("u1")), ("name".into(), json!("Alice"))]),
        );

        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "Node".into(),
                label: "User".into(),
                key: "id".into(),
                value: json!("u1"),
            },
            steps: vec![TraversalStep {
                action: TraversalAction::HasLabel,
                label: None,
                labels: Some(vec!["Admin".to_string()]),
                filter: None,
                compound_filter: None,
                repeat_steps: None,
                max_depth: None,
                custom_filter: None,
                any_key_contains: None,
                count: None,
                skip: None,
                order_by: None,
            }],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        assert!(result.is_empty());
    }

    // ---- Step 25: new action tests ----

    fn make_linear_graph() -> (InMemoryGraph, NodeId, NodeId, NodeId) {
        // n1 --E--> n2 --E--> n3
        let mut g = InMemoryGraph::new();
        let n1 = g.add_node(
            "N",
            HashMap::from([("id".into(), json!("n1")), ("score".into(), json!(30))]),
        );
        let n2 = g.add_node(
            "N",
            HashMap::from([("id".into(), json!("n2")), ("score".into(), json!(10))]),
        );
        let n3 = g.add_node(
            "N",
            HashMap::from([("id".into(), json!("n3")), ("score".into(), json!(20))]),
        );
        g.add_edge("E", n1, n2, HashMap::new());
        g.add_edge("E", n1, n3, HashMap::new());
        (g, n1, n2, n3)
    }

    fn simple_cmd(start_id: &str, steps: Vec<TraversalStep>) -> TraversalCommand {
        TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "Node".into(),
                label: "N".into(),
                key: "id".into(),
                value: json!(start_id),
            },
            steps,
            collect: CollectSpec::Nodes {
                properties: vec!["id".into(), "score".into()],
                with_edges: None,
            },
        }
    }

    fn simple_step(action: TraversalAction) -> TraversalStep {
        TraversalStep {
            action,
            ..Default::default()
        }
    }

    fn labeled_step(action: TraversalAction, label: &str) -> TraversalStep {
        TraversalStep {
            action,
            label: Some(label.into()),
            labels: None,
            filter: None,
            count: None,
            skip: None,
            order_by: None,
            compound_filter: None,
            repeat_steps: None,
            max_depth: None,
            custom_filter: None,
            any_key_contains: None,
        }
    }

    #[test]
    fn both_edges_returns_out_and_in() {
        // Setup: n1 --E--> n2, n3 --E--> n1; test BothEdges on n1
        let mut g = InMemoryGraph::new();
        let n1 = g.add_node("N", HashMap::from([("id".into(), json!("n1"))]));
        let n2 = g.add_node("N", HashMap::from([("id".into(), json!("n2"))]));
        let n3 = g.add_node("N", HashMap::from([("id".into(), json!("n3"))]));
        let e_out = g.add_edge("E", n1, n2, HashMap::new());
        let e_in = g.add_edge("E", n3, n1, HashMap::new());
        let _ = (n2, n3);

        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "Node".into(),
                label: "N".into(),
                key: "id".into(),
                value: json!("n1"),
            },
            steps: vec![labeled_step(TraversalAction::BothEdges, "E")],
            collect: CollectSpec::Nodes {
                properties: vec![],
                with_edges: None,
            },
        };
        // BothEdges returns Cursor::Edges → edge_ids include e_out and e_in
        // collect converts Edges → OutNodes, but here we verify edge Count directly
        let c_count = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            collect: CollectSpec::Count,
            ..c
        };
        assert_eq!(count(execute(&g, &c_count).unwrap()), 2);
        let _ = (e_out, e_in);
    }

    #[test]
    fn both_nodes_returns_both_endpoints() {
        // n1 --E--> n2: both n1 and n2 are returned
        let mut g = InMemoryGraph::new();
        let n1 = g.add_node("N", HashMap::from([("id".into(), json!("n1"))]));
        let n2 = g.add_node("N", HashMap::from([("id".into(), json!("n2"))]));
        let _e = g.add_edge("E", n1, n2, HashMap::new());
        let _ = n2;

        let c = simple_cmd(
            "n1",
            vec![
                labeled_step(TraversalAction::OutEdges, "E"),
                simple_step(TraversalAction::BothNodes),
            ],
        );
        let result = rows(execute(&g, &c).unwrap());
        let ids: HashSet<&str> = result.iter().map(|r| r["id"].as_str().unwrap()).collect();
        // BothNodes returns both to_node and from_node → n2 and n1
        assert!(ids.contains("n1") || ids.contains("n2"));
    }

    #[test]
    fn dedup_removes_duplicates() {
        // Two duplicate edges from n1 to n2
        let mut g = InMemoryGraph::new();
        let n1 = g.add_node("N", HashMap::from([("id".into(), json!("n1"))]));
        let n2 = g.add_node("N", HashMap::from([("id".into(), json!("n2"))]));
        g.add_edge("E", n1, n2, HashMap::new());
        g.add_edge("E", n1, n2, HashMap::new()); // second edge to the same target
        let _ = n2;

        let c = simple_cmd(
            "n1",
            vec![
                labeled_step(TraversalAction::OutEdges, "E"),
                labeled_step(TraversalAction::OutNodes, "N"),
                simple_step(TraversalAction::Dedup),
            ],
        );
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], json!("n2"));
    }

    #[test]
    fn limit_truncates_results() {
        let (g, n1, _, _) = make_linear_graph();
        let _ = n1;
        let c = simple_cmd(
            "n1",
            vec![
                labeled_step(TraversalAction::OutEdges, "E"),
                labeled_step(TraversalAction::OutNodes, "N"),
                TraversalStep {
                    action: TraversalAction::Limit,
                    label: None,
                    labels: None,
                    filter: None,
                    count: Some(1),
                    skip: None,
                    order_by: None,
                    compound_filter: None,
                    repeat_steps: None,
                    max_depth: None,
                    custom_filter: None,
                    any_key_contains: None,
                },
            ],
        );
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn skip_and_limit_pagination() {
        let (g, n1, _, _) = make_linear_graph();
        let _ = n1;
        // OutNodes of n1 are n2 and n3 (order undefined, but 2 results expected)
        let c = simple_cmd(
            "n1",
            vec![
                labeled_step(TraversalAction::OutEdges, "E"),
                labeled_step(TraversalAction::OutNodes, "N"),
                simple_step(TraversalAction::Dedup),
                TraversalStep {
                    action: TraversalAction::OrderBy,
                    label: None,
                    labels: None,
                    filter: None,
                    compound_filter: None,
                    count: None,
                    skip: None,
                    order_by: Some(crate::traversal::command::OrderBySpec {
                        key: "id".into(),
                        direction: crate::traversal::command::SortDirection::Asc,
                    }),
                    repeat_steps: None,
                    max_depth: None,
                    custom_filter: None,
                    any_key_contains: None,
                },
                TraversalStep {
                    action: TraversalAction::Skip,
                    label: None,
                    labels: None,
                    filter: None,
                    count: None,
                    skip: Some(1),
                    order_by: None,
                    compound_filter: None,
                    repeat_steps: None,
                    max_depth: None,
                    custom_filter: None,
                    any_key_contains: None,
                },
                TraversalStep {
                    action: TraversalAction::Limit,
                    label: None,
                    labels: None,
                    filter: None,
                    count: Some(1),
                    skip: None,
                    order_by: None,
                    compound_filter: None,
                    repeat_steps: None,
                    max_depth: None,
                    custom_filter: None,
                    any_key_contains: None,
                },
            ],
        );
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
        // Sorted by id asc, Skip(1) → "n3"
        assert_eq!(result[0]["id"], json!("n3"));
    }

    #[test]
    fn order_by_asc() {
        let (g, n1, _, _) = make_linear_graph();
        let _ = n1;
        let c = simple_cmd(
            "n1",
            vec![
                labeled_step(TraversalAction::OutEdges, "E"),
                labeled_step(TraversalAction::OutNodes, "N"),
                simple_step(TraversalAction::Dedup),
                TraversalStep {
                    action: TraversalAction::OrderBy,
                    label: None,
                    labels: None,
                    filter: None,
                    compound_filter: None,
                    count: None,
                    skip: None,
                    order_by: Some(crate::traversal::command::OrderBySpec {
                        key: "score".into(),
                        direction: crate::traversal::command::SortDirection::Asc,
                    }),
                    repeat_steps: None,
                    max_depth: None,
                    custom_filter: None,
                    any_key_contains: None,
                },
            ],
        );
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 2);
        // score ascending: n2(10) < n3(20)
        assert_eq!(result[0]["id"], json!("n2"));
        assert_eq!(result[1]["id"], json!("n3"));
    }

    #[test]
    fn order_by_desc() {
        let (g, n1, _, _) = make_linear_graph();
        let _ = n1;
        let c = simple_cmd(
            "n1",
            vec![
                labeled_step(TraversalAction::OutEdges, "E"),
                labeled_step(TraversalAction::OutNodes, "N"),
                simple_step(TraversalAction::Dedup),
                TraversalStep {
                    action: TraversalAction::OrderBy,
                    label: None,
                    labels: None,
                    filter: None,
                    compound_filter: None,
                    count: None,
                    skip: None,
                    order_by: Some(crate::traversal::command::OrderBySpec {
                        key: "score".into(),
                        direction: crate::traversal::command::SortDirection::Desc,
                    }),
                    repeat_steps: None,
                    max_depth: None,
                    custom_filter: None,
                    any_key_contains: None,
                },
            ],
        );
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 2);
        // score descending: n3(20) > n2(10)
        assert_eq!(result[0]["id"], json!("n3"));
        assert_eq!(result[1]["id"], json!("n2"));
    }

    #[test]
    fn count_collect_type() {
        let (g, n1, _, _) = make_linear_graph();
        let _ = n1;
        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "Node".into(),
                label: "N".into(),
                key: "id".into(),
                value: json!("n1"),
            },
            steps: vec![
                labeled_step(TraversalAction::OutEdges, "E"),
                labeled_step(TraversalAction::OutNodes, "N"),
                simple_step(TraversalAction::Dedup),
            ],
            collect: CollectSpec::Count,
        };
        assert_eq!(count(execute(&g, &c).unwrap()), 2);
    }

    // ---- Step 26: filter operator extension tests ----

    fn name_graph() -> InMemoryGraph {
        let mut g = InMemoryGraph::new();
        g.add_node(
            "N",
            HashMap::from([("id".into(), json!("a1")), ("name".into(), json!("Alice"))]),
        );
        g.add_node(
            "N",
            HashMap::from([
                ("id".into(), json!("a2")),
                ("name".into(), json!("Alexander")),
            ]),
        );
        g.add_node(
            "N",
            HashMap::from([("id".into(), json!("b1")), ("name".into(), json!("Bob"))]),
        );
        g.add_node(
            "N",
            HashMap::from([
                ("id".into(), json!("bn")),
                ("name".into(), json!("Brandon")),
            ]),
        );
        g
    }

    fn filter_cmd(start_id: &str, f: Filter) -> TraversalCommand {
        TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "Node".into(),
                label: "N".into(),
                key: "id".into(),
                value: json!(start_id),
            },
            steps: vec![TraversalStep {
                action: TraversalAction::Filter,
                label: None,
                labels: None,
                filter: Some(f),
                compound_filter: None,
                repeat_steps: None,
                max_depth: None,
                custom_filter: None,
                any_key_contains: None,
                count: None,
                skip: None,
                order_by: None,
            }],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        }
    }

    #[test]
    fn starts_with_matches() {
        let g = name_graph();
        // "Alice" starts with "Al" → match
        let result = rows(
            execute(
                &g,
                &filter_cmd(
                    "a1",
                    Filter {
                        property: "name".into(),
                        operator: FilterOperator::StartsWith,
                        value: FilterValue::Literal(json!("Al")),
                    },
                ),
            )
            .unwrap(),
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], json!("a1"));
    }

    #[test]
    fn starts_with_no_match() {
        let g = name_graph();
        // "Bob" does not start with "Al" → empty
        let result = rows(
            execute(
                &g,
                &filter_cmd(
                    "b1",
                    Filter {
                        property: "name".into(),
                        operator: FilterOperator::StartsWith,
                        value: FilterValue::Literal(json!("Al")),
                    },
                ),
            )
            .unwrap(),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn ends_with_matches() {
        let g = name_graph();
        // "Alice" ends with "ce" → match
        let result = rows(
            execute(
                &g,
                &filter_cmd(
                    "a1",
                    Filter {
                        property: "name".into(),
                        operator: FilterOperator::EndsWith,
                        value: FilterValue::Literal(json!("ce")),
                    },
                ),
            )
            .unwrap(),
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], json!("a1"));
    }

    fn compound_cmd(start_id: &str, cf: CompoundFilter) -> TraversalCommand {
        TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "Node".into(),
                label: "N".into(),
                key: "id".into(),
                value: json!(start_id),
            },
            steps: vec![TraversalStep {
                action: TraversalAction::Filter,
                label: None,
                labels: None,
                filter: None,
                compound_filter: Some(cf),
                count: None,
                skip: None,
                order_by: None,
                repeat_steps: None,
                max_depth: None,
                custom_filter: None,
                any_key_contains: None,
            }],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        }
    }

    #[test]
    fn or_filter_matches_either() {
        let g = name_graph();
        // name of "Alice" is either "Alice" or "Alexander" → matches one of them
        let cf = CompoundFilter::Or {
            filters: vec![
                FilterExpr::Simple(Filter {
                    property: "name".into(),
                    operator: FilterOperator::Equals,
                    value: FilterValue::Literal(json!("Alice")),
                }),
                FilterExpr::Simple(Filter {
                    property: "name".into(),
                    operator: FilterOperator::Equals,
                    value: FilterValue::Literal(json!("Alexander")),
                }),
            ],
        };
        let result = rows(execute(&g, &compound_cmd("a1", cf)).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], json!("a1"));
    }

    #[test]
    fn or_filter_no_match() {
        let g = name_graph();
        // "Bob" is neither "Alice" nor "Alexander" → empty
        let cf = CompoundFilter::Or {
            filters: vec![
                FilterExpr::Simple(Filter {
                    property: "name".into(),
                    operator: FilterOperator::Equals,
                    value: FilterValue::Literal(json!("Alice")),
                }),
                FilterExpr::Simple(Filter {
                    property: "name".into(),
                    operator: FilterOperator::Equals,
                    value: FilterValue::Literal(json!("Alexander")),
                }),
            ],
        };
        let result = rows(execute(&g, &compound_cmd("b1", cf)).unwrap());
        assert!(result.is_empty());
    }

    #[test]
    fn not_filter_inverts() {
        let g = name_graph();
        // "Alice" != "Bob" → Not(Equals("Bob")) = true
        let cf = CompoundFilter::Not {
            filter: Box::new(FilterExpr::Simple(Filter {
                property: "name".into(),
                operator: FilterOperator::Equals,
                value: FilterValue::Literal(json!("Bob")),
            })),
        };
        let result = rows(execute(&g, &compound_cmd("a1", cf)).unwrap());
        assert_eq!(result.len(), 1);

        // "Bob" == "Bob" → Not(Equals("Bob")) = false → empty
        let cf2 = CompoundFilter::Not {
            filter: Box::new(FilterExpr::Simple(Filter {
                property: "name".into(),
                operator: FilterOperator::Equals,
                value: FilterValue::Literal(json!("Bob")),
            })),
        };
        let result2 = rows(execute(&g, &compound_cmd("b1", cf2)).unwrap());
        assert!(result2.is_empty());
    }

    // ---- #4 AllNodes start ----

    #[test]
    fn all_nodes_start_returns_all_of_label() {
        let g = make_graph();
        // Retrieve all nodes with the User label via AllNodes
        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "AllNodes".into(),
                label: "User".into(),
                key: String::new(),
                value: json!(null),
            },
            steps: vec![],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        let ids: HashSet<&str> = result.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, HashSet::from(["u1", "u2"]));
    }

    #[test]
    fn all_nodes_start_with_filter() {
        let g = make_graph();
        // AllNodes → Filter: keep only archived projects
        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "AllNodes".into(),
                label: "Project".into(),
                key: String::new(),
                value: json!(null),
            },
            steps: vec![step_with_filter(
                TraversalAction::Filter,
                "",
                Filter {
                    property: "isArchived".into(),
                    operator: FilterOperator::Equals,
                    value: FilterValue::Literal(json!(true)),
                },
            )],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], json!("p2"));
    }

    // ---- #5 AllEdges start ----

    #[test]
    fn all_edges_start_returns_all_edges() {
        let g = make_graph();
        // make_graph has 3 OWNS edges; AllEdges with empty label returns all.
        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "AllEdges".into(),
                label: String::new(),
                key: String::new(),
                value: json!(null),
            },
            steps: vec![],
            collect: CollectSpec::Edges { properties: vec![] },
        };
        let result = execute(&g, &c).unwrap();
        let rows = match result {
            CollectResult::Rows(r) => r,
            _ => panic!("expected Rows"),
        };
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn all_edges_start_filters_by_label() {
        let g = make_graph();
        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "AllEdges".into(),
                label: "OWNS".into(),
                key: String::new(),
                value: json!(null),
            },
            steps: vec![],
            collect: CollectSpec::Edges { properties: vec![] },
        };
        let result = execute(&g, &c).unwrap();
        let rows = match result {
            CollectResult::Rows(r) => r,
            _ => panic!("expected Rows"),
        };
        assert_eq!(rows.len(), 3);
    }

    // ---- #6 Repeat ----

    #[test]
    fn repeat_traverses_chain() {
        // Chain: n1 -> n2 -> n3
        let mut g = InMemoryGraph::new();
        let n1 = g.add_node("N", HashMap::from([("id".into(), json!("n1"))]));
        let n2 = g.add_node("N", HashMap::from([("id".into(), json!("n2"))]));
        let n3 = g.add_node("N", HashMap::from([("id".into(), json!("n3"))]));
        g.add_edge("E", n1, n2, HashMap::new());
        g.add_edge("E", n2, n3, HashMap::new());
        let _ = (n2, n3);

        let inner = vec![
            TraversalStep {
                action: TraversalAction::OutEdges,
                label: Some("E".into()),
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
            },
            TraversalStep {
                action: TraversalAction::OutNodes,
                label: Some("N".into()),
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
            },
        ];

        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "Node".into(),
                label: "N".into(),
                key: "id".into(),
                value: json!("n1"),
            },
            steps: vec![TraversalStep {
                action: TraversalAction::Repeat,
                label: None,
                labels: None,
                filter: None,
                compound_filter: None,
                custom_filter: None,
                any_key_contains: None,
                count: None,
                skip: None,
                order_by: None,
                repeat_steps: Some(inner),
                max_depth: Some(2),
            }],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };

        let result = rows(execute(&g, &c).unwrap());
        // n1 (start), n2 (depth=1), n3 (depth=2) are all included
        let ids: HashSet<&str> = result.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert!(ids.contains("n1"));
        assert!(ids.contains("n2"));
        assert!(ids.contains("n3"));
    }

    #[test]
    fn repeat_respects_max_depth() {
        // n1 -> n2 -> n3 with max_depth=1: n3 is not reached
        let mut g = InMemoryGraph::new();
        let n1 = g.add_node("N", HashMap::from([("id".into(), json!("n1"))]));
        let n2 = g.add_node("N", HashMap::from([("id".into(), json!("n2"))]));
        let n3 = g.add_node("N", HashMap::from([("id".into(), json!("n3"))]));
        g.add_edge("E", n1, n2, HashMap::new());
        g.add_edge("E", n2, n3, HashMap::new());
        let _ = (n2, n3);

        let inner = vec![
            TraversalStep {
                action: TraversalAction::OutEdges,
                label: Some("E".into()),
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
            },
            TraversalStep {
                action: TraversalAction::OutNodes,
                label: Some("N".into()),
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
            },
        ];

        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "Node".into(),
                label: "N".into(),
                key: "id".into(),
                value: json!("n1"),
            },
            steps: vec![TraversalStep {
                action: TraversalAction::Repeat,
                label: None,
                labels: None,
                filter: None,
                compound_filter: None,
                custom_filter: None,
                any_key_contains: None,
                count: None,
                skip: None,
                order_by: None,
                repeat_steps: Some(inner),
                max_depth: Some(1),
            }],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };

        let result = rows(execute(&g, &c).unwrap());
        let ids: HashSet<&str> = result.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert!(ids.contains("n1"));
        assert!(ids.contains("n2"));
        assert!(!ids.contains("n3")); // depth=1, so n3 is not reached
    }

    // ---- #3 And filter ----

    #[test]
    fn and_filter_requires_all() {
        let g = name_graph();
        // "Alice": name starts_with "Al" AND ends_with "ce" → both match
        let cf = CompoundFilter::And {
            filters: vec![
                FilterExpr::Simple(Filter {
                    property: "name".into(),
                    operator: FilterOperator::StartsWith,
                    value: FilterValue::Literal(json!("Al")),
                }),
                FilterExpr::Simple(Filter {
                    property: "name".into(),
                    operator: FilterOperator::EndsWith,
                    value: FilterValue::Literal(json!("ce")),
                }),
            ],
        };
        let result = rows(execute(&g, &compound_cmd("a1", cf)).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], json!("a1"));
    }

    #[test]
    fn and_filter_partial_match_fails() {
        let g = name_graph();
        // "Alexander": starts_with "Al" is true but ends_with "ce" is false → And overall is false
        let cf = CompoundFilter::And {
            filters: vec![
                FilterExpr::Simple(Filter {
                    property: "name".into(),
                    operator: FilterOperator::StartsWith,
                    value: FilterValue::Literal(json!("Al")),
                }),
                FilterExpr::Simple(Filter {
                    property: "name".into(),
                    operator: FilterOperator::EndsWith,
                    value: FilterValue::Literal(json!("ce")),
                }),
            ],
        };
        let result = rows(execute(&g, &compound_cmd("a2", cf)).unwrap());
        assert!(result.is_empty());
    }

    // ---- #5 Exists / IsNull filter ----

    #[test]
    fn exists_matches_non_null_field() {
        let mut g = InMemoryGraph::new();
        g.add_node(
            "N",
            HashMap::from([("id".into(), json!("n1")), ("score".into(), json!(42))]),
        );

        let c = filter_cmd(
            "n1",
            Filter {
                property: "score".into(),
                operator: FilterOperator::Exists,
                value: FilterValue::Literal(json!(null)), // value is ignored
            },
        );
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn exists_rejects_missing_field() {
        let mut g = InMemoryGraph::new();
        g.add_node("N", HashMap::from([("id".into(), json!("n1"))]));

        let c = filter_cmd(
            "n1",
            Filter {
                property: "score".into(),
                operator: FilterOperator::Exists,
                value: FilterValue::Literal(json!(null)),
            },
        );
        let result = rows(execute(&g, &c).unwrap());
        assert!(result.is_empty());
    }

    #[test]
    fn is_null_matches_missing_field() {
        let mut g = InMemoryGraph::new();
        g.add_node("N", HashMap::from([("id".into(), json!("n1"))]));

        let c = filter_cmd(
            "n1",
            Filter {
                property: "score".into(),
                operator: FilterOperator::IsNull,
                value: FilterValue::Literal(json!(null)),
            },
        );
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn is_null_matches_explicit_null() {
        let mut g = InMemoryGraph::new();
        g.add_node(
            "N",
            HashMap::from([("id".into(), json!("n1")), ("score".into(), json!(null))]),
        );

        let c = filter_cmd(
            "n1",
            Filter {
                property: "score".into(),
                operator: FilterOperator::IsNull,
                value: FilterValue::Literal(json!(null)),
            },
        );
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn is_null_rejects_non_null_field() {
        let mut g = InMemoryGraph::new();
        g.add_node(
            "N",
            HashMap::from([("id".into(), json!("n1")), ("score".into(), json!(42))]),
        );

        let c = filter_cmd(
            "n1",
            Filter {
                property: "score".into(),
                operator: FilterOperator::IsNull,
                value: FilterValue::Literal(json!(null)),
            },
        );
        let result = rows(execute(&g, &c).unwrap());
        assert!(result.is_empty());
    }

    // ---- Arbitrary condition filtering ----

    fn node_graph_with_ages() -> InMemoryGraph {
        let mut g = InMemoryGraph::new();
        g.add_node(
            "N",
            HashMap::from([
                ("id".into(), json!("n1")),
                ("name".into(), json!("Alice")),
                ("age".into(), json!(30)),
                ("minAge".into(), json!(25)),
            ]),
        );
        g.add_node(
            "N",
            HashMap::from([
                ("id".into(), json!("n2")),
                ("name".into(), json!("Bob")),
                ("age".into(), json!(20)),
                ("minAge".into(), json!(25)),
            ]),
        );
        g.add_node(
            "N",
            HashMap::from([
                ("id".into(), json!("n3")),
                ("name".into(), json!("Carol")),
                ("age".into(), json!(28)),
                ("minAge".into(), json!(25)),
            ]),
        );
        g
    }

    #[test]
    fn matches_regex_filter() {
        let g = node_graph_with_ages();
        // name starts with "A" → Alice only
        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "AllNodes".into(),
                label: "N".into(),
                key: String::new(),
                value: json!(null),
            },
            steps: vec![TraversalStep {
                action: TraversalAction::Filter,
                label: None,
                labels: None,
                compound_filter: None,
                custom_filter: None,
                any_key_contains: None,
                count: None,
                skip: None,
                order_by: None,
                repeat_steps: None,
                max_depth: None,
                filter: Some(Filter {
                    property: "name".into(),
                    operator: FilterOperator::Matches,
                    value: FilterValue::Literal(json!("^A")),
                }),
            }],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], json!("n1"));
    }

    #[test]
    fn property_compare_greater_than() {
        let g = node_graph_with_ages();
        // age > minAge (cross-field comparison on same node): Alice(30>25)✓, Bob(20>25)✗, Carol(28>25)✓
        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "AllNodes".into(),
                label: "N".into(),
                key: String::new(),
                value: json!(null),
            },
            steps: vec![TraversalStep {
                action: TraversalAction::Filter,
                label: None,
                labels: None,
                compound_filter: None,
                custom_filter: None,
                any_key_contains: None,
                count: None,
                skip: None,
                order_by: None,
                repeat_steps: None,
                max_depth: None,
                filter: Some(Filter {
                    property: "age".into(),
                    operator: FilterOperator::PropertyGreaterThan,
                    value: FilterValue::PropertyRef {
                        r#type: crate::traversal::command::PropertyRefMarker::Property,
                        key: "minAge".into(),
                    },
                }),
            }],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        let ids: HashSet<&str> = result.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, HashSet::from(["n1", "n3"]));
    }

    #[test]
    fn custom_filter_batch() {
        // Custom filter: express "age > 25" as a Rust-side custom filter
        struct AgeGraph(InMemoryGraph);
        impl GraphAccess for AgeGraph {
            fn find_node(&self, l: &str, k: &str, v: &Value) -> Option<NodeId> {
                self.0.find_node(l, k, v)
            }
            fn all_nodes_of_label(&self, l: &str) -> Vec<NodeId> {
                self.0.all_nodes_of_label(l)
            }
            fn node_properties(&self, id: NodeId) -> Option<Cow<'_, HashMap<String, Value>>> {
                self.0.node_properties(id)
            }
            fn node_label(&self, id: NodeId) -> Option<String> {
                self.0.node_label(id)
            }
            fn out_edges(&self, n: NodeId, l: Option<&str>) -> Vec<EdgeId> {
                self.0.out_edges(n, l)
            }
            fn in_edges(&self, n: NodeId, l: Option<&str>) -> Vec<EdgeId> {
                self.0.in_edges(n, l)
            }
            fn edge_to_node(&self, e: EdgeId) -> Option<NodeId> {
                self.0.edge_to_node(e)
            }
            fn edge_from_node(&self, e: EdgeId) -> Option<NodeId> {
                self.0.edge_from_node(e)
            }
            fn edge_label(&self, id: EdgeId) -> Option<String> {
                self.0.edge_label(id)
            }
            fn edge_properties(&self, id: EdgeId) -> Option<Cow<'_, HashMap<String, Value>>> {
                self.0.edge_properties(id)
            }
            fn apply_custom_filter(
                &self,
                callback_id: &str,
                candidates: Vec<(NodeId, HashMap<String, Value>)>,
            ) -> Vec<NodeId> {
                if callback_id == "age_over_25" {
                    candidates
                        .into_iter()
                        .filter(|(_, p)| {
                            p.get("age")
                                .and_then(|v| v.as_i64())
                                .is_some_and(|a| a > 25)
                        })
                        .map(|(id, _)| id)
                        .collect()
                } else {
                    candidates.into_iter().map(|(id, _)| id).collect()
                }
            }
        }

        let g = AgeGraph(node_graph_with_ages());
        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "AllNodes".into(),
                label: "N".into(),
                key: String::new(),
                value: json!(null),
            },
            steps: vec![TraversalStep {
                action: TraversalAction::Filter,
                label: None,
                labels: None,
                filter: None,
                compound_filter: None,
                custom_filter: Some(crate::traversal::command::CustomFilter {
                    callback_id: "age_over_25".into(),
                }),
                count: None,
                skip: None,
                order_by: None,
                repeat_steps: None,
                max_depth: None,
                any_key_contains: None,
            }],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        let ids: HashSet<&str> = result.iter().map(|r| r["id"].as_str().unwrap()).collect();
        // Alice(30) and Carol(28) pass
        assert_eq!(ids, HashSet::from(["n1", "n3"]));
    }

    // ---- Binding variable tests ----

    #[test]
    fn binding_in_start_value() {
        let g = make_graph();
        let mut bindings = std::collections::HashMap::new();
        bindings.insert("uid".to_string(), json!("u1"));

        let c = TraversalCommand {
            bindings,
            version: 1,
            start: StartSpec {
                kind: "Node".into(),
                label: "User".into(),
                key: "id".into(),
                value: json!({ "$bind": "uid" }),
            },
            steps: vec![],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], json!("u1"));
    }

    #[test]
    fn binding_in_filter_value() {
        let g = make_graph();
        let mut bindings = std::collections::HashMap::new();
        bindings.insert("archived".to_string(), json!(false));

        let c = TraversalCommand {
            bindings,
            version: 1,
            start: StartSpec {
                kind: "Node".into(),
                label: "User".into(),
                key: "id".into(),
                value: json!("u1"),
            },
            steps: vec![
                step(TraversalAction::OutEdges, "OWNS"),
                step_with_filter(
                    TraversalAction::OutNodes,
                    "Project",
                    Filter {
                        property: "isArchived".into(),
                        operator: FilterOperator::Equals,
                        value: FilterValue::Binding {
                            bind: "archived".into(),
                        },
                    },
                ),
            ],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], json!("p1"));
    }

    #[test]
    fn missing_binding_returns_no_match() {
        let g = make_graph();
        // Undefined binding variable → cannot be resolved, so no results
        let c = TraversalCommand {
            bindings: std::collections::HashMap::new(),
            version: 1,
            start: StartSpec {
                kind: "Node".into(),
                label: "User".into(),
                key: "id".into(),
                value: json!("u1"),
            },
            steps: vec![step_with_filter(
                TraversalAction::Filter,
                "",
                Filter {
                    property: "name".into(),
                    operator: FilterOperator::Equals,
                    value: FilterValue::Binding {
                        bind: "nonexistent".into(),
                    },
                },
            )],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        assert!(result.is_empty());
    }

    // proptest: traversal results must match the in-memory model for random graph structures
    proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(50))]
        #[test]
        fn random_graph_traversal_matches_model(
            node_count in 2usize..10,
            edge_indices in proptest::collection::vec((0usize..10, 0usize..10), 0..20usize),
        ) {
            let mut g = InMemoryGraph::new();
            let mut node_ids = Vec::new();
            for i in 0..node_count {
                let nid = g.add_node("N", HashMap::from([
                    ("id".into(), json!(i.to_string())),
                ]));
                node_ids.push(nid);
            }

            // Model: set of edges from_node → to_node
            let mut model_out: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
            for (fi, ti) in &edge_indices {
                let from = node_ids[fi % node_count];
                let to   = node_ids[ti % node_count];
                g.add_edge("E", from, to, HashMap::new());
                model_out.entry(from).or_default().insert(to);
            }

            // Compare traversal results against the model for every node
            for &start in &node_ids {
                let start_id_str = g.node_properties(start).unwrap()["id"].as_str().unwrap().to_string();
                let c = TraversalCommand { bindings: std::collections::HashMap::new(),
                    version: 1,
                    start: StartSpec {
                        kind: "Node".into(),
                        label: "N".into(),
                        key: "id".into(),
                        value: json!(start_id_str),
                    },
                    steps: vec![
                        TraversalStep { action: TraversalAction::OutEdges, label: Some("E".into()), labels: None, filter: None, count: None, skip: None, order_by: None, compound_filter: None, repeat_steps: None, max_depth: None, custom_filter: None, any_key_contains: None },
                        TraversalStep { action: TraversalAction::OutNodes, label: Some("N".into()), labels: None, filter: None, count: None, skip: None, order_by: None, compound_filter: None, repeat_steps: None, max_depth: None, custom_filter: None, any_key_contains: None },
                    ],
                    collect: CollectSpec::Nodes { properties: vec!["id".into()], with_edges: None },
                };

                let result = rows(execute(&g, &c).unwrap());
                let actual_ids: HashSet<String> = result.iter()
                    .map(|r| r["id"].as_str().unwrap().to_string())
                    .collect();

                let expected_ids: HashSet<String> = model_out
                    .get(&start)
                    .map(|ns| ns.iter().map(|&n| {
                        g.node_properties(n).unwrap()["id"].as_str().unwrap().to_string()
                    }).collect())
                    .unwrap_or_default();

                prop_assert_eq!(actual_ids, expected_ids);
            }
        }
    }

    // ---- AnyKeyContains tests ----

    fn step_any_key_contains(substr: &str) -> TraversalStep {
        TraversalStep {
            action: TraversalAction::Filter,
            any_key_contains: Some(substr.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn any_key_contains_matches_string_property() {
        let g = make_graph();
        // "Ali" matches name="Alice"
        let c = TraversalCommand {
            version: 1,
            bindings: Default::default(),
            start: StartSpec {
                kind: "AllNodes".into(),
                label: "User".into(),
                key: "".into(),
                value: json!(null),
            },
            steps: vec![step_any_key_contains("Ali")],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], json!("u1"));
    }

    #[test]
    fn any_key_contains_is_case_insensitive() {
        let g = make_graph();
        // Case-insensitive
        let c = TraversalCommand {
            version: 1,
            bindings: Default::default(),
            start: StartSpec {
                kind: "AllNodes".into(),
                label: "User".into(),
                key: "".into(),
                value: json!(null),
            },
            steps: vec![step_any_key_contains("alice")],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], json!("u1"));
    }

    #[test]
    fn any_key_contains_no_match_returns_empty() {
        let g = make_graph();
        let c = TraversalCommand {
            version: 1,
            bindings: Default::default(),
            start: StartSpec {
                kind: "AllNodes".into(),
                label: "User".into(),
                key: "".into(),
                value: json!(null),
            },
            steps: vec![step_any_key_contains("zzz_no_match")],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        assert!(result.is_empty());
    }

    #[test]
    fn any_key_contains_skips_non_string_properties() {
        let g = make_graph();
        // isArchived=false is a boolean, so searching for "fal" does not match
        let c = TraversalCommand {
            version: 1,
            bindings: Default::default(),
            start: StartSpec {
                kind: "AllNodes".into(),
                label: "Project".into(),
                key: "".into(),
                value: json!(null),
            },
            steps: vec![step_any_key_contains("fal")],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        assert!(result.is_empty());
    }

    #[test]
    fn any_key_contains_matches_any_property_key() {
        let g = make_graph();
        // "Alph" matches title="Alpha"
        let c = TraversalCommand {
            version: 1,
            bindings: Default::default(),
            start: StartSpec {
                kind: "AllNodes".into(),
                label: "Project".into(),
                key: "".into(),
                value: json!(null),
            },
            steps: vec![step_any_key_contains("Alph")],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], json!("p1"));
    }

    #[test]
    fn value_contains_string_recurses_into_objects_and_arrays() {
        // Top-level string still matches (backward compatible).
        assert!(value_contains_string(&json!("Sakamoto Ryoma"), "ryoma"));
        // String nested in an object value is reached.
        assert!(value_contains_string(
            &json!({"name": "Sakamoto Ryoma", "year": 1867}),
            "ryoma"
        ));
        // String nested in an array is reached.
        assert!(value_contains_string(&json!(["Person", "Hero"]), "hero"));
        // Deeply nested object/array mix.
        assert!(value_contains_string(
            &json!({"props": {"roles": ["samurai", "diplomat"]}}),
            "diplomat"
        ));
        // Object keys are NOT searched (only values).
        assert!(!value_contains_string(&json!({"diplomat": 1}), "diplomat"));
        // No match returns false.
        assert!(!value_contains_string(
            &json!({"name": "Alice", "tags": ["a", "b"]}),
            "zzz"
        ));
        // Non-string scalars are ignored.
        assert!(!value_contains_string(&json!(1867), "1867"));
    }

    #[test]
    fn any_key_contains_reaches_into_aggregated_json_property() {
        let mut g = InMemoryGraph::new();
        // A meta-schema pattern: arbitrary user properties aggregated into a single
        // Json-typed `props` field. Keyword search must still reach nested strings.
        g.add_node(
            "Entity",
            HashMap::from([(
                "props".into(),
                json!({"name": "Sakamoto Ryoma", "year": 1867}),
            )]),
        );
        g.add_node(
            "Entity",
            HashMap::from([(
                "props".into(),
                json!({"name": "Tokugawa Yoshinobu", "year": 1837}),
            )]),
        );

        let c = TraversalCommand {
            version: 1,
            bindings: Default::default(),
            start: StartSpec {
                kind: "AllNodes".into(),
                label: "Entity".into(),
                key: "".into(),
                value: json!(null),
            },
            steps: vec![step_any_key_contains("Ryoma")],
            collect: CollectSpec::Nodes {
                properties: vec!["props".into()],
                with_edges: None,
            },
        };
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["props"]["name"], json!("Sakamoto Ryoma"));
    }

    fn entity_graph() -> InMemoryGraph {
        let mut g = InMemoryGraph::new();
        g.add_node(
            "Entity",
            HashMap::from([(
                "props".into(),
                json!({"name": "Ryoma", "year": 1867, "status": "active"}),
            )]),
        );
        g.add_node(
            "Entity",
            HashMap::from([(
                "props".into(),
                json!({"name": "Yoshinobu", "year": 1837, "status": "active"}),
            )]),
        );
        g.add_node(
            "Entity",
            HashMap::from([(
                "props".into(),
                json!({"name": "Saigo", "year": 1828, "status": "retired"}),
            )]),
        );
        g
    }

    fn all_entities(steps: Vec<TraversalStep>, props: Vec<&str>) -> TraversalCommand {
        TraversalCommand {
            version: 1,
            bindings: Default::default(),
            start: StartSpec {
                kind: "AllNodes".into(),
                label: "Entity".into(),
                key: "".into(),
                value: json!(null),
            },
            steps,
            collect: CollectSpec::Nodes {
                properties: props.into_iter().map(|s| s.to_string()).collect(),
                with_edges: None,
            },
        }
    }

    fn nested_path(segments: &[&str]) -> PropertyPath {
        PropertyPath::Path {
            path: segments.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn filter_on_nested_json_path_scalar() {
        let g = entity_graph();
        let filter = Filter {
            property: nested_path(&["props", "year"]),
            operator: FilterOperator::Equals,
            value: FilterValue::Literal(json!(1867)),
        };
        let c = all_entities(
            vec![step_with_filter(TraversalAction::Filter, "Entity", filter)],
            vec!["props"],
        );
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["props"]["name"], json!("Ryoma"));
    }

    #[test]
    fn filter_on_nested_json_path_string() {
        let g = entity_graph();
        let filter = Filter {
            property: nested_path(&["props", "status"]),
            operator: FilterOperator::Equals,
            value: FilterValue::Literal(json!("active")),
        };
        let c = all_entities(
            vec![step_with_filter(TraversalAction::Filter, "Entity", filter)],
            vec!["props"],
        );
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn order_by_nested_json_path() {
        let g = entity_graph();
        let mut step = step(TraversalAction::OrderBy, "Entity");
        step.order_by = Some(OrderBySpec {
            key: nested_path(&["props", "year"]),
            direction: SortDirection::Asc,
        });
        let c = all_entities(vec![step], vec!["props"]);
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 3);
        // Ascending by nested year: 1828, 1837, 1867.
        assert_eq!(result[0]["props"]["year"], json!(1828));
        assert_eq!(result[1]["props"]["year"], json!(1837));
        assert_eq!(result[2]["props"]["year"], json!(1867));
    }

    #[test]
    fn flat_property_filter_unchanged() {
        // Backward compatibility: a flat string property still resolves top-level.
        let mut g = InMemoryGraph::new();
        g.add_node("Entity", HashMap::from([("year".into(), json!(1867))]));
        g.add_node("Entity", HashMap::from([("year".into(), json!(1837))]));
        let filter = Filter {
            property: PropertyPath::Flat("year".to_string()),
            operator: FilterOperator::Equals,
            value: FilterValue::Literal(json!(1867)),
        };
        let c = all_entities(
            vec![step_with_filter(TraversalAction::Filter, "Entity", filter)],
            vec!["year"],
        );
        let result = rows(execute(&g, &c).unwrap());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["year"], json!(1867));
    }
}
