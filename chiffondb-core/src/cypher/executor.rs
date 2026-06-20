use std::collections::HashMap;

use serde_json::Value;

use crate::db::Database;
use crate::error::GraphError;
use crate::storage::page::{EdgeRid, NodeRid, RecordId};

use super::result::CypherResult;

/// Typed binding for a variable bound during MATCH/CREATE/MERGE.
/// Carries the node/edge distinction at the type level, eliminating runtime `__is_edge` markers.
#[derive(Clone, Copy)]
enum RidBinding {
    Node(NodeRid),
    Edge(#[allow(dead_code)] EdgeRid),
}

impl RidBinding {
    fn as_node(self) -> Option<NodeRid> {
        match self {
            RidBinding::Node(r) => Some(r),
            RidBinding::Edge(_) => None,
        }
    }
}

pub fn execute_cypher_query(db: &mut Database, query: &str) -> Result<CypherResult, GraphError> {
    let ast = decypher::parse(query).map_err(|e| GraphError::InvalidCommand(e.to_string()))?;

    let mut last_result = CypherResult::Empty;
    for stmt in &ast.statements {
        last_result = execute_statement(db, stmt)?;
    }
    Ok(last_result)
}

fn execute_statement(
    db: &mut Database,
    stmt: &decypher::ast::query::QueryBody,
) -> Result<CypherResult, GraphError> {
    use decypher::ast::query::QueryBody;
    match stmt {
        QueryBody::SingleQuery(sq) => execute_single_query(db, sq),
        _ => Err(GraphError::InvalidCommand("unsupported query type".into())),
    }
}

fn execute_single_query(
    db: &mut Database,
    sq: &decypher::ast::query::SingleQuery,
) -> Result<CypherResult, GraphError> {
    use decypher::ast::query::SingleQueryKind;
    match &sq.kind {
        SingleQueryKind::SinglePart(spq) => execute_single_part(db, spq),
        SingleQueryKind::MultiPart(_) => Err(GraphError::InvalidCommand(
            "multi-part queries (WITH) are not supported".into(),
        )),
    }
}

fn execute_single_part(
    db: &mut Database,
    spq: &decypher::ast::query::SinglePartQuery,
) -> Result<CypherResult, GraphError> {
    use decypher::ast::query::{ReadingClause, SinglePartBody, UpdatingClause};

    let mut bindings: HashMap<String, RidBinding> = HashMap::new();
    // Tracks which variable names were bound by MATCH (vs CREATE/MERGE).
    // SET/REMOVE/DELETE may only target MATCH-bound variables.
    let mut match_bound: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut match_rows: Vec<HashMap<String, Value>> = Vec::new();
    let mut has_match = false;
    let mut match_count = 0usize;

    for clause in &spq.reading_clauses {
        match clause {
            ReadingClause::Match(m) => {
                match_count += 1;
                if match_count > 1 {
                    return Err(GraphError::InvalidCommand(
                        "multiple MATCH clauses in a single query part are not supported".into(),
                    ));
                }
                has_match = true;
                // Collect variable names from the MATCH AST before execution so
                // match_bound is populated even when 0 rows are returned.
                collect_match_variables(m, &mut match_bound);
                let (new_bindings, rows) = execute_match(db, m)?;
                bindings.extend(new_bindings);
                match_rows = rows;
            }
            _ => {
                return Err(GraphError::InvalidCommand(
                    "only MATCH is supported as reading clause".into(),
                ));
            }
        }
    }

    match &spq.body {
        SinglePartBody::Return(ret) => {
            let rows = if has_match {
                project_return(ret, &match_rows)?
            } else {
                Vec::new()
            };
            Ok(CypherResult::Rows(rows))
        }
        SinglePartBody::Updating {
            updating,
            return_clause,
        } => {
            // Preflight: inspect the updating clause list before executing anything,
            // to prevent partial application (orphaned nodes from a CREATE that
            // preceded a rejected SET/DELETE).
            let has_update = updating.iter().any(|uc| {
                matches!(
                    uc,
                    UpdatingClause::Set(_) | UpdatingClause::Remove(_) | UpdatingClause::Delete(_)
                )
            });
            let has_create_or_merge = updating
                .iter()
                .any(|uc| matches!(uc, UpdatingClause::Create(_) | UpdatingClause::Merge(_)));
            if has_update {
                if !has_match {
                    return Err(GraphError::InvalidCommand(
                        "SET/REMOVE/DELETE without MATCH is not supported; \
                         use MATCH ... SET/DELETE to update existing nodes/edges"
                            .into(),
                    ));
                }
                if has_create_or_merge {
                    return Err(GraphError::InvalidCommand(
                        "mixing CREATE/MERGE with SET/REMOVE/DELETE in the same query part \
                         is not supported"
                            .into(),
                    ));
                }
            }

            let mut total_nodes = 0usize;
            let mut total_edges = 0usize;
            let mut total_updated = 0usize;
            let mut total_deleted = 0usize;
            let mut last_op = OpKind::None;
            let mut create_rows: Vec<HashMap<String, Value>> = Vec::new();

            for uc in updating {
                match uc {
                    UpdatingClause::Create(c) => {
                        let (n, e, rows) = execute_create(db, c, &mut bindings)?;
                        total_nodes += n;
                        total_edges += e;
                        create_rows.extend(rows);
                        last_op = OpKind::Create;
                    }
                    UpdatingClause::Merge(m) => {
                        let (n, e, rows) = execute_merge(db, m, &mut bindings)?;
                        total_nodes += n;
                        total_edges += e;
                        create_rows.extend(rows);
                        last_op = OpKind::Create;
                    }
                    UpdatingClause::Set(s) => {
                        let n = execute_set(db, s, &match_rows, &bindings, &match_bound)?;
                        total_updated += n;
                        last_op = OpKind::Update;
                    }
                    UpdatingClause::Remove(r) => {
                        let n = execute_remove(db, r, &match_rows, &bindings, &match_bound)?;
                        total_updated += n;
                        last_op = OpKind::Update;
                    }
                    UpdatingClause::Delete(d) => {
                        let n = execute_delete(db, d, &match_rows, &bindings, &match_bound)?;
                        total_deleted += n;
                        last_op = OpKind::Delete;
                    }
                    UpdatingClause::Foreach(_) => {
                        return Err(GraphError::InvalidCommand(
                            "FOREACH is not supported".into(),
                        ));
                    }
                }
            }

            if let Some(ret) = return_clause {
                let rows_to_project = if !create_rows.is_empty() {
                    &create_rows
                } else {
                    &match_rows
                };
                let rows = project_return(ret, rows_to_project)?;
                return Ok(CypherResult::Rows(rows));
            }

            Ok(match last_op {
                OpKind::Create => CypherResult::Created {
                    nodes: total_nodes,
                    edges: total_edges,
                },
                OpKind::Update => CypherResult::Updated(total_updated),
                OpKind::Delete => CypherResult::Deleted(total_deleted),
                OpKind::None => CypherResult::Empty,
            })
        }
        SinglePartBody::Finish(_) => Ok(CypherResult::Empty),
    }
}

enum OpKind {
    None,
    Create,
    Update,
    Delete,
}

// ---- MATCH ----

type MatchResult = (HashMap<String, RidBinding>, Vec<HashMap<String, Value>>);
type CreateResult = (usize, usize, Vec<HashMap<String, Value>>);

/// Validates that a SET/REMOVE/DELETE target variable is MATCH-bound.
/// Returns Ok(()) if the variable was declared in MATCH (may still produce a 0-row no-op).
/// Returns Err if the variable is unbound (typo) or was bound only by CREATE/MERGE.
///
/// Note: the CREATE/MERGE branch below is currently unreachable — execute_single_part's
/// preflight rejects any query that mixes CREATE/MERGE with SET/REMOVE/DELETE before
/// these functions are called, so bindings never contains CREATE/MERGE-only variables
/// at this point. The branch is retained as a guard for when mixed queries are supported.
fn check_target_bound(
    vn: &str,
    clause: &str,
    bindings: &HashMap<String, RidBinding>,
    match_bound: &std::collections::HashSet<String>,
) -> Result<(), GraphError> {
    if match_bound.contains(vn) {
        return Ok(());
    }
    if bindings.contains_key(vn) {
        // Currently unreachable; see doc comment above.
        return Err(GraphError::InvalidCommand(format!(
            "{clause} target '{vn}' was not bound by MATCH; \
             updating CREATE/MERGE variables is not supported"
        )));
    }
    Err(GraphError::InvalidCommand(format!(
        "{clause} target '{vn}' is not bound in MATCH"
    )))
}

/// Collects variable names from a single Path element (start node + chains).
fn collect_path_variables(
    start: &decypher::ast::pattern::NodePattern,
    chains: &[decypher::ast::pattern::PatternElementChain],
    out: &mut std::collections::HashSet<String>,
) {
    if let Some(var) = &start.variable {
        out.insert(var.name.name.clone());
    }
    for chain in chains {
        if let Some(detail) = &chain.relationship.detail {
            if let Some(var) = &detail.variable {
                out.insert(var.name.name.clone());
            }
        }
        if let Some(var) = &chain.node.variable {
            out.insert(var.name.name.clone());
        }
    }
}

/// Collects all variable names declared in a MATCH clause from the AST.
/// Used to populate match_bound before execution so the set is correct even
/// when MATCH returns 0 rows (in which case bindings remains empty).
fn collect_match_variables(
    m: &decypher::ast::clause::Match,
    out: &mut std::collections::HashSet<String>,
) {
    use decypher::ast::pattern::PatternElement;
    for part in &m.pattern.parts {
        match &part.anonymous.element {
            PatternElement::Path { start, chains } => {
                collect_path_variables(start, chains, out);
            }
            PatternElement::Parenthesized(inner) => {
                if let PatternElement::Path { start, chains } = inner.as_ref() {
                    collect_path_variables(start, chains, out);
                }
            }
            PatternElement::Quantified { .. } => {
                // Quantified path patterns are not supported; variable collection skipped.
            }
        }
    }
}

fn execute_match(
    db: &mut Database,
    m: &decypher::ast::clause::Match,
) -> Result<MatchResult, GraphError> {
    use decypher::ast::pattern::PatternElement;

    let mut bindings: HashMap<String, RidBinding> = HashMap::new();
    // Start with a single empty row so the first cross join has something to join against.
    let mut rows: Vec<HashMap<String, Value>> = vec![HashMap::new()];

    for part in &m.pattern.parts {
        let (part_bindings, part_rows) = match &part.anonymous.element {
            PatternElement::Path { start, chains } if chains.is_empty() => {
                // Simple node pattern: (n:Label {props})
                let label = extract_first_label(&start.labels);
                let candidates = db.list_nodes(label.as_deref())?;
                let mut pb: HashMap<String, RidBinding> = HashMap::new();
                let mut pr: Vec<HashMap<String, Value>> = Vec::new();

                for (rid, props) in candidates {
                    if let Some(inline_props) = extract_inline_props(&start.properties) {
                        if !props_match(&props, &inline_props) {
                            continue;
                        }
                    }

                    if let Some(where_expr) = &m.where_clause {
                        let var_name = start
                            .variable
                            .as_ref()
                            .map(|v| v.name.name.clone())
                            .unwrap_or_default();
                        let ctx = build_node_ctx(&var_name, rid, &props);
                        if !eval_bool(where_expr, &ctx) {
                            continue;
                        }
                    }

                    let mut row: HashMap<String, Value> = HashMap::new();
                    if let Some(var) = &start.variable {
                        let vn = &var.name.name;
                        for (k, v) in &props {
                            row.insert(format!("{}.{}", vn, k), v.clone());
                        }
                        row.insert(format!("{}.__rid_page", vn), Value::from(rid.page_id().0));
                        row.insert(format!("{}.__rid_slot", vn), Value::from(rid.slot_id().0));
                        let labels = db.get_node_label_names(rid)?;
                        row.insert(vn.clone(), node_value(rid.0, &labels, &props));
                        pb.insert(vn.clone(), RidBinding::Node(rid));
                    }
                    pr.push(row);
                }
                (pb, pr)
            }
            PatternElement::Path { start, chains } => {
                // (a)-[r]->(b) form
                execute_path_match(db, start, chains, &m.where_clause)?
            }
            PatternElement::Parenthesized(inner) => {
                // Parenthesized pattern: process recursively (simplified: Path only)
                if let PatternElement::Path { start, chains } = inner.as_ref() {
                    execute_path_match(db, start, chains, &m.where_clause)?
                } else {
                    (HashMap::new(), Vec::new())
                }
            }
            PatternElement::Quantified { .. } => {
                return Err(GraphError::InvalidCommand(
                    "quantified patterns are not supported".into(),
                ));
            }
        };

        bindings.extend(part_bindings);
        // Cross join: combine every existing row with every row from this pattern part.
        rows = rows
            .iter()
            .flat_map(|existing| {
                part_rows.iter().map(move |part_row| {
                    let mut merged = existing.clone();
                    merged.extend(part_row.clone());
                    merged
                })
            })
            .collect();
    }

    // If there were no pattern parts, rows contains one empty row; normalise to empty.
    if rows.len() == 1 && rows[0].is_empty() {
        rows.clear();
    }

    Ok((bindings, rows))
}

fn execute_path_match(
    db: &mut Database,
    start: &decypher::ast::pattern::NodePattern,
    chains: &[decypher::ast::pattern::PatternElementChain],
    where_clause: &Option<decypher::ast::expr::Expression>,
) -> Result<MatchResult, GraphError> {
    use decypher::ast::pattern::RelationshipDirection;

    let start_label = extract_first_label(&start.labels);
    let start_candidates = db.list_nodes(start_label.as_deref())?;
    let mut path_bindings: HashMap<String, RidBinding> = HashMap::new();
    let mut rows: Vec<HashMap<String, Value>> = Vec::new();

    for (start_rid, start_props) in &start_candidates {
        if let Some(ip) = extract_inline_props(&start.properties) {
            if !props_match(start_props, &ip) {
                continue;
            }
        }

        // Store edge info (rid, label, props, from_rid, to_rid) for each hop,
        // keyed to the tail node, so that relationship variables (e.g. r) can be
        // referenced directly in RETURN. Intermediate-hop edges and nodes are not
        // recorded, only the final hop (consistent with the existing implementation).
        type Hop = (EdgeRid, HashMap<String, Value>, NodeRid, NodeRid);
        let mut current: Vec<(NodeRid, HashMap<String, Value>, Option<Hop>)> =
            vec![(*start_rid, start_props.clone(), None)];

        let mut ok = true;
        for chain in chains {
            let rel = &chain.relationship;
            if rel.detail.as_ref().and_then(|d| d.range.as_ref()).is_some() {
                return Err(GraphError::InvalidCommand(
                    "variable-length paths are not supported".into(),
                ));
            }

            let rel_label = rel
                .detail
                .as_ref()
                .and_then(|d| d.types.as_ref())
                .and_then(label_expr_name);
            let end_label = extract_first_label(&chain.node.labels);
            let end_inline = extract_inline_props(&chain.node.properties);

            let mut next: Vec<(NodeRid, HashMap<String, Value>, Option<Hop>)> = Vec::new();

            for (cur_rid, _, _) in &current {
                // is_outgoing=true means cur_rid is the source (list_out_neighbors);
                // is_outgoing=false means cur_rid is the target (list_in_neighbors).
                // Needed to correctly restore the actual source/target of each edge.
                let neighbors: Vec<(EdgeRid, HashMap<String, Value>, NodeRid, bool)> =
                    match rel.direction {
                        RelationshipDirection::Left => db
                            .list_in_neighbors(*cur_rid, rel_label.as_deref())?
                            .into_iter()
                            .map(|(e, p, n)| (e, p, n, false))
                            .collect(),
                        RelationshipDirection::Right | RelationshipDirection::Undirected => db
                            .list_out_neighbors(*cur_rid, rel_label.as_deref())?
                            .into_iter()
                            .map(|(e, p, n)| (e, p, n, true))
                            .collect(),
                        RelationshipDirection::Both => {
                            let mut v: Vec<_> = db
                                .list_out_neighbors(*cur_rid, rel_label.as_deref())?
                                .into_iter()
                                .map(|(e, p, n)| (e, p, n, true))
                                .collect();
                            v.extend(
                                db.list_in_neighbors(*cur_rid, rel_label.as_deref())?
                                    .into_iter()
                                    .map(|(e, p, n)| (e, p, n, false)),
                            );
                            v
                        }
                    };

                for (edge_rid, edge_props, node_rid, is_outgoing) in neighbors {
                    let node_props = db.get_node_properties(node_rid)?;
                    if let Some(el) = &end_label {
                        let labels = db.get_node_label_names(node_rid)?;
                        if !labels.iter().any(|l| l == el) {
                            continue;
                        }
                    }
                    if let Some(ep) = &end_inline {
                        if !props_match(&node_props, ep) {
                            continue;
                        }
                    }
                    let (from_rid, to_rid) = if is_outgoing {
                        (*cur_rid, node_rid)
                    } else {
                        (node_rid, *cur_rid)
                    };
                    let hop = (edge_rid, edge_props, from_rid, to_rid);
                    next.push((node_rid, node_props, Some(hop)));
                }
            }

            if next.is_empty() {
                ok = false;
                break;
            }
            current = next;
        }

        if !ok {
            continue;
        }

        let last_chain = chains.last().unwrap();
        for (end_rid, end_props, last_hop) in &current {
            let mut row: HashMap<String, Value> = HashMap::new();

            if let Some(var) = &start.variable {
                let vn = &var.name.name;
                for (k, v) in start_props {
                    row.insert(format!("{}.{}", vn, k), v.clone());
                }
                row.insert(
                    format!("{}.__rid_page", vn),
                    Value::from(start_rid.page_id().0),
                );
                row.insert(
                    format!("{}.__rid_slot", vn),
                    Value::from(start_rid.slot_id().0),
                );
                let labels = db.get_node_label_names(*start_rid)?;
                row.insert(vn.clone(), node_value(start_rid.0, &labels, start_props));
                path_bindings.insert(vn.clone(), RidBinding::Node(*start_rid));
            }

            if let Some(var) = &last_chain.node.variable {
                let vn = &var.name.name;
                for (k, v) in end_props {
                    row.insert(format!("{}.{}", vn, k), v.clone());
                }
                row.insert(
                    format!("{}.__rid_page", vn),
                    Value::from(end_rid.page_id().0),
                );
                row.insert(
                    format!("{}.__rid_slot", vn),
                    Value::from(end_rid.slot_id().0),
                );
                let labels = db.get_node_label_names(*end_rid)?;
                row.insert(vn.clone(), node_value(end_rid.0, &labels, end_props));
                path_bindings.insert(vn.clone(), RidBinding::Node(*end_rid));
            }

            if let Some(rel_var) = last_chain
                .relationship
                .detail
                .as_ref()
                .and_then(|d| d.variable.as_ref())
            {
                if let Some((edge_rid, edge_props, from_rid, to_rid)) = last_hop {
                    let vn = &rel_var.name.name;
                    let edge_labels = db.get_edge_label_names(*edge_rid)?;
                    let edge_type = edge_labels.first().cloned().unwrap_or_default();
                    for (k, v) in edge_props {
                        row.insert(format!("{}.{}", vn, k), v.clone());
                    }
                    row.insert(
                        format!("{}.__rid_page", vn),
                        Value::from(edge_rid.page_id().0),
                    );
                    row.insert(
                        format!("{}.__rid_slot", vn),
                        Value::from(edge_rid.slot_id().0),
                    );
                    row.insert(
                        vn.clone(),
                        edge_value(edge_rid.0, &edge_type, from_rid.0, to_rid.0, edge_props),
                    );
                    path_bindings.insert(vn.clone(), RidBinding::Edge(*edge_rid));
                }
            }

            if let Some(where_expr) = where_clause {
                if !eval_bool(where_expr, &row) {
                    continue;
                }
            }

            rows.push(row);
        }
    }

    Ok((path_bindings, rows))
}

// ---- CREATE ----

fn execute_create(
    db: &mut Database,
    c: &decypher::ast::clause::Create,
    bindings: &mut HashMap<String, RidBinding>,
) -> Result<CreateResult, GraphError> {
    use decypher::ast::pattern::PatternElement;

    let mut nodes = 0usize;
    let mut edges = 0usize;
    let mut rows: Vec<HashMap<String, Value>> = Vec::new();

    for part in &c.pattern.parts {
        match &part.anonymous.element {
            PatternElement::Path { start, chains } if chains.is_empty() => {
                let label = extract_first_label(&start.labels).ok_or_else(|| {
                    GraphError::InvalidCommand("CREATE node requires a label".into())
                })?;
                let props = extract_inline_props(&start.properties).unwrap_or_default();
                let rid = db.insert_node_by_name(&label, props.clone())?;
                let mut row: HashMap<String, Value> = HashMap::new();
                if let Some(var) = &start.variable {
                    let vn = &var.name.name;
                    bindings.insert(vn.clone(), RidBinding::Node(rid));
                    for (k, v) in &props {
                        row.insert(format!("{}.{}", vn, k), v.clone());
                    }
                    row.insert(format!("{}.__rid_page", vn), Value::from(rid.page_id().0));
                    row.insert(format!("{}.__rid_slot", vn), Value::from(rid.slot_id().0));
                    let labels = db.get_node_label_names(rid)?;
                    row.insert(vn.clone(), node_value(rid.0, &labels, &props));
                }
                nodes += 1;
                rows.push(row);
            }
            PatternElement::Path { start, chains } => {
                let (n, e, row) = create_path(db, start, chains, bindings)?;
                nodes += n;
                edges += e;
                rows.push(row);
            }
            _ => {
                return Err(GraphError::InvalidCommand(
                    "unsupported CREATE pattern".into(),
                ));
            }
        }
    }

    Ok((nodes, edges, rows))
}

fn create_path(
    db: &mut Database,
    start: &decypher::ast::pattern::NodePattern,
    chains: &[decypher::ast::pattern::PatternElementChain],
    bindings: &mut HashMap<String, RidBinding>,
) -> Result<(usize, usize, HashMap<String, Value>), GraphError> {
    let mut nodes = 0usize;
    let mut edges = 0usize;
    let mut row: HashMap<String, Value> = HashMap::new();

    let mut cur_rid = ensure_node(db, start, bindings, &mut nodes)?;
    fill_node_row(db, cur_rid, start, &mut row)?;

    for chain in chains {
        let end_rid = ensure_node(db, &chain.node, bindings, &mut nodes)?;
        fill_node_row(db, end_rid, &chain.node, &mut row)?;
        let rel = &chain.relationship;

        let rel_label = rel
            .detail
            .as_ref()
            .and_then(|d| d.types.as_ref())
            .and_then(label_expr_name)
            .ok_or_else(|| {
                GraphError::InvalidCommand("CREATE relationship requires a type".into())
            })?;
        let rel_props = rel
            .detail
            .as_ref()
            .and_then(|d| extract_inline_props(&d.properties))
            .unwrap_or_default();

        use decypher::ast::pattern::RelationshipDirection;
        let (from, to) = match rel.direction {
            RelationshipDirection::Left => (end_rid, cur_rid),
            _ => (cur_rid, end_rid),
        };

        let edge_rid = db.insert_edge_by_name(&rel_label, from, to, rel_props.clone())?;
        if let Some(detail) = &rel.detail {
            if let Some(var) = &detail.variable {
                let vn = &var.name.name;
                bindings.insert(vn.clone(), RidBinding::Edge(edge_rid));
                for (k, v) in &rel_props {
                    row.insert(format!("{}.{}", vn, k), v.clone());
                }
                row.insert(
                    format!("{}.__rid_page", vn),
                    Value::from(edge_rid.page_id().0),
                );
                row.insert(
                    format!("{}.__rid_slot", vn),
                    Value::from(edge_rid.slot_id().0),
                );
                row.insert(
                    vn.clone(),
                    edge_value(edge_rid.0, &rel_label, from.0, to.0, &rel_props),
                );
            }
        }
        edges += 1;
        cur_rid = end_rid;
    }

    Ok((nodes, edges, row))
}

fn ensure_node(
    db: &mut Database,
    node_pat: &decypher::ast::pattern::NodePattern,
    bindings: &mut HashMap<String, RidBinding>,
    nodes: &mut usize,
) -> Result<NodeRid, GraphError> {
    if let Some(var) = &node_pat.variable {
        if let Some(binding) = bindings.get(&var.name.name) {
            if let Some(nrid) = binding.as_node() {
                return Ok(nrid);
            }
        }
    }
    let label = extract_first_label(&node_pat.labels)
        .ok_or_else(|| GraphError::InvalidCommand("node requires a label".into()))?;
    let props = extract_inline_props(&node_pat.properties).unwrap_or_default();
    let rid = db.insert_node_by_name(&label, props)?;
    if let Some(var) = &node_pat.variable {
        bindings.insert(var.name.name.clone(), RidBinding::Node(rid));
    }
    *nodes += 1;
    Ok(rid)
}

fn fill_node_row(
    db: &mut Database,
    rid: NodeRid,
    node_pat: &decypher::ast::pattern::NodePattern,
    row: &mut HashMap<String, Value>,
) -> Result<(), GraphError> {
    if let Some(var) = &node_pat.variable {
        let vn = &var.name.name;
        let props = extract_inline_props(&node_pat.properties).unwrap_or_default();
        for (k, v) in &props {
            row.insert(format!("{}.{}", vn, k), v.clone());
        }
        row.insert(format!("{}.__rid_page", vn), Value::from(rid.page_id().0));
        row.insert(format!("{}.__rid_slot", vn), Value::from(rid.slot_id().0));
        let labels = db.get_node_label_names(rid)?;
        row.insert(vn.clone(), node_value(rid.0, &labels, &props));
    }
    Ok(())
}

// ---- MERGE ----

fn execute_merge(
    db: &mut Database,
    m: &decypher::ast::clause::Merge,
    bindings: &mut HashMap<String, RidBinding>,
) -> Result<CreateResult, GraphError> {
    use decypher::ast::pattern::PatternElement;

    match &m.pattern.anonymous.element {
        PatternElement::Path { start, chains } if chains.is_empty() => {
            let label = extract_first_label(&start.labels)
                .ok_or_else(|| GraphError::InvalidCommand("MERGE node requires a label".into()))?;
            let props = extract_inline_props(&start.properties).unwrap_or_default();

            let existing = db.list_nodes(Some(&label))?;
            let found = existing.into_iter().find(|(_, ep)| props_match(ep, &props));

            let rid = if let Some((rid, _)) = found {
                rid
            } else {
                db.insert_node_by_name(&label, props.clone())?
            };

            let mut row: HashMap<String, Value> = HashMap::new();
            if let Some(var) = &start.variable {
                let vn = &var.name.name;
                bindings.insert(vn.clone(), RidBinding::Node(rid));
                let actual_props = db.get_node_properties(rid)?;
                for (k, v) in &actual_props {
                    row.insert(format!("{}.{}", vn, k), v.clone());
                }
                row.insert(format!("{}.__rid_page", vn), Value::from(rid.page_id().0));
                row.insert(format!("{}.__rid_slot", vn), Value::from(rid.slot_id().0));
                let labels = db.get_node_label_names(rid)?;
                row.insert(vn.clone(), node_value(rid.0, &labels, &actual_props));
            }

            // ON MATCH / ON CREATE SET is not implemented (skipped)
            Ok((0, 0, vec![row]))
        }
        _ => Err(GraphError::InvalidCommand(
            "MERGE with relationship patterns is not supported".into(),
        )),
    }
}

// ---- SET ----

fn execute_set(
    db: &mut Database,
    s: &decypher::ast::clause::Set,
    match_rows: &[HashMap<String, Value>],
    bindings: &HashMap<String, RidBinding>,
    match_bound: &std::collections::HashSet<String>,
) -> Result<usize, GraphError> {
    use decypher::ast::clause::SetItem;
    use decypher::ast::expr::Expression;

    // Pre-check all target variables before touching any row, so that unbound
    // variables and non-MATCH variables are caught even when match_rows is empty.
    for item in &s.items {
        if let SetItem::Property {
            property: Expression::PropertyLookup { base, .. },
            ..
        } = item
        {
            if let Expression::Variable(var) = base.as_ref() {
                check_target_bound(&var.name.name, "SET", bindings, match_bound)?;
            }
        }
    }

    let mut count = 0usize;

    for row in match_rows {
        for item in &s.items {
            if let SetItem::Property {
                property:
                    Expression::PropertyLookup {
                        base,
                        property: key_name,
                        ..
                    },
                value,
                ..
            } = item
            {
                if let Expression::Variable(var) = base.as_ref() {
                    let vn = &var.name.name;
                    if let (Some(rid), Some(binding)) =
                        (extract_rid_from_row(row, vn), bindings.get(vn))
                    {
                        let new_val = eval_expr(value, row).unwrap_or(Value::Null);
                        match binding {
                            RidBinding::Edge(_) => {
                                let erid = EdgeRid(rid);
                                let mut props = db.get_edge_properties(erid)?;
                                props.insert(key_name.name.name.clone(), new_val);
                                db.update_edge_properties(erid, props)?;
                            }
                            RidBinding::Node(_) => {
                                let nrid = NodeRid(rid);
                                let mut props = db.get_node_properties(nrid)?;
                                props.insert(key_name.name.name.clone(), new_val);
                                db.update_node_properties(nrid, props)?;
                            }
                        }
                        count += 1;
                    }
                }
            }
        }
    }

    Ok(count)
}

// ---- REMOVE ----

fn execute_remove(
    db: &mut Database,
    r: &decypher::ast::clause::Remove,
    match_rows: &[HashMap<String, Value>],
    bindings: &HashMap<String, RidBinding>,
    match_bound: &std::collections::HashSet<String>,
) -> Result<usize, GraphError> {
    use decypher::ast::clause::RemoveItem;
    use decypher::ast::expr::Expression;

    // Pre-check all target variables before row iteration.
    for item in &r.items {
        match item {
            RemoveItem::Property(prop_expr) => {
                if let Expression::PropertyLookup { base, .. } = prop_expr {
                    if let Expression::Variable(var) = base.as_ref() {
                        check_target_bound(&var.name.name, "REMOVE", bindings, match_bound)?;
                    }
                }
            }
            RemoveItem::Labels { variable, .. } => {
                let vn = &variable.name.name;
                // decypher 0.2.0-alpha.6 bug: variable.name.name is always empty for
                // label REMOVE syntax. Return a clear error until the parser is fixed.
                if vn.is_empty() {
                    return Err(GraphError::InvalidCommand(
                        "REMOVE <variable>:<Label> is not available due to a parser \
                         limitation; use DELETE to remove nodes"
                            .into(),
                    ));
                }
                check_target_bound(vn, "REMOVE", bindings, match_bound)?;
            }
        }
    }

    let mut count = 0usize;

    for row in match_rows {
        for item in &r.items {
            match item {
                RemoveItem::Property(prop_expr) => {
                    if let Expression::PropertyLookup {
                        base,
                        property: key_name,
                        ..
                    } = prop_expr
                    {
                        if let Expression::Variable(var) = base.as_ref() {
                            let vn = &var.name.name;
                            if let (Some(rid), Some(binding)) =
                                (extract_rid_from_row(row, vn), bindings.get(vn))
                            {
                                match binding {
                                    RidBinding::Edge(_) => {
                                        let erid = EdgeRid(rid);
                                        let mut props = db.get_edge_properties(erid)?;
                                        props.remove(&key_name.name.name);
                                        db.update_edge_properties(erid, props)?;
                                    }
                                    RidBinding::Node(_) => {
                                        let nrid = NodeRid(rid);
                                        let mut props = db.get_node_properties(nrid)?;
                                        props.remove(&key_name.name.name);
                                        db.update_node_properties(nrid, props)?;
                                    }
                                }
                                count += 1;
                            }
                        }
                    }
                }
                RemoveItem::Labels { variable, labels } => {
                    // Currently unreachable: the pre-check vn.is_empty() guard fires
                    // first due to decypher 0.2.0-alpha.6 bug. Remove that guard once
                    // the parser is fixed; this branch will then handle label removal
                    // and reject REMOVE on relationships.
                    let vn = &variable.name.name;
                    if let (Some(rid), Some(binding)) =
                        (extract_rid_from_row(row, vn), bindings.get(vn))
                    {
                        match binding {
                            RidBinding::Edge(_) => {
                                return Err(GraphError::InvalidCommand(
                                    "REMOVE label is not supported on relationships".into(),
                                ));
                            }
                            RidBinding::Node(_) => {
                                let nrid = NodeRid(rid);
                                for sym in labels {
                                    db.remove_node_label_by_name(nrid, &sym.name)?;
                                    count += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(count)
}

// ---- DELETE ----

fn execute_delete(
    db: &mut Database,
    d: &decypher::ast::clause::Delete,
    match_rows: &[HashMap<String, Value>],
    bindings: &HashMap<String, RidBinding>,
    match_bound: &std::collections::HashSet<String>,
) -> Result<usize, GraphError> {
    use decypher::ast::expr::Expression;

    // Pre-check all target variables before row iteration.
    for target in &d.targets {
        if let Expression::Variable(var) = target {
            check_target_bound(&var.name.name, "DELETE", bindings, match_bound)?;
        }
    }

    let mut count = 0usize;

    for row in match_rows {
        for target in &d.targets {
            if let Expression::Variable(var) = target {
                let vn = &var.name.name;
                // Resolve the current row's RID from row metadata, then use bindings
                // to determine node vs edge. Existence check handles both duplicate
                // rows (same node in multiple match rows) and cascaded edges.
                if let Some(rid) = extract_rid_from_row(row, vn) {
                    match bindings.get(vn) {
                        Some(RidBinding::Edge(_)) => {
                            let erid = EdgeRid(rid);
                            if db.edge_exists(erid) {
                                db.delete_edge(erid)?;
                                count += 1;
                            }
                        }
                        Some(RidBinding::Node(_)) => {
                            let nrid = NodeRid(rid);
                            if db.node_exists(nrid) {
                                db.delete_node(nrid)?;
                                count += 1;
                            }
                        }
                        // Row has RID metadata for vn but bindings has no entry.
                        // This cannot happen while execute_match writes __rid_* and
                        // RidBinding entries in lockstep, but skip defensively rather
                        // than panic so a future refactor does not silently turn this
                        // into a crash.
                        None => continue,
                    }
                }
            }
        }
    }

    Ok(count)
}

// ---- RETURN projection ----

fn project_return(
    ret: &decypher::ast::clause::Return,
    rows: &[HashMap<String, Value>],
) -> Result<Vec<HashMap<String, Value>>, GraphError> {
    let mut result: Vec<HashMap<String, Value>> = rows
        .iter()
        .map(|row| project_row(&ret.items, row))
        .collect();

    if let Some(order) = &ret.order {
        for sort_item in order.items.iter().rev() {
            let key = expr_to_key(&sort_item.expression);
            let asc = !matches!(
                sort_item.direction,
                Some(decypher::ast::clause::SortDirection::Descending)
            );
            result.sort_by(|a, b| {
                let av = a.get(&key).unwrap_or(&Value::Null);
                let bv = b.get(&key).unwrap_or(&Value::Null);
                let ord = compare_values(av, bv);
                if asc {
                    ord
                } else {
                    ord.reverse()
                }
            });
        }
    }

    if let Some(skip_expr) = &ret.skip {
        let n = eval_as_usize(skip_expr);
        result = result.into_iter().skip(n).collect();
    }

    if let Some(limit_expr) = &ret.limit {
        let n = eval_as_usize(limit_expr);
        result.truncate(n);
    }

    if ret.distinct {
        let mut seen: Vec<HashMap<String, Value>> = Vec::new();
        for row in result {
            if !seen.contains(&row) {
                seen.push(row);
            }
        }
        return Ok(seen);
    }

    Ok(result)
}

fn project_row(
    items: &[decypher::ast::clause::ProjectionItem],
    row: &HashMap<String, Value>,
) -> HashMap<String, Value> {
    let mut out: HashMap<String, Value> = HashMap::new();
    for item in items {
        let key = if let Some(alias) = &item.alias {
            alias.name.name.clone()
        } else {
            expr_to_key(&item.expression)
        };
        let val = eval_expr(&item.expression, row).unwrap_or(Value::Null);
        out.insert(key, val);
    }
    out
}

fn expr_to_key(expr: &decypher::ast::expr::Expression) -> String {
    use decypher::ast::expr::Expression;
    match expr {
        Expression::Variable(v) => v.name.name.clone(),
        Expression::PropertyLookup { base, property, .. } => {
            format!("{}.{}", expr_to_key(base), property.name.name)
        }
        _ => "value".to_string(),
    }
}

// ---- Graph rendering: convert nodes/edges to RETURN-able objects ----
//
// The graph renderer in the GUI browser (separate repo) expects rows that return a
// variable as-is (e.g. `RETURN n`) to contain objects shaped as:
//   nodes: {id, labels, properties}
//   edges: {id, type, source, target, properties} (distinguished by the presence of source/target)
// RecordIds are serialized as "<page>_<slot>" strings to match the identifier format
// used by the node list (Explorer).

fn rid_to_string(rid: RecordId) -> String {
    format!("{}_{}", rid.page_id.0, rid.slot_id.0)
}

fn node_value(rid: RecordId, labels: &[String], props: &HashMap<String, Value>) -> Value {
    serde_json::json!({
        "id": rid_to_string(rid),
        "labels": labels,
        "properties": props,
    })
}

fn edge_value(
    rid: RecordId,
    type_name: &str,
    source: RecordId,
    target: RecordId,
    props: &HashMap<String, Value>,
) -> Value {
    serde_json::json!({
        "id": rid_to_string(rid),
        "type": type_name,
        "source": rid_to_string(source),
        "target": rid_to_string(target),
        "properties": props,
    })
}

// ---- Expression evaluator ----

fn build_node_ctx(
    var_name: &str,
    rid: NodeRid,
    props: &HashMap<String, Value>,
) -> HashMap<String, Value> {
    let mut ctx: HashMap<String, Value> = HashMap::new();
    for (k, v) in props {
        ctx.insert(format!("{}.{}", var_name, k), v.clone());
    }
    ctx.insert(
        format!("{}.__rid_page", var_name),
        Value::from(rid.page_id().0),
    );
    ctx.insert(
        format!("{}.__rid_slot", var_name),
        Value::from(rid.slot_id().0),
    );
    ctx
}

pub fn eval_expr(
    expr: &decypher::ast::expr::Expression,
    ctx: &HashMap<String, Value>,
) -> Option<Value> {
    use decypher::ast::expr::{BinaryOperator, Expression, Literal, NumberLiteral, UnaryOperator};

    match expr {
        Expression::Literal(lit) => match lit {
            Literal::Number(n) => match n {
                NumberLiteral::Integer(i) => Some(Value::from(*i)),
                NumberLiteral::Float(f) => serde_json::Number::from_f64(*f).map(Value::Number),
            },
            Literal::String(s) => Some(Value::String(s.value.clone())),
            Literal::Boolean(b) => Some(Value::Bool(*b)),
            Literal::Null => Some(Value::Null),
            Literal::List(list) => {
                let items: Option<Vec<Value>> =
                    list.elements.iter().map(|e| eval_expr(e, ctx)).collect();
                items.map(Value::Array)
            }
            Literal::Map(map) => {
                let mut obj = serde_json::Map::new();
                for (k, v) in &map.entries {
                    if let Some(val) = eval_expr(v, ctx) {
                        obj.insert(k.name.name.clone(), val);
                    }
                }
                Some(Value::Object(obj))
            }
        },
        Expression::Variable(v) => {
            // If the variable itself (a node/edge object) is stored in the row,
            // return it directly — handles cases like `RETURN n` where the whole object is returned.
            ctx.get(&v.name.name).cloned()
        }
        Expression::PropertyLookup { base, property, .. } => {
            let base_key = expr_to_key(base);
            ctx.get(&format!("{}.{}", base_key, property.name.name))
                .cloned()
        }
        Expression::Comparison { lhs, operators, .. } => {
            use decypher::ast::expr::ComparisonOperator;
            let lv = eval_expr(lhs, ctx)?;
            let mut cur = lv;
            for (op, rhs_expr) in operators {
                let rv = eval_expr(rhs_expr, ctx)?;
                let ok = match op {
                    ComparisonOperator::Eq => values_eq(&cur, &rv),
                    ComparisonOperator::Ne => !values_eq(&cur, &rv),
                    ComparisonOperator::Lt => compare_values(&cur, &rv).is_lt(),
                    ComparisonOperator::Gt => compare_values(&cur, &rv).is_gt(),
                    ComparisonOperator::Le => compare_values(&cur, &rv).is_le(),
                    ComparisonOperator::Ge => compare_values(&cur, &rv).is_ge(),
                    ComparisonOperator::Contains => str_op(&cur, &rv, |a, b| a.contains(b)),
                    ComparisonOperator::StartsWith => str_op(&cur, &rv, |a, b| a.starts_with(b)),
                    ComparisonOperator::EndsWith => str_op(&cur, &rv, |a, b| a.ends_with(b)),
                    ComparisonOperator::RegexMatch => false,
                };
                if !ok {
                    return Some(Value::Bool(false));
                }
                cur = rv;
            }
            Some(Value::Bool(true))
        }
        Expression::BinaryOp { op, lhs, rhs, .. } => match op {
            BinaryOperator::And => Some(Value::Bool(eval_bool(lhs, ctx) && eval_bool(rhs, ctx))),
            BinaryOperator::Or => Some(Value::Bool(eval_bool(lhs, ctx) || eval_bool(rhs, ctx))),
            BinaryOperator::Xor => Some(Value::Bool(eval_bool(lhs, ctx) ^ eval_bool(rhs, ctx))),
            BinaryOperator::Add => numeric_op(
                &eval_expr(lhs, ctx)?,
                &eval_expr(rhs, ctx)?,
                |a, b| a + b,
                |a, b| a + b,
            ),
            BinaryOperator::Subtract => numeric_op(
                &eval_expr(lhs, ctx)?,
                &eval_expr(rhs, ctx)?,
                |a, b| a - b,
                |a, b| a - b,
            ),
            BinaryOperator::Multiply => numeric_op(
                &eval_expr(lhs, ctx)?,
                &eval_expr(rhs, ctx)?,
                |a, b| a * b,
                |a, b| a * b,
            ),
            BinaryOperator::Divide => numeric_op(
                &eval_expr(lhs, ctx)?,
                &eval_expr(rhs, ctx)?,
                |a, b| a / b,
                |a, b| a / b,
            ),
            _ => None,
        },
        Expression::UnaryOp { op, operand, .. } => match op {
            UnaryOperator::Not => Some(Value::Bool(!eval_bool(operand, ctx))),
            UnaryOperator::Negate => {
                let v = eval_expr(operand, ctx)?;
                match v {
                    Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            Some(Value::from(-i))
                        } else {
                            serde_json::Number::from_f64(-n.as_f64()?).map(Value::Number)
                        }
                    }
                    _ => None,
                }
            }
            UnaryOperator::Plus => eval_expr(operand, ctx),
        },
        Expression::IsNull {
            operand, negated, ..
        } => {
            let v = eval_expr(operand, ctx);
            let is_null = v.is_none_or(|v| v.is_null());
            Some(Value::Bool(if *negated { !is_null } else { is_null }))
        }
        Expression::Parenthesized(inner) => eval_expr(inner, ctx),
        _ => None,
    }
}

fn eval_bool(expr: &decypher::ast::expr::Expression, ctx: &HashMap<String, Value>) -> bool {
    match eval_expr(expr, ctx) {
        Some(Value::Bool(b)) => b,
        Some(Value::Null) | None => false,
        _ => true,
    }
}

fn eval_as_usize(expr: &decypher::ast::expr::Expression) -> usize {
    use decypher::ast::expr::{Expression, Literal, NumberLiteral};
    match expr {
        Expression::Literal(Literal::Number(NumberLiteral::Integer(i))) => *i as usize,
        _ => 0,
    }
}

// ---- Helpers ----

fn extract_first_label(labels: &[decypher::ast::pattern::LabelExpression]) -> Option<String> {
    labels.first().and_then(label_expr_name)
}

fn label_expr_name(label: &decypher::ast::pattern::LabelExpression) -> Option<String> {
    use decypher::ast::pattern::LabelExpression;
    match label {
        LabelExpression::Static(name) => Some(name.name.clone()),
        _ => None,
    }
}

fn extract_inline_props(
    props: &Option<decypher::ast::pattern::Properties>,
) -> Option<HashMap<String, Value>> {
    use decypher::ast::pattern::Properties;

    let map = match props {
        Some(Properties::Map(m)) => m,
        _ => return None,
    };

    let mut result: HashMap<String, Value> = HashMap::new();
    for (k, v) in &map.entries {
        let val = eval_expr(v, &HashMap::new()).unwrap_or(Value::Null);
        result.insert(k.name.name.clone(), val);
    }
    Some(result)
}

fn props_match(actual: &HashMap<String, Value>, expected: &HashMap<String, Value>) -> bool {
    expected
        .iter()
        .all(|(k, v)| actual.get(k).is_some_and(|av| values_eq(av, v)))
}

fn extract_rid_from_row(row: &HashMap<String, Value>, var_name: &str) -> Option<RecordId> {
    let page = row.get(&format!("{}.__rid_page", var_name))?;
    let slot = row.get(&format!("{}.__rid_slot", var_name))?;
    match (page, slot) {
        (Value::Number(p), Value::Number(s)) => {
            Some(RecordId::new(p.as_u64()? as u32, s.as_u64()? as u16))
        }
        _ => None,
    }
}

fn values_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(an), Value::Number(bn)) => {
            if let (Some(ai), Some(bi)) = (an.as_i64(), bn.as_i64()) {
                ai == bi
            } else if let (Some(af), Some(bf)) = (an.as_f64(), bn.as_f64()) {
                (af - bf).abs() < f64::EPSILON
            } else {
                false
            }
        }
        _ => a == b,
    }
}

fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Number(an), Value::Number(bn)) => {
            let af = an.as_f64().unwrap_or(0.0);
            let bf = bn.as_f64().unwrap_or(0.0);
            af.partial_cmp(&bf).unwrap_or(Ordering::Equal)
        }
        (Value::String(as_), Value::String(bs)) => as_.cmp(bs),
        (Value::Bool(ab), Value::Bool(bb)) => ab.cmp(bb),
        _ => Ordering::Equal,
    }
}

fn str_op<F: Fn(&str, &str) -> bool>(a: &Value, b: &Value, f: F) -> bool {
    match (a, b) {
        (Value::String(as_), Value::String(bs)) => f(as_, bs),
        _ => false,
    }
}

fn numeric_op(
    a: &Value,
    b: &Value,
    int_op: impl Fn(i64, i64) -> i64,
    float_op: impl Fn(f64, f64) -> f64,
) -> Option<Value> {
    match (a, b) {
        (Value::Number(an), Value::Number(bn)) => {
            if let (Some(ai), Some(bi)) = (an.as_i64(), bn.as_i64()) {
                Some(Value::from(int_op(ai, bi)))
            } else {
                serde_json::Number::from_f64(float_op(an.as_f64()?, bn.as_f64()?))
                    .map(Value::Number)
            }
        }
        _ => None,
    }
}

// ---- Database extension: neighbor lookup ----

type NeighborList = Vec<(EdgeRid, HashMap<String, Value>, NodeRid)>;

trait DatabaseExt {
    fn list_out_neighbors(
        &mut self,
        node_rid: NodeRid,
        edge_label: Option<&str>,
    ) -> Result<NeighborList, GraphError>;

    fn list_in_neighbors(
        &mut self,
        node_rid: NodeRid,
        edge_label: Option<&str>,
    ) -> Result<NeighborList, GraphError>;
}

impl DatabaseExt for Database {
    fn list_out_neighbors(
        &mut self,
        node_rid: NodeRid,
        edge_label: Option<&str>,
    ) -> Result<NeighborList, GraphError> {
        let all_edges = self.list_edges(edge_label)?;
        let mut result = Vec::new();
        for (edge_rid, edge_props) in all_edges {
            let (from, to) = self.get_edge_endpoints(edge_rid)?;
            if from == node_rid {
                result.push((edge_rid, edge_props, to));
            }
        }
        Ok(result)
    }

    fn list_in_neighbors(
        &mut self,
        node_rid: NodeRid,
        edge_label: Option<&str>,
    ) -> Result<NeighborList, GraphError> {
        let all_edges = self.list_edges(edge_label)?;
        let mut result = Vec::new();
        for (edge_rid, edge_props) in all_edges {
            let (from, to) = self.get_edge_endpoints(edge_rid)?;
            if to == node_rid {
                result.push((edge_rid, edge_props, from));
            }
        }
        Ok(result)
    }
}

// ---- Tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn setup_db() -> Database {
        let mut db = Database::open_in_memory().unwrap();
        db.apply_schema(
            "node Person { name: String age: Int }\nnode City { name: String }\nedge KNOWS { from: Person to: Person props: { since: String } }\nedge LIVES_IN { from: Person to: City }",
        )
        .unwrap();
        db
    }

    #[test]
    fn test_create_node() {
        let mut db = setup_db();
        let result =
            execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice", age: 30})"#).unwrap();
        assert!(matches!(
            result,
            CypherResult::Created { nodes: 1, edges: 0 }
        ));
    }

    #[test]
    fn test_match_return() {
        let mut db = setup_db();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice", age: 30})"#).unwrap();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Bob", age: 25})"#).unwrap();

        let result =
            execute_cypher_query(&mut db, "MATCH (n:Person) RETURN n.name, n.age").unwrap();
        if let CypherResult::Rows(rows) = result {
            assert_eq!(rows.len(), 2);
        } else {
            panic!("expected Rows");
        }
    }

    #[test]
    fn test_return_node_object() {
        let mut db = setup_db();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice", age: 30})"#).unwrap();

        let result = execute_cypher_query(&mut db, "MATCH (n:Person) RETURN n").unwrap();
        if let CypherResult::Rows(rows) = result {
            assert_eq!(rows.len(), 1);
            let n = rows[0].get("n").expect("n should be present");
            assert_eq!(n["labels"], serde_json::json!(["Person"]));
            assert_eq!(n["properties"]["name"], Value::String("Alice".into()));
            assert!(n["id"].is_string());
        } else {
            panic!("expected Rows");
        }
    }

    #[test]
    fn test_return_path_objects() {
        let mut db = setup_db();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice", age: 30})"#).unwrap();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Bob", age: 25})"#).unwrap();
        execute_cypher_query(
            &mut db,
            r#"MATCH (a:Person {name: "Alice"}), (b:Person {name: "Bob"}) CREATE (a)-[r:KNOWS {since: "2024"}]->(b)"#,
        )
        .unwrap();

        let result = execute_cypher_query(
            &mut db,
            "MATCH (a:Person {name: \"Alice\"})-[r:KNOWS]->(b:Person) RETURN a, r, b",
        )
        .unwrap();
        if let CypherResult::Rows(rows) = result {
            assert_eq!(rows.len(), 1);
            let row = &rows[0];

            let a = row.get("a").expect("a should be present");
            assert_eq!(a["properties"]["name"], Value::String("Alice".into()));
            let a_id = a["id"].as_str().unwrap().to_string();

            let b = row.get("b").expect("b should be present");
            assert_eq!(b["properties"]["name"], Value::String("Bob".into()));
            let b_id = b["id"].as_str().unwrap().to_string();

            let r = row.get("r").expect("r should be present");
            assert_eq!(r["type"], Value::String("KNOWS".into()));
            assert_eq!(r["properties"]["since"], Value::String("2024".into()));
            assert_eq!(r["source"], Value::String(a_id));
            assert_eq!(r["target"], Value::String(b_id));
        } else {
            panic!("expected Rows");
        }
    }

    #[test]
    fn test_match_where() {
        let mut db = setup_db();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice", age: 30})"#).unwrap();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Bob", age: 25})"#).unwrap();

        let result =
            execute_cypher_query(&mut db, "MATCH (n:Person) WHERE n.age > 28 RETURN n.name")
                .unwrap();
        if let CypherResult::Rows(rows) = result {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].get("n.name"), Some(&Value::String("Alice".into())));
        } else {
            panic!("expected Rows");
        }
    }

    #[test]
    fn test_delete_node() {
        let mut db = setup_db();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice"})"#).unwrap();

        let result =
            execute_cypher_query(&mut db, r#"MATCH (n:Person {name: "Alice"}) DELETE n"#).unwrap();
        assert!(matches!(result, CypherResult::Deleted(1)));

        let after = execute_cypher_query(&mut db, "MATCH (n:Person) RETURN n.name").unwrap();
        if let CypherResult::Rows(rows) = after {
            assert_eq!(rows.len(), 0);
        } else {
            panic!("expected Rows");
        }
    }

    #[test]
    fn test_set_property() {
        let mut db = setup_db();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice", age: 30})"#).unwrap();

        execute_cypher_query(
            &mut db,
            r#"MATCH (n:Person {name: "Alice"}) SET n.age = 31"#,
        )
        .unwrap();

        let result =
            execute_cypher_query(&mut db, r#"MATCH (n:Person {name: "Alice"}) RETURN n.age"#)
                .unwrap();
        if let CypherResult::Rows(rows) = result {
            assert_eq!(rows[0].get("n.age"), Some(&Value::from(31i64)));
        } else {
            panic!("expected Rows");
        }
    }

    #[test]
    fn test_limit_skip() {
        let mut db = setup_db();
        for i in 0..5i64 {
            execute_cypher_query(
                &mut db,
                &format!(r#"CREATE (n:Person {{name: "P{}", age: {}}})"#, i, i),
            )
            .unwrap();
        }

        let result = execute_cypher_query(
            &mut db,
            "MATCH (n:Person) RETURN n.name ORDER BY n.age SKIP 1 LIMIT 2",
        )
        .unwrap();
        if let CypherResult::Rows(rows) = result {
            assert_eq!(rows.len(), 2);
        } else {
            panic!("expected Rows");
        }
    }

    #[test]
    fn test_delete_relationship_does_not_delete_node() {
        // DELETE r on a relationship variable must delete the edge, not a node.
        let mut db = setup_db();
        execute_cypher_query(
            &mut db,
            r#"CREATE (a:Person {name: "Alice"})-[:KNOWS {since: "2020"}]->(b:Person {name: "Bob"})"#,
        )
        .unwrap();

        let result =
            execute_cypher_query(&mut db, "MATCH (a:Person)-[r:KNOWS]->(b:Person) DELETE r")
                .unwrap();
        assert!(
            matches!(result, CypherResult::Deleted(1)),
            "expected 1 deleted, got {result:?}"
        );

        // Both nodes must still exist.
        let nodes = execute_cypher_query(&mut db, "MATCH (n:Person) RETURN n.name").unwrap();
        if let CypherResult::Rows(rows) = nodes {
            assert_eq!(
                rows.len(),
                2,
                "both Person nodes should survive edge deletion"
            );
        } else {
            panic!("expected Rows");
        }
    }

    #[test]
    fn test_delete_node_appearing_in_multiple_rows() {
        // MATCH (u)-[:KNOWS]->(b) DELETE u where u has two outgoing edges
        // u should be deleted exactly once, not error on the second occurrence.
        let mut db = setup_db();
        execute_cypher_query(
            &mut db,
            r#"CREATE (u:Person {name: "Alice"})-[:KNOWS {since: "2020"}]->(b:Person {name: "Bob"})"#,
        )
        .unwrap();
        execute_cypher_query(
            &mut db,
            r#"CREATE (u:Person {name: "Alice"})-[:KNOWS {since: "2021"}]->(c:Person {name: "Carol"})"#,
        )
        .unwrap();

        // Alice should appear once (she was created twice as separate nodes in setup_db-less schema);
        // use a fresh scenario: one Alice with two KNOWS edges.
        let mut db2 = setup_db();
        execute_cypher_query(
            &mut db2,
            r#"CREATE (a:Person {name: "Alice"}), (b:Person {name: "Bob"}), (c:Person {name: "Carol"})"#,
        )
        .unwrap();
        execute_cypher_query(
            &mut db2,
            r#"MATCH (a:Person {name: "Alice"}), (b:Person {name: "Bob"}) CREATE (a)-[:KNOWS {since: "2020"}]->(b)"#,
        )
        .unwrap();
        execute_cypher_query(
            &mut db2,
            r#"MATCH (a:Person {name: "Alice"}), (c:Person {name: "Carol"}) CREATE (a)-[:KNOWS {since: "2021"}]->(c)"#,
        )
        .unwrap();

        // Alice appears in 2 match rows. DELETE u should succeed (count=1, not error).
        let result = execute_cypher_query(
            &mut db2,
            "MATCH (u:Person {name: \"Alice\"})-[:KNOWS]->(b:Person) DELETE u",
        )
        .unwrap();
        assert!(
            matches!(result, CypherResult::Deleted(1)),
            "expected 1 deleted, got {result:?}"
        );

        // Alice should be gone, Bob and Carol should remain.
        let remaining = execute_cypher_query(&mut db2, "MATCH (n:Person) RETURN n.name").unwrap();
        if let CypherResult::Rows(rows) = remaining {
            assert_eq!(rows.len(), 2, "Bob and Carol should remain");
        } else {
            panic!("expected Rows");
        }
    }

    #[test]
    fn test_delete_node_and_edge_same_rid_coords() {
        // DELETE r, a where r=(0,0) and a=(0,0): dedup must not skip the node delete.
        let mut db = setup_db();
        execute_cypher_query(
            &mut db,
            r#"CREATE (a:Person {name: "Alice"})-[:KNOWS {since: "2020"}]->(b:Person {name: "Bob"})"#,
        )
        .unwrap();

        // MATCH gives one row with a=(0,0) node and r=(0,0) edge.
        // DELETE r, a must delete the edge then the node (or vice versa), not skip a.
        let result = execute_cypher_query(
            &mut db,
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) DELETE r, a",
        )
        .unwrap();
        // 2 items deleted: the edge and the node
        assert!(
            matches!(result, CypherResult::Deleted(2)),
            "expected 2 deleted, got {result:?}"
        );

        // Alice should be gone.
        let remaining = execute_cypher_query(&mut db, "MATCH (n:Person) RETURN n.name").unwrap();
        if let CypherResult::Rows(rows) = remaining {
            assert_eq!(rows.len(), 1, "only Bob should remain");
        } else {
            panic!("expected Rows");
        }
    }

    #[test]
    fn test_delete_node_then_cascaded_edge_is_no_op() {
        // DELETE a, r where delete_node cascades r: the explicit DELETE r must not error.
        let mut db = setup_db();
        execute_cypher_query(
            &mut db,
            r#"CREATE (a:Person {name: "Alice"})-[:KNOWS {since: "2020"}]->(b:Person {name: "Bob"})"#,
        )
        .unwrap();

        // delete_node(a) cascades r; then delete_edge(r) should be a no-op, not an error.
        let result = execute_cypher_query(
            &mut db,
            "MATCH (a:Person {name: \"Alice\"})-[r:KNOWS]->(b:Person) DELETE a, r",
        )
        .unwrap();
        // node deleted = 1; cascaded edge also removed; explicit edge delete is skipped (exists=false)
        assert!(
            matches!(result, CypherResult::Deleted(1)),
            "expected 1 deleted (node only, edge cascaded), got {result:?}"
        );
    }

    #[test]
    fn test_set_edge_property() {
        // SET r.prop must update the edge, not a node.
        let mut db = setup_db();
        execute_cypher_query(
            &mut db,
            r#"CREATE (a:Person {name: "Alice"})-[:KNOWS {since: "2020"}]->(b:Person {name: "Bob"})"#,
        )
        .unwrap();

        let result = execute_cypher_query(
            &mut db,
            r#"MATCH (a:Person)-[r:KNOWS]->(b:Person) SET r.since = "2025""#,
        )
        .unwrap();
        assert!(
            matches!(result, CypherResult::Updated(1)),
            "expected Updated(1), got {result:?}"
        );

        // Edge property must be updated.
        let rows = execute_cypher_query(
            &mut db,
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN r.since",
        )
        .unwrap();
        if let CypherResult::Rows(rows) = rows {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].get("r.since"), Some(&Value::String("2025".into())));
        } else {
            panic!("expected Rows");
        }

        // Alice node must NOT be mutated.
        let alice_rows = execute_cypher_query(
            &mut db,
            r#"MATCH (n:Person {name: "Alice"}) RETURN n.since"#,
        )
        .unwrap();
        if let CypherResult::Rows(rows) = alice_rows {
            assert!(
                rows.is_empty()
                    || !rows[0].contains_key("n.since")
                    || rows[0]["n.since"] == Value::Null,
                "Alice node must not have a 'since' property"
            );
        } else {
            panic!("expected Rows");
        }
    }

    // ---- §2-1 regression tests: error contract for unsupported clause combinations ----

    #[test]
    fn test_create_then_set_is_invalid_command() {
        // CREATE ... SET must be rejected before any node is created (§2-2 preflight).
        let mut db = setup_db();
        let result = execute_cypher_query(
            &mut db,
            r#"CREATE (a:Person {name: "Alice", age: 30}) SET a.age = 31"#,
        );
        assert!(
            matches!(result, Err(GraphError::InvalidCommand(_))),
            "expected InvalidCommand, got {result:?}"
        );
        // No node must have been created (preflight must fire before CREATE executes).
        let rows = execute_cypher_query(&mut db, "MATCH (n:Person) RETURN n.name").unwrap();
        assert!(
            matches!(rows, CypherResult::Rows(ref r) if r.is_empty()),
            "no node must be created when the query is rejected"
        );
    }

    #[test]
    fn test_merge_then_set_is_invalid_command() {
        // MERGE ... SET must be rejected even when MERGE would match an existing node.
        let mut db = setup_db();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice", age: 30})"#).unwrap();

        let result = execute_cypher_query(
            &mut db,
            r#"MERGE (a:Person {name: "Alice"}) SET a.age = 99"#,
        );
        assert!(
            matches!(result, Err(GraphError::InvalidCommand(_))),
            "expected InvalidCommand, got {result:?}"
        );
        // Alice's age must remain 30.
        let rows =
            execute_cypher_query(&mut db, r#"MATCH (n:Person {name: "Alice"}) RETURN n.age"#)
                .unwrap();
        if let CypherResult::Rows(rows) = rows {
            assert_eq!(
                rows[0].get("n.age"),
                Some(&Value::from(30i64)),
                "age must not be mutated"
            );
        } else {
            panic!("expected Rows");
        }
    }

    #[test]
    fn test_multiple_match_is_invalid_command_and_data_intact() {
        // MATCH ... MATCH ... DELETE must be rejected and data must remain intact.
        let mut db = setup_db();
        execute_cypher_query(&mut db, r#"CREATE (a:Person {name: "Alice", age: 30})"#).unwrap();
        execute_cypher_query(&mut db, r#"CREATE (b:Person {name: "Bob", age: 25})"#).unwrap();

        let result = execute_cypher_query(&mut db, "MATCH (a:Person) MATCH (b:Person) DELETE a");
        assert!(
            matches!(result, Err(GraphError::InvalidCommand(_))),
            "expected InvalidCommand, got {result:?}"
        );
        // Both nodes must still exist.
        let rows = execute_cypher_query(&mut db, "MATCH (n:Person) RETURN n.name").unwrap();
        if let CypherResult::Rows(rows) = rows {
            assert_eq!(rows.len(), 2, "both nodes must be intact after rejection");
        } else {
            panic!("expected Rows");
        }
    }

    #[test]
    fn test_match_zero_rows_then_set_is_noop() {
        // MATCH that returns 0 rows followed by SET must return Ok(Updated(0)) — not an error.
        let mut db = setup_db();
        // No nodes in DB; MATCH (a:Person) returns 0 rows.
        let result = execute_cypher_query(&mut db, "MATCH (a:Person) SET a.age = 1").unwrap();
        assert!(
            matches!(result, CypherResult::Updated(0)),
            "expected Updated(0) no-op, got {result:?}"
        );
    }

    // ---- §2-3 regression test: unbound variable in SET/DELETE must error ----

    #[test]
    fn test_set_unbound_variable_is_invalid_command() {
        let mut db = setup_db();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice", age: 30})"#).unwrap();

        let result = execute_cypher_query(&mut db, "MATCH (a:Person) SET zzz.age = 1");
        assert!(
            matches!(result, Err(GraphError::InvalidCommand(_))),
            "expected InvalidCommand for unbound variable in SET, got {result:?}"
        );
    }

    #[test]
    fn test_delete_unbound_variable_is_invalid_command() {
        let mut db = setup_db();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice", age: 30})"#).unwrap();

        let result = execute_cypher_query(&mut db, "MATCH (a:Person) DELETE zzz");
        assert!(
            matches!(result, Err(GraphError::InvalidCommand(_))),
            "expected InvalidCommand for unbound variable in DELETE, got {result:?}"
        );
    }

    // ---- §3-1 regression: unbound check must fire even when MATCH returns 0 rows ----

    #[test]
    fn test_set_unbound_variable_zero_rows_is_invalid_command() {
        // DB is empty; MATCH returns 0 rows. Typo in SET target must still error.
        let mut db = setup_db();
        let result = execute_cypher_query(&mut db, "MATCH (a:Person) SET zzz.age = 1");
        assert!(
            matches!(result, Err(GraphError::InvalidCommand(_))),
            "expected InvalidCommand for unbound SET variable when MATCH returns 0 rows, got {result:?}"
        );
    }

    #[test]
    fn test_delete_unbound_variable_zero_rows_is_invalid_command() {
        // DB is empty; MATCH returns 0 rows. Typo in DELETE target must still error.
        let mut db = setup_db();
        let result = execute_cypher_query(&mut db, "MATCH (a:Person) DELETE zzz");
        assert!(
            matches!(result, Err(GraphError::InvalidCommand(_))),
            "expected InvalidCommand for unbound DELETE variable when MATCH returns 0 rows, got {result:?}"
        );
    }

    // ---- §3-2 regression: SET/DELETE targeting CREATE/MERGE variables must error ----

    #[test]
    fn test_set_create_variable_is_invalid_command() {
        // MATCH (a) CREATE (b) SET b.age: b was bound by CREATE, not MATCH.
        let mut db = setup_db();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice", age: 30})"#).unwrap();

        let result = execute_cypher_query(
            &mut db,
            r#"MATCH (a:Person) CREATE (b:Person {name: "Bob", age: 25}) SET b.age = 26"#,
        );
        assert!(
            matches!(result, Err(GraphError::InvalidCommand(_))),
            "expected InvalidCommand for SET targeting CREATE variable, got {result:?}"
        );
        // Bob must not have been created (preflight fires before CREATE).
        let rows = execute_cypher_query(&mut db, r#"MATCH (n:Person {name: "Bob"}) RETURN n.name"#)
            .unwrap();
        assert!(
            matches!(rows, CypherResult::Rows(ref r) if r.is_empty()),
            "Bob must not exist when the query is rejected by preflight"
        );
    }

    #[test]
    fn test_delete_create_variable_is_invalid_command() {
        // MATCH (a) CREATE (b) DELETE b: b was bound by CREATE, not MATCH.
        let mut db = setup_db();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice", age: 30})"#).unwrap();

        let result = execute_cypher_query(
            &mut db,
            r#"MATCH (a:Person) CREATE (b:Person {name: "Bob", age: 25}) DELETE b"#,
        );
        assert!(
            matches!(result, Err(GraphError::InvalidCommand(_))),
            "expected InvalidCommand for DELETE targeting CREATE variable, got {result:?}"
        );
    }

    // ---- cross-product (direct-product) MATCH tests ----

    #[test]
    fn test_match_multi_pattern_returns_cross_product() {
        // MATCH (c:Person {name: "Carol"}), (a:Person)-[r:KNOWS]->(b:Person)
        // must return a cross product: 1 Carol × 1 KNOWS edge = 1 row with both c.name and a.name.
        let mut db = setup_db();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice", age: 30})"#).unwrap();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Bob", age: 25})"#).unwrap();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Carol", age: 28})"#).unwrap();
        execute_cypher_query(
            &mut db,
            r#"MATCH (a:Person {name: "Alice"}), (b:Person {name: "Bob"}) CREATE (a)-[r:KNOWS {since: "2024"}]->(b)"#,
        )
        .unwrap();

        let result = execute_cypher_query(
            &mut db,
            r#"MATCH (c:Person {name: "Carol"}), (a:Person)-[r:KNOWS]->(b:Person) RETURN c.name, a.name"#,
        )
        .unwrap();

        if let CypherResult::Rows(rows) = result {
            assert_eq!(
                rows.len(),
                1,
                "cross product must yield 1 row, got {rows:?}"
            );
            assert_eq!(
                rows[0].get("c.name"),
                Some(&Value::String("Carol".into())),
                "c.name must be Carol"
            );
            assert_eq!(
                rows[0].get("a.name"),
                Some(&Value::String("Alice".into())),
                "a.name must be Alice"
            );
        } else {
            panic!("expected Rows");
        }
    }

    #[test]
    fn test_match_two_node_patterns_returns_cross_product() {
        // MATCH (a:Person), (b:Person) with 2 nodes must return 2×2=4 rows (self-pairs included).
        let mut db = setup_db();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice", age: 30})"#).unwrap();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Bob", age: 25})"#).unwrap();

        let result = execute_cypher_query(
            &mut db,
            "MATCH (a:Person), (b:Person) RETURN a.name, b.name",
        )
        .unwrap();

        if let CypherResult::Rows(rows) = result {
            assert_eq!(
                rows.len(),
                4,
                "2 nodes × 2 nodes = 4 cross-product rows, got {rows:?}"
            );
        } else {
            panic!("expected Rows");
        }
    }

    #[test]
    fn test_create_return_property() {
        let mut db = setup_db();
        let result = execute_cypher_query(
            &mut db,
            r#"CREATE (n:Person {name: "Alice", age: 30}) RETURN n.name, n.age"#,
        )
        .unwrap();
        if let CypherResult::Rows(rows) = result {
            assert_eq!(rows.len(), 1, "CREATE should return 1 row, got {rows:?}");
            assert_eq!(
                rows[0].get("n.name"),
                Some(&Value::String("Alice".into())),
                "n.name must be Alice"
            );
            assert_eq!(
                rows[0].get("n.age"),
                Some(&Value::Number(30.into())),
                "n.age must be 30"
            );
        } else {
            panic!("expected Rows, got {result:?}");
        }
    }

    #[test]
    fn test_create_return_node_object() {
        let mut db = setup_db();
        let result =
            execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice"}) RETURN n"#).unwrap();
        if let CypherResult::Rows(rows) = result {
            assert_eq!(rows.len(), 1, "CREATE should return 1 row, got {rows:?}");
            let node = rows[0].get("n").expect("n must be in row");
            assert!(node.is_object(), "n must be an object");
            let obj = node.as_object().unwrap();
            assert!(obj.contains_key("id"), "node object must have id");
            assert_eq!(
                obj.get("labels")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len()),
                Some(1),
                "node must have 1 label"
            );
            assert_eq!(
                obj.get("properties")
                    .and_then(|v| v.get("name"))
                    .and_then(|v| v.as_str()),
                Some("Alice"),
                "properties.name must be Alice"
            );
        } else {
            panic!("expected Rows, got {result:?}");
        }
    }

    #[test]
    fn test_create_path_return_all() {
        let mut db = setup_db();
        let result = execute_cypher_query(
            &mut db,
            r#"CREATE (a:Person {name: "Alice"})-[r:KNOWS {since: "2020"}]->(b:Person {name: "Bob"}) RETURN a.name, r, b.name"#,
        )
        .unwrap();
        if let CypherResult::Rows(rows) = result {
            assert_eq!(
                rows.len(),
                1,
                "path CREATE should return 1 row, got {rows:?}"
            );
            assert_eq!(
                rows[0].get("a.name"),
                Some(&Value::String("Alice".into())),
                "a.name must be Alice"
            );
            assert_eq!(
                rows[0].get("b.name"),
                Some(&Value::String("Bob".into())),
                "b.name must be Bob"
            );
            let edge = rows[0].get("r").expect("r must be in row");
            assert!(edge.is_object(), "r must be an object");
            let eobj = edge.as_object().unwrap();
            assert_eq!(
                eobj.get("type").and_then(|v| v.as_str()),
                Some("KNOWS"),
                "edge type must be KNOWS"
            );
        } else {
            panic!("expected Rows, got {result:?}");
        }
    }

    #[test]
    fn test_create_multi_pattern_return() {
        let mut db = setup_db();
        let result = execute_cypher_query(
            &mut db,
            r#"CREATE (a:Person {name: "Alice"}), (b:Person {name: "Bob"}) RETURN a.name, b.name"#,
        )
        .unwrap();
        if let CypherResult::Rows(rows) = result {
            assert_eq!(
                rows.len(),
                2,
                "2-pattern CREATE should return 2 rows, got {rows:?}"
            );
            let names: Vec<Option<&str>> = rows
                .iter()
                .map(|r| r.values().find_map(|v| v.as_str()))
                .collect();
            let has_alice = rows.iter().any(|r| {
                r.get("a.name")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "Alice")
                    .unwrap_or(false)
            });
            let has_bob = rows.iter().any(|r| {
                r.get("b.name")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "Bob")
                    .unwrap_or(false)
            });
            let _ = names;
            assert!(has_alice, "one row must have a.name = Alice");
            assert!(has_bob, "one row must have b.name = Bob");
        } else {
            panic!("expected Rows, got {result:?}");
        }
    }

    #[test]
    fn test_merge_return_new_node() {
        let mut db = setup_db();
        let result =
            execute_cypher_query(&mut db, r#"MERGE (n:Person {name: "Alice"}) RETURN n.name"#)
                .unwrap();
        if let CypherResult::Rows(rows) = result {
            assert_eq!(rows.len(), 1, "MERGE should return 1 row, got {rows:?}");
            assert_eq!(
                rows[0].get("n.name"),
                Some(&Value::String("Alice".into())),
                "n.name must be Alice"
            );
        } else {
            panic!("expected Rows, got {result:?}");
        }
    }

    #[test]
    fn test_merge_return_existing_node() {
        let mut db = setup_db();
        execute_cypher_query(&mut db, r#"CREATE (n:Person {name: "Alice", age: 30})"#).unwrap();
        let result =
            execute_cypher_query(&mut db, r#"MERGE (n:Person {name: "Alice"}) RETURN n.name"#)
                .unwrap();
        if let CypherResult::Rows(rows) = result {
            assert_eq!(
                rows.len(),
                1,
                "MERGE existing should return 1 row, got {rows:?}"
            );
            assert_eq!(
                rows[0].get("n.name"),
                Some(&Value::String("Alice".into())),
                "n.name must be Alice"
            );
        } else {
            panic!("expected Rows, got {result:?}");
        }
    }
}
