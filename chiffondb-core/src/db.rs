use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use crate::error::GraphError;
use crate::pathfinding::{ConnectingSubgraph, PathOptions, PathResult, PathfindingEngine};
use crate::schema::registry::SchemaRegistry;
use crate::schema::store::{load_schema, register_dynamic_type, DynamicTypeKind};
use crate::storage::file::{DatabaseFile, FileSnapshot, OpenOptions};
use crate::storage::index;
use crate::storage::page::{EdgeRid, NodeRid, RecordId};
use crate::storage::topology::TopologyStore;
use crate::storage::value::{decode_label_list, encode_label_list, PropertyStore};
use crate::traversal::executor::{EdgeId, GraphAccess, NodeId};

pub type NodeList = Vec<(NodeRid, HashMap<String, Value>)>;
pub type EdgeList = Vec<(EdgeRid, HashMap<String, Value>)>;

/// The result of resolving a label name: its type id and whether the registration was
/// newly created (`true`) or already existed (`false`).
/// `Serialize` so the FFI layer can return it as a JSON object string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct LabelAssignment {
    pub id: u16,
    pub created: bool,
}

pub(crate) struct DbSnapshot {
    pub(crate) topo: TopologyStore,
    pub(crate) file: FileSnapshot,
}

/// Main entry point for ChiffonDB.
/// Integrates DatabaseFile, TopologyStore, and PropertyStore. Property lookups
/// (`find`/`list`/`count` by type) scan the topology via the bounded page cache
/// rather than a resident index, so memory stays bounded by the cache budget.
pub struct Database {
    pub(crate) file: DatabaseFile,
    pub(crate) topo: TopologyStore,
}

impl Database {
    /// Creates a new database file with default options.
    pub fn create(path: &Path) -> Result<Self, GraphError> {
        Self::create_with_options(path, &OpenOptions::default())
    }

    /// Creates a new database file with the given options (e.g. memory budget).
    pub fn create_with_options(path: &Path, opts: &OpenOptions) -> Result<Self, GraphError> {
        let mut file = DatabaseFile::create_with_options(path, opts)?;
        // Pre-allocate the topology segment (pages 1..prop_start-1) with zero pages
        // so that PropertyStore only writes at prop_start and beyond.
        let prop_start = file.header.property_segment_start;
        let current = file.page_count()?;
        let empty = [0u8; crate::storage::page::PAGE_SIZE];
        for _ in current..prop_start {
            file.append_page(&empty)?;
        }
        let topo_start = file.header.topology_segment_start;
        file.flush()?;
        Ok(Self {
            file,
            topo: TopologyStore::new(topo_start, prop_start),
        })
    }

    /// Creates an in-memory database. Operates entirely in memory without any files.
    pub fn open_in_memory() -> Result<Self, GraphError> {
        let mut file = DatabaseFile::create_in_memory()?;
        let prop_start = file.header.property_segment_start;
        let topo_start = file.header.topology_segment_start;
        let empty = [0u8; crate::storage::page::PAGE_SIZE];
        for _ in 1..prop_start {
            file.append_page(&empty)?;
        }
        Ok(Self {
            file,
            topo: TopologyStore::new(topo_start, prop_start),
        })
    }

    /// Opens an existing database with default options.
    pub fn open(path: &Path) -> Result<Self, GraphError> {
        Self::open_with_options(path, &OpenOptions::default())
    }

    /// Opens an existing database with the given options (e.g. memory budget).
    /// Restores the topology segment metadata from the header.
    pub fn open_with_options(path: &Path, opts: &OpenOptions) -> Result<Self, GraphError> {
        let file = DatabaseFile::open_with_options(path, opts)?;

        let topo_start = file.header.topology_segment_start;
        let prop_start = file.header.property_segment_start;

        // Logical page counts come from the header (persisted at flush), not inferred from
        // the pre-allocated segment width — otherwise reopen would always restore the
        // segment-wide maximum and make scans walk the whole segment.
        let node_pages = file.header.node_page_count as usize;
        let edge_pages = file.header.edge_page_count as usize;
        let topo = TopologyStore::with_counts(topo_start, prop_start, node_pages, edge_pages);

        Ok(Self { file, topo })
    }

    /// Returns `(resident_pages, capacity)` of the page cache for the file backend,
    /// or `None` for the in-memory backend. Intended for memory-footprint diagnostics.
    pub fn cache_stats(&self) -> Option<(usize, usize)> {
        self.file.cache_stats()
    }

    /// Flushes pending writes to disk.
    ///
    /// Topology pages now live in the database file and are written through on every
    /// mutation (via the bounded page cache / WAL), so `flush` only needs to checkpoint;
    /// there is no separate in-memory topology to lay back into the segment. Segment
    /// capacity is enforced at allocation time (see `TopologyStore::alloc_*`).
    pub fn flush(&mut self) -> Result<(), GraphError> {
        // Logical page counts are persisted to the header (through the WAL) at the moment
        // a new logical page is allocated, so they are already durable here; flush only
        // needs to checkpoint.
        self.file.flush()
    }

    // ---- Internal raw helpers (use RecordId directly for intra-module code) ----

    fn get_node_properties_raw(
        &mut self,
        rid: RecordId,
    ) -> Result<HashMap<String, Value>, GraphError> {
        let node = self.topo.read_node(&mut self.file, rid)?;
        match node.property_ref {
            None => Ok(HashMap::new()),
            Some(pref) => PropertyStore::read(&mut self.file, pref),
        }
    }

    fn get_edge_properties_raw(
        &mut self,
        rid: RecordId,
    ) -> Result<HashMap<String, Value>, GraphError> {
        let edge = self.topo.read_edge(&mut self.file, rid)?;
        match edge.property_ref {
            None => Ok(HashMap::new()),
            Some(pref) => PropertyStore::read(&mut self.file, pref),
        }
    }

    // ---- Node operations ----

    /// Inserts a node and returns its NodeRid.
    pub fn insert_node(
        &mut self,
        type_id: u16,
        properties: HashMap<String, Value>,
    ) -> Result<NodeRid, GraphError> {
        let prop_ref = if properties.is_empty() {
            None
        } else {
            Some(PropertyStore::write(&mut self.file, &properties)?)
        };

        let rid = self.topo.alloc_node(&mut self.file, type_id)?;
        if let Some(pref) = prop_ref {
            let mut node = self.topo.read_node(&mut self.file, rid)?;
            node.property_ref = Some(pref);
            self.topo.write_node(&mut self.file, &node)?;
        }
        Ok(NodeRid(rid))
    }

    /// Reads node properties.
    pub fn get_node_properties(
        &mut self,
        rid: NodeRid,
    ) -> Result<HashMap<String, Value>, GraphError> {
        let rid = rid.0;
        let node = self.topo.read_node(&mut self.file, rid)?;
        match node.property_ref {
            None => Ok(HashMap::new()),
            Some(pref) => PropertyStore::read(&mut self.file, pref),
        }
    }

    /// Updates node properties (overwrites existing values).
    pub fn update_node_properties(
        &mut self,
        rid: NodeRid,
        properties: HashMap<String, Value>,
    ) -> Result<(), GraphError> {
        let rid = rid.0;
        if !self.node_exists_raw(rid) {
            return Err(GraphError::NodeNotFound(format!("{:?}", rid)));
        }
        let mut node = self.topo.read_node(&mut self.file, rid)?;
        match load_schema(&mut self.file) {
            Ok(ast) => {
                let (node_assignments, edge_assignments) =
                    crate::schema::store::load_type_assignments(&mut self.file)?;
                let registry = crate::schema::registry::SchemaRegistry::from_assignments(
                    &node_assignments,
                    &edge_assignments,
                );
                if let Some(type_name) = registry.node_type_name(node.node_type_id) {
                    if let Some(node_def) = ast.definitions.iter().find_map(|d| {
                        if let crate::schema::ast::Definition::Node(n) = d {
                            if n.name == type_name {
                                return Some(n);
                            }
                        }
                        None
                    }) {
                        crate::schema::validate_value::validate_properties(
                            type_name,
                            &node_def.fields,
                            &properties,
                        )?;
                    }
                }
            }
            Err(GraphError::SchemaError(_)) => {}
            Err(e) => return Err(e),
        }
        let new_pref = if properties.is_empty() {
            None
        } else {
            Some(PropertyStore::write(&mut self.file, &properties)?)
        };
        node.property_ref = new_pref;
        self.topo.write_node(&mut self.file, &node)?;
        Ok(())
    }

    /// Deletes a node and cascade-deletes all associated edges.
    pub fn delete_node(&mut self, rid: NodeRid) -> Result<(), GraphError> {
        let rid = rid.0;
        if !self.node_exists_raw(rid) {
            return Err(GraphError::NodeNotFound(format!("{:?}", rid)));
        }

        let out_edges: Vec<RecordId> = self
            .topo
            .collect_out_edges(&mut self.file, rid)?
            .iter()
            .map(|e| e.id)
            .collect();
        let in_edges: Vec<RecordId> = self
            .topo
            .collect_in_edges(&mut self.file, rid)?
            .iter()
            .map(|e| e.id)
            .collect();
        for eid in out_edges.into_iter().chain(in_edges) {
            self.topo.delete_edge(&mut self.file, eid)?;
        }
        self.topo.free_node(&mut self.file, rid)
    }

    // ---- Edge operations ----

    /// Inserts an edge and returns its EdgeRid.
    pub fn insert_edge(
        &mut self,
        type_id: u16,
        from: NodeRid,
        to: NodeRid,
        properties: HashMap<String, Value>,
    ) -> Result<EdgeRid, GraphError> {
        let from = from.0;
        let to = to.0;
        let prop_ref = if properties.is_empty() {
            None
        } else {
            Some(PropertyStore::write(&mut self.file, &properties)?)
        };

        let eid = self.topo.alloc_edge(&mut self.file, type_id, from, to)?;
        if let Some(pref) = prop_ref {
            let mut edge = self.topo.read_edge(&mut self.file, eid)?;
            edge.property_ref = Some(pref);
            self.topo.write_edge(&mut self.file, &edge)?;
        }
        self.topo.append_out_edge(&mut self.file, from, eid)?;
        self.topo.append_in_edge(&mut self.file, to, eid)?;
        Ok(EdgeRid(eid))
    }

    /// Reads edge properties.
    pub fn get_edge_properties(
        &mut self,
        rid: EdgeRid,
    ) -> Result<HashMap<String, Value>, GraphError> {
        let rid = rid.0;
        let edge = self.topo.read_edge(&mut self.file, rid)?;
        match edge.property_ref {
            None => Ok(HashMap::new()),
            Some(pref) => PropertyStore::read(&mut self.file, pref),
        }
    }

    /// Updates edge properties (overwrites existing values).
    pub fn update_edge_properties(
        &mut self,
        rid: EdgeRid,
        properties: HashMap<String, Value>,
    ) -> Result<(), GraphError> {
        let rid = rid.0;
        if !self.edge_exists_raw(rid) {
            return Err(GraphError::EdgeNotFound(format!("{:?}", rid)));
        }
        let edge = self.topo.read_edge(&mut self.file, rid)?;
        match load_schema(&mut self.file) {
            Ok(ast) => {
                let (node_assignments, edge_assignments) =
                    crate::schema::store::load_type_assignments(&mut self.file)?;
                let registry = crate::schema::registry::SchemaRegistry::from_assignments(
                    &node_assignments,
                    &edge_assignments,
                );
                if let Some(type_name) = registry.edge_type_name(edge.edge_type_id) {
                    if let Some(edge_def) = ast.definitions.iter().find_map(|d| {
                        if let crate::schema::ast::Definition::Edge(e) = d {
                            if e.name == type_name {
                                return Some(e);
                            }
                        }
                        None
                    }) {
                        crate::schema::validate_value::validate_properties(
                            type_name,
                            &edge_def.props,
                            &properties,
                        )?;
                    }
                }
            }
            Err(GraphError::SchemaError(_)) => {}
            Err(e) => return Err(e),
        }
        let new_pref = if properties.is_empty() {
            None
        } else {
            Some(PropertyStore::write(&mut self.file, &properties)?)
        };
        let mut edge = self.topo.read_edge(&mut self.file, rid)?;
        edge.property_ref = new_pref;
        self.topo.write_edge(&mut self.file, &edge)
    }

    /// Deletes an edge.
    pub fn delete_edge(&mut self, rid: EdgeRid) -> Result<(), GraphError> {
        let rid = rid.0;
        if !self.edge_exists_raw(rid) {
            return Err(GraphError::EdgeNotFound(format!("{:?}", rid)));
        }
        self.topo.delete_edge(&mut self.file, rid)
    }

    // ---- List operations ----

    /// Returns all node NodeRids and their properties. Filters by type_name when provided.
    /// When type_name is given, filters live nodes by type via a topology scan.
    pub fn list_nodes(&mut self, type_name: Option<&str>) -> Result<NodeList, GraphError> {
        if let Some(name) = type_name {
            let registry = self.load_schema_registry().ok();
            let type_id = registry.as_ref().and_then(|r| r.node_type_id(name));
            if let Some(tid) = type_id {
                let rids = index::rids_of_type(&self.topo, &mut self.file, tid)?;
                let mut result = Vec::new();
                for rid in rids {
                    let props = self.get_node_properties_raw(rid)?;
                    result.push((NodeRid(rid), props));
                }
                return Ok(result);
            }
        }

        // No type_name, or schema not applied — full scan
        let mut result = Vec::new();
        for rid in self.topo.live_node_rids(&mut self.file)? {
            let props = self.get_node_properties_raw(rid)?;
            result.push((NodeRid(rid), props));
        }
        Ok(result)
    }

    /// Returns all edge EdgeRids and their properties. Filters by type_name when provided.
    pub fn list_edges(&mut self, type_name: Option<&str>) -> Result<EdgeList, GraphError> {
        let registry = self.load_schema_registry().ok();
        let type_id_filter: Option<u16> =
            type_name.and_then(|name| registry.as_ref().and_then(|r| r.edge_type_id(name)));

        // If type_name is given but the type_id cannot be resolved, return an empty list
        if type_name.is_some() && type_id_filter.is_none() {
            return Ok(Vec::new());
        }

        let mut result = Vec::new();
        for rid in self.topo.live_edge_rids(&mut self.file)? {
            let edge = self.topo.read_edge(&mut self.file, rid)?;
            if let Some(filter_id) = type_id_filter {
                if edge.edge_type_id != filter_id {
                    continue;
                }
            }
            let props = self.get_edge_properties_raw(rid)?;
            result.push((EdgeRid(rid), props));
        }
        Ok(result)
    }

    // ---- Schema integration ----

    /// Parses and validates the schema text, then saves it to the database.
    pub fn apply_schema(&mut self, schema_text: &str) -> Result<(), GraphError> {
        let ast = crate::schema::parser::parse(schema_text)?;
        crate::schema::validator::validate(&ast)?;
        crate::schema::store::save_schema(&mut self.file, &ast)
    }

    /// Returns a SchemaRegistry built from the schema stored in the database.
    /// type_ids come from the persisted assignments, not from definition order,
    /// so they stay stable when types are added or removed.
    pub fn load_schema_registry(&mut self) -> Result<SchemaRegistry, GraphError> {
        let (nodes, edges) = crate::schema::store::load_type_assignments(&mut self.file)?;
        Ok(SchemaRegistry::from_assignments(&nodes, &edges))
    }

    /// Returns the schema stored in the database as DSL source text.
    /// Returns an error if no schema has been applied.
    pub fn get_schema_text(&mut self) -> Result<String, GraphError> {
        let ast = load_schema(&mut self.file)?;
        Ok(crate::schema::unparse::unparse(&ast))
    }

    /// Inserts a node by type name. Returns an error if no schema has been applied.
    pub fn insert_node_by_name(
        &mut self,
        type_name: &str,
        properties: HashMap<String, Value>,
    ) -> Result<NodeRid, GraphError> {
        let ast = load_schema(&mut self.file)?;
        let node_def = ast
            .definitions
            .iter()
            .find_map(|d| {
                if let crate::schema::ast::Definition::Node(n) = d {
                    if n.name == type_name {
                        return Some(n);
                    }
                }
                None
            })
            .ok_or_else(|| GraphError::SchemaError(format!("unknown node type: {type_name}")))?;
        crate::schema::validate_value::validate_properties(
            type_name,
            &node_def.fields,
            &properties,
        )?;
        let registry = crate::schema::store::load_type_assignments(&mut self.file)
            .map(|(n, e)| crate::schema::registry::SchemaRegistry::from_assignments(&n, &e))?;
        let type_id = registry
            .node_type_id(type_name)
            .ok_or_else(|| GraphError::SchemaError(format!("unknown node type: {type_name}")))?;
        self.insert_node(type_id, properties)
    }

    // ---- Edge multi-label operations ----

    /// Inserts an edge with multiple labels.
    pub fn insert_edge_with_labels(
        &mut self,
        primary_type: u16,
        from: NodeRid,
        to: NodeRid,
        additional_labels: Vec<u16>,
        properties: HashMap<String, Value>,
    ) -> Result<EdgeRid, GraphError> {
        let eid = self.insert_edge(primary_type, from, to, properties)?;
        if !additional_labels.is_empty() {
            self.set_edge_additional_labels(eid.0, &additional_labels)?;
        }
        Ok(eid)
    }

    /// Reads the additional label list (Vec of type_ids) for an edge.
    pub fn get_edge_additional_labels(&mut self, rid: EdgeRid) -> Result<Vec<u16>, GraphError> {
        let rid = rid.0;
        let edge = self.topo.read_edge(&mut self.file, rid)?;
        match edge.label_ref {
            None => Ok(vec![]),
            Some(lref) => {
                let bytes = PropertyStore::read_raw(&mut self.file, lref)?;
                decode_label_list(&bytes)
            }
        }
    }

    /// Returns all type_ids for an edge (primary type_id plus additional type_ids).
    pub fn get_edge_type_ids(&mut self, rid: EdgeRid) -> Result<Vec<u16>, GraphError> {
        let raw = rid.0;
        let edge = self.topo.read_edge(&mut self.file, raw)?;
        let mut ids = vec![edge.edge_type_id];
        ids.extend(self.get_edge_additional_labels(rid)?);
        Ok(ids)
    }

    /// Resolves and returns all label names for an edge using the schema.
    pub fn get_edge_label_names(&mut self, rid: EdgeRid) -> Result<Vec<String>, GraphError> {
        let registry = self.load_schema_registry()?;
        let ids = self.get_edge_type_ids(rid)?;
        Ok(ids
            .iter()
            .filter_map(|&id| registry.edge_type_name(id).map(|s| s.to_string()))
            .collect())
    }

    /// Adds an additional label to an edge (by type_id).
    pub fn add_edge_label(&mut self, rid: EdgeRid, type_id: u16) -> Result<(), GraphError> {
        let mut labels = self.get_edge_additional_labels(rid)?;
        if !labels.contains(&type_id) {
            labels.push(type_id);
            self.set_edge_additional_labels(rid.0, &labels)?;
        }
        Ok(())
    }

    /// Removes an additional label from an edge (by type_id).
    pub fn remove_edge_label(&mut self, rid: EdgeRid, type_id: u16) -> Result<(), GraphError> {
        let labels: Vec<u16> = self
            .get_edge_additional_labels(rid)?
            .into_iter()
            .filter(|&id| id != type_id)
            .collect();
        self.set_edge_additional_labels(rid.0, &labels)
    }

    /// Adds a label to an edge by type name.
    pub fn add_edge_label_by_name(
        &mut self,
        rid: EdgeRid,
        type_name: &str,
    ) -> Result<(), GraphError> {
        let registry = self.load_schema_registry()?;
        let type_id = registry
            .edge_type_id(type_name)
            .ok_or_else(|| GraphError::SchemaError(format!("unknown edge type: {type_name}")))?;
        self.add_edge_label(rid, type_id)
    }

    /// Adds an additional label to an edge by type name, registering the name dynamically
    /// if unknown. Lets an app attach user-defined edge kinds on top of a fixed edge type
    /// (whose from/to constraints stay schema-defined) without growing the schema.
    pub fn add_edge_label_dynamic(
        &mut self,
        rid: EdgeRid,
        type_name: &str,
    ) -> Result<LabelAssignment, GraphError> {
        let a = if let Some(id) = self.load_schema_registry()?.edge_type_id(type_name) {
            LabelAssignment { id, created: false }
        } else {
            let (id, created) =
                register_dynamic_type(&mut self.file, DynamicTypeKind::Edge, type_name)?;
            LabelAssignment { id, created }
        };
        self.add_edge_label(rid, a.id)?;
        Ok(a)
    }

    /// Removes a label from an edge by type name.
    pub fn remove_edge_label_by_name(
        &mut self,
        rid: EdgeRid,
        type_name: &str,
    ) -> Result<(), GraphError> {
        let registry = self.load_schema_registry()?;
        let type_id = registry
            .edge_type_id(type_name)
            .ok_or_else(|| GraphError::SchemaError(format!("unknown edge type: {type_name}")))?;
        self.remove_edge_label(rid, type_id)
    }

    /// Writes the additional label list to the property page and updates label_ref.
    ///
    /// Normalizes the list to a set before persisting: the primary edge_type_id is excluded
    /// and duplicates are dropped (first occurrence wins, preserving input order). This is the
    /// single write-through point for an edge's additional labels, so all callers share the
    /// invariant that they never repeat or shadow the primary edge type. Mirrors the node-side
    /// `set_additional_labels`.
    fn set_edge_additional_labels(
        &mut self,
        rid: RecordId,
        labels: &[u16],
    ) -> Result<(), GraphError> {
        let mut edge = self.topo.read_edge(&mut self.file, rid)?;
        let mut normalized = Vec::with_capacity(labels.len());
        for &id in labels {
            if id != edge.edge_type_id && !normalized.contains(&id) {
                normalized.push(id);
            }
        }
        if normalized.is_empty() {
            edge.label_ref = None;
        } else {
            let bytes = encode_label_list(&normalized)?;
            let lref = PropertyStore::write_raw(&mut self.file, &bytes)?;
            edge.label_ref = Some(lref);
        }
        self.topo.write_edge(&mut self.file, &edge)
    }

    /// Inserts an edge by type name. Returns an error if no schema has been applied.
    pub fn insert_edge_by_name(
        &mut self,
        type_name: &str,
        from: NodeRid,
        to: NodeRid,
        properties: HashMap<String, Value>,
    ) -> Result<EdgeRid, GraphError> {
        let from_raw = from.0;
        let to_raw = to.0;
        // Verify that both endpoint nodes exist (bitmap check, not just slot read).
        if !self.node_exists_raw(from_raw) {
            return Err(GraphError::NodeNotFound(format!(
                "from node {:?}",
                from_raw
            )));
        }
        if !self.node_exists_raw(to_raw) {
            return Err(GraphError::NodeNotFound(format!("to node {:?}", to_raw)));
        }
        let from_node = self.topo.read_node(&mut self.file, from_raw)?;
        let to_node = self.topo.read_node(&mut self.file, to_raw)?;

        let ast = load_schema(&mut self.file)?;
        let edge_def = ast
            .definitions
            .iter()
            .find_map(|d| {
                if let crate::schema::ast::Definition::Edge(e) = d {
                    if e.name == type_name {
                        return Some(e);
                    }
                }
                None
            })
            .ok_or_else(|| GraphError::SchemaError(format!("unknown edge type: {type_name}")))?;

        // Validate edge properties.
        crate::schema::validate_value::validate_properties(
            type_name,
            &edge_def.props,
            &properties,
        )?;

        // Validate from/to endpoint types.
        let (node_assignments, edge_assignments) =
            crate::schema::store::load_type_assignments(&mut self.file)?;
        let registry = crate::schema::registry::SchemaRegistry::from_assignments(
            &node_assignments,
            &edge_assignments,
        );
        let type_id = registry
            .edge_type_id(type_name)
            .ok_or_else(|| GraphError::SchemaError(format!("unknown edge type: {type_name}")))?;

        // Resolve the required node type name from the edge's from/to constraint.
        // Generic params (e.g. `T: User`) are resolved to their bound.
        let from_required = resolve_endpoint_type(&edge_def.from, &edge_def.generic_params);
        let to_required = resolve_endpoint_type(&edge_def.to, &edge_def.generic_params);

        if let Some(required_type) = from_required {
            let actual_type = registry
                .node_type_name(from_node.node_type_id)
                .unwrap_or("");
            if actual_type != required_type {
                return Err(GraphError::ValidationError(format!(
                    "edge {type_name}: 'from' node must be type '{required_type}', got '{actual_type}'"
                )));
            }
        }

        if let Some(required_type) = to_required {
            let actual_type = registry.node_type_name(to_node.node_type_id).unwrap_or("");
            if actual_type != required_type {
                return Err(GraphError::ValidationError(format!(
                    "edge {type_name}: 'to' node must be type '{required_type}', got '{actual_type}'"
                )));
            }
        }

        self.insert_edge(type_id, from, to, properties)
    }

    // ---- Node multi-label operations ----

    /// Inserts a node with multiple labels.
    pub fn insert_node_with_labels(
        &mut self,
        primary_type: u16,
        additional_labels: Vec<u16>,
        properties: HashMap<String, Value>,
    ) -> Result<NodeRid, GraphError> {
        let rid = self.insert_node(primary_type, properties)?;
        if !additional_labels.is_empty() {
            self.set_additional_labels(rid.0, &additional_labels)?;
        }
        Ok(rid)
    }

    /// Reads the additional label list (Vec of type_ids) for a node.
    pub fn get_additional_labels(&mut self, rid: NodeRid) -> Result<Vec<u16>, GraphError> {
        let rid = rid.0;
        let node = self.topo.read_node(&mut self.file, rid)?;
        match node.label_ref {
            None => Ok(vec![]),
            Some(lref) => {
                let bytes = PropertyStore::read_raw(&mut self.file, lref)?;
                decode_label_list(&bytes)
            }
        }
    }

    /// Returns all type_ids for a node (primary type_id plus additional type_ids).
    pub fn get_node_type_ids(&mut self, rid: NodeRid) -> Result<Vec<u16>, GraphError> {
        let raw = rid.0;
        let node = self.topo.read_node(&mut self.file, raw)?;
        let mut ids = vec![node.node_type_id];
        let extra = self.get_additional_labels(rid)?;
        ids.extend(extra);
        Ok(ids)
    }

    /// Resolves and returns all label names (primary + additional) for a node using the schema.
    pub fn get_node_label_names(&mut self, rid: NodeRid) -> Result<Vec<String>, GraphError> {
        let registry = self.load_schema_registry()?;
        let ids = self.get_node_type_ids(rid)?;
        let names = ids
            .iter()
            .filter_map(|&id| registry.node_type_name(id).map(|s| s.to_string()))
            .collect();
        Ok(names)
    }

    /// Adds an additional label to a node (by type_id). No-op if the label already exists.
    pub fn add_node_label(&mut self, rid: NodeRid, type_id: u16) -> Result<(), GraphError> {
        let mut labels = self.get_additional_labels(rid)?;
        if !labels.contains(&type_id) {
            labels.push(type_id);
            self.set_additional_labels(rid.0, &labels)?;
        }
        Ok(())
    }

    /// Removes an additional label from a node (by type_id).
    pub fn remove_node_label(&mut self, rid: NodeRid, type_id: u16) -> Result<(), GraphError> {
        let labels: Vec<u16> = self
            .get_additional_labels(rid)?
            .into_iter()
            .filter(|&id| id != type_id)
            .collect();
        self.set_additional_labels(rid.0, &labels)
    }

    /// Adds a label to a node by type name.
    pub fn add_node_label_by_name(
        &mut self,
        rid: NodeRid,
        type_name: &str,
    ) -> Result<(), GraphError> {
        let registry = self.load_schema_registry()?;
        let type_id = registry
            .node_type_id(type_name)
            .ok_or_else(|| GraphError::SchemaError(format!("unknown node type: {type_name}")))?;
        self.add_node_label(rid, type_id)
    }

    /// Removes a label from a node by type name.
    pub fn remove_node_label_by_name(
        &mut self,
        rid: NodeRid,
        type_name: &str,
    ) -> Result<(), GraphError> {
        let registry = self.load_schema_registry()?;
        let type_id = registry
            .node_type_id(type_name)
            .ok_or_else(|| GraphError::SchemaError(format!("unknown node type: {type_name}")))?;
        self.remove_node_label(rid, type_id)
    }

    /// Inserts a node with multiple labels specified by type names.
    /// Every label must already exist in the schema; an unknown name is an error.
    /// For apps that grow labels dynamically, use `insert_node_with_dynamic_labels`.
    pub fn insert_node_with_label_names(
        &mut self,
        primary_type_name: &str,
        additional_label_names: Vec<&str>,
        properties: HashMap<String, Value>,
    ) -> Result<NodeRid, GraphError> {
        let registry = self.load_schema_registry()?;
        let primary_id = registry.node_type_id(primary_type_name).ok_or_else(|| {
            GraphError::SchemaError(format!("unknown node type: {primary_type_name}"))
        })?;
        let additional_ids: Vec<u16> = additional_label_names
            .iter()
            .map(|name| {
                registry
                    .node_type_id(name)
                    .ok_or_else(|| GraphError::SchemaError(format!("unknown node type: {name}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.insert_node_with_labels(primary_id, additional_ids, properties)
    }

    /// Resolves a node type name to an id, registering it dynamically if unknown.
    /// Returns the id and whether it was newly created.
    fn resolve_or_register_node_type(&mut self, name: &str) -> Result<LabelAssignment, GraphError> {
        if let Some(id) = self.load_schema_registry()?.node_type_id(name) {
            return Ok(LabelAssignment { id, created: false });
        }
        let (id, created) = register_dynamic_type(&mut self.file, DynamicTypeKind::Node, name)?;
        Ok(LabelAssignment { id, created })
    }

    /// Inserts a node, registering any unknown label names on the fly. Returns the new node id
    /// and a map from each label name (primary + additional) to its assignment, so the caller
    /// can learn which ids were minted without a separate registration call.
    pub fn insert_node_with_dynamic_labels(
        &mut self,
        primary_type_name: &str,
        additional_label_names: Vec<&str>,
        properties: HashMap<String, Value>,
    ) -> Result<(NodeRid, HashMap<String, LabelAssignment>), GraphError> {
        let mut assignments = HashMap::new();

        let primary = self.resolve_or_register_node_type(primary_type_name)?;
        assignments.insert(primary_type_name.to_string(), primary);

        let mut additional_ids = Vec::new();
        for name in additional_label_names {
            let a = self.resolve_or_register_node_type(name)?;
            additional_ids.push(a.id);
            assignments.insert(name.to_string(), a);
        }

        let rid = self.insert_node_with_labels(primary.id, additional_ids, properties)?;
        Ok((rid, assignments))
    }

    /// Adds a label to a node by type name, registering the name dynamically if unknown.
    /// Returns the label's assignment (id and whether it was newly created).
    pub fn add_node_label_dynamic(
        &mut self,
        rid: NodeRid,
        type_name: &str,
    ) -> Result<LabelAssignment, GraphError> {
        let a = self.resolve_or_register_node_type(type_name)?;
        self.add_node_label(rid, a.id)?;
        Ok(a)
    }

    /// Writes the additional label list to the property page and updates label_ref.
    ///
    /// Normalizes the list to a set before persisting: the primary type_id is excluded and
    /// duplicates are dropped (first occurrence wins, preserving input order). This is the
    /// single write-through point for additional labels, so all callers share the invariant
    /// that a node's stored additional labels never repeat or shadow its primary type.
    fn set_additional_labels(&mut self, rid: RecordId, labels: &[u16]) -> Result<(), GraphError> {
        let mut node = self.topo.read_node(&mut self.file, rid)?;
        let mut normalized = Vec::with_capacity(labels.len());
        for &id in labels {
            if id != node.node_type_id && !normalized.contains(&id) {
                normalized.push(id);
            }
        }
        if normalized.is_empty() {
            node.label_ref = None;
        } else {
            let bytes = encode_label_list(&normalized)?;
            let lref = PropertyStore::write_raw(&mut self.file, &bytes)?;
            node.label_ref = Some(lref);
        }
        self.topo.write_node(&mut self.file, &node)
    }
}

// ---- GraphAccess lazy-load implementation ----

/// Wrapper passed to `execute()`.
///
/// Holds no node/edge-count-sized state: labels and property addresses are derived on demand
/// by reading the topology through the bounded page cache (the same pattern as the
/// `find`/`list`/`count` topology scans on `Database`). Only the schema registry — bounded by
/// the number of label *kinds*, not by node/edge count — is cached so that label lookups do not
/// reload the schema on every call. Property values are still cached lazily once read.
/// `RefCell` is used so that internal state can be mutated through a `&self` reference.
pub struct DatabaseGraphView<'a> {
    /// The database is wrapped in a `RefCell` so that topology/property reads (which require
    /// `&mut DatabaseFile`) can be performed through the `&self` methods of `GraphAccess`.
    /// Every accessor returns an owned value, so no borrow escapes the `RefCell`.
    db: RefCell<&'a mut Database>,
    /// Schema registry (type_id <-> name). Bounded by the number of label kinds.
    registry: Option<SchemaRegistry>,
    /// Node property cache (lazy-loaded)
    node_props: RefCell<HashMap<NodeId, HashMap<String, Value>>>,
    /// Edge property cache (lazy-loaded)
    edge_props: RefCell<HashMap<EdgeId, HashMap<String, Value>>>,
}

impl<'a> DatabaseGraphView<'a> {
    /// Builds a view from the database.
    ///
    /// Loads only the (bounded) schema registry; no per-node/edge state is materialized.
    pub fn load(db: &'a mut Database) -> Result<Self, GraphError> {
        let registry = db.load_schema_registry().ok();
        Ok(Self {
            db: RefCell::new(db),
            registry,
            node_props: RefCell::new(HashMap::new()),
            edge_props: RefCell::new(HashMap::new()),
        })
    }

    /// Resolves a node type_id to its label name, falling back to the numeric id.
    fn node_type_label(&self, type_id: u16) -> String {
        self.registry
            .as_ref()
            .and_then(|r| r.node_type_name(type_id))
            .map(|s| s.to_string())
            .unwrap_or_else(|| type_id.to_string())
    }

    /// Resolves an edge type_id to its label name, falling back to the numeric id.
    fn edge_type_label(&self, type_id: u16) -> String {
        self.registry
            .as_ref()
            .and_then(|r| r.edge_type_name(type_id))
            .map(|s| s.to_string())
            .unwrap_or_else(|| type_id.to_string())
    }

    /// Reads all labels (primary + additional) of a node by reading its topology record.
    ///
    /// The additional-label list is decoded inline from the record's `label_ref` rather than
    /// via `get_additional_labels`, which would re-read the node. With no `label_ref` (the
    /// common case: no extra labels) no further page read happens at all.
    fn read_node_all_labels(&self, id: NodeId) -> Vec<String> {
        let rid = id_to_rid(id);
        let mut db = self.db.borrow_mut();
        let db = &mut **db;
        let Ok(node) = db.topo.read_node(&mut db.file, rid) else {
            return vec![];
        };
        let mut labels = vec![self.node_type_label(node.node_type_id)];
        if let Some(lref) = node.label_ref {
            if let Ok(bytes) = PropertyStore::read_raw(&mut db.file, lref) {
                if let Ok(extra_ids) = decode_label_list(&bytes) {
                    labels.extend(extra_ids.into_iter().map(|id| self.node_type_label(id)));
                }
            }
        }
        labels
    }

    /// Reads all labels (primary + additional) of an edge by reading its topology record.
    ///
    /// Like `read_node_all_labels`, additional labels are decoded inline from `label_ref` to
    /// avoid a second `read_edge`.
    fn read_edge_all_labels(&self, id: EdgeId) -> Vec<String> {
        let rid = id_to_rid(id);
        let mut db = self.db.borrow_mut();
        let db = &mut **db;
        let Ok(edge) = db.topo.read_edge(&mut db.file, rid) else {
            return vec![];
        };
        let mut labels = vec![self.edge_type_label(edge.edge_type_id)];
        if let Some(lref) = edge.label_ref {
            if let Ok(bytes) = PropertyStore::read_raw(&mut db.file, lref) {
                if let Ok(extra_ids) = decode_label_list(&bytes) {
                    labels.extend(extra_ids.into_iter().map(|id| self.edge_type_label(id)));
                }
            }
        }
        labels
    }

    /// Searches for a node by key/value via a topology scan.
    pub fn find_node_by(&self, label: &str, key: &str, value: &Value) -> Option<NodeId> {
        // Resolve the label to a type_id via the registry; when no schema is registered the
        // label is the numeric type_id rendered as a string (the same fallback the label
        // accessors use), so parse it directly.
        let type_id = self
            .registry
            .as_ref()
            .and_then(|r| r.node_type_id(label))
            .or_else(|| label.parse::<u16>().ok())?;
        let mut db = self.db.borrow_mut();
        let db = &mut **db;
        let rid = index::find(&db.topo, &mut db.file, type_id, key, value).ok()??;
        Some(rid_to_id(rid))
    }

    /// Reads node properties, caching them on first access (file I/O only when not yet cached).
    /// The property address is read from the topology record on demand.
    fn load_node_props(&self, id: NodeId) {
        if self.node_props.borrow().contains_key(&id) {
            return;
        }
        let props = {
            let mut db = self.db.borrow_mut();
            let db = &mut **db;
            match db.topo.read_node(&mut db.file, id_to_rid(id)) {
                Ok(node) => match node.property_ref {
                    Some(pref) => PropertyStore::read(&mut db.file, pref).unwrap_or_default(),
                    None => HashMap::new(),
                },
                Err(_) => HashMap::new(),
            }
        };
        self.node_props.borrow_mut().insert(id, props);
    }

    /// Reads edge properties, caching them on first access (file I/O only when not yet cached).
    /// The property address is read from the topology record on demand.
    fn load_edge_props(&self, id: EdgeId) {
        if self.edge_props.borrow().contains_key(&id) {
            return;
        }
        let props = {
            let mut db = self.db.borrow_mut();
            let db = &mut **db;
            match db.topo.read_edge(&mut db.file, id_to_rid(id)) {
                Ok(edge) => match edge.property_ref {
                    Some(pref) => PropertyStore::read(&mut db.file, pref).unwrap_or_default(),
                    None => HashMap::new(),
                },
                Err(_) => HashMap::new(),
            }
        };
        self.edge_props.borrow_mut().insert(id, props);
    }
}

fn rid_to_id(rid: RecordId) -> u64 {
    ((rid.page_id.0 as u64) << 16) | rid.slot_id.0 as u64
}

fn id_to_rid(id: u64) -> RecordId {
    RecordId::new((id >> 16) as u32, (id & 0xFFFF) as u16)
}

impl<'a> GraphAccess for DatabaseGraphView<'a> {
    fn find_node(&self, label: &str, key: &str, value: &Value) -> Option<NodeId> {
        self.find_node_by(label, key, value)
    }

    fn node_properties(&self, id: NodeId) -> Option<Cow<'_, HashMap<String, Value>>> {
        self.load_node_props(id);
        self.node_props.borrow().get(&id).cloned().map(Cow::Owned)
    }

    fn node_label(&self, id: NodeId) -> Option<String> {
        let rid = id_to_rid(id);
        let mut db = self.db.borrow_mut();
        let db = &mut **db;
        let node = db.topo.read_node(&mut db.file, rid).ok()?;
        Some(self.node_type_label(node.node_type_id))
    }

    fn all_nodes_of_label(&self, label: &str) -> Vec<NodeId> {
        let rids = {
            let mut db = self.db.borrow_mut();
            let db = &mut **db;
            db.topo.live_node_rids(&mut db.file).unwrap_or_default()
        };
        rids.into_iter()
            .map(rid_to_id)
            .filter(|&nid| {
                label.is_empty() || self.read_node_all_labels(nid).iter().any(|l| l == label)
            })
            .collect()
    }

    fn all_edges_of_label(&self, label: &str) -> Vec<EdgeId> {
        let rids = {
            let mut db = self.db.borrow_mut();
            let db = &mut **db;
            db.topo.live_edge_rids(&mut db.file).unwrap_or_default()
        };
        rids.into_iter()
            .map(rid_to_id)
            .filter(|&eid| {
                label.is_empty() || self.read_edge_all_labels(eid).iter().any(|l| l == label)
            })
            .collect()
    }

    fn node_labels(&self, id: NodeId) -> Vec<String> {
        self.read_node_all_labels(id)
    }

    fn out_edges(&self, node_id: NodeId, label: Option<&str>) -> Vec<EdgeId> {
        let rid = id_to_rid(node_id);
        let eids: Vec<EdgeId> = {
            let mut db = self.db.borrow_mut();
            let db = &mut **db;
            db.topo
                .collect_out_edges(&mut db.file, rid)
                .unwrap_or_default()
                .iter()
                .map(|e| rid_to_id(e.id))
                .collect()
        };
        eids.into_iter()
            .filter(|&eid| match label {
                None => true,
                Some(l) => self.read_edge_all_labels(eid).iter().any(|el| el == l),
            })
            .collect()
    }

    fn in_edges(&self, node_id: NodeId, label: Option<&str>) -> Vec<EdgeId> {
        let rid = id_to_rid(node_id);
        let eids: Vec<EdgeId> = {
            let mut db = self.db.borrow_mut();
            let db = &mut **db;
            db.topo
                .collect_in_edges(&mut db.file, rid)
                .unwrap_or_default()
                .iter()
                .map(|e| rid_to_id(e.id))
                .collect()
        };
        eids.into_iter()
            .filter(|&eid| match label {
                None => true,
                Some(l) => self.read_edge_all_labels(eid).iter().any(|el| el == l),
            })
            .collect()
    }

    fn edge_to_node(&self, edge_id: EdgeId) -> Option<NodeId> {
        let rid = id_to_rid(edge_id);
        let mut db = self.db.borrow_mut();
        let db = &mut **db;
        let edge = db.topo.read_edge(&mut db.file, rid).ok()?;
        Some(rid_to_id(edge.to_node))
    }

    fn edge_from_node(&self, edge_id: EdgeId) -> Option<NodeId> {
        let rid = id_to_rid(edge_id);
        let mut db = self.db.borrow_mut();
        let db = &mut **db;
        let edge = db.topo.read_edge(&mut db.file, rid).ok()?;
        Some(rid_to_id(edge.from_node))
    }

    fn edge_label(&self, id: EdgeId) -> Option<String> {
        let rid = id_to_rid(id);
        let mut db = self.db.borrow_mut();
        let db = &mut **db;
        let edge = db.topo.read_edge(&mut db.file, rid).ok()?;
        Some(self.edge_type_label(edge.edge_type_id))
    }

    fn edge_properties(&self, id: EdgeId) -> Option<Cow<'_, HashMap<String, Value>>> {
        self.load_edge_props(id);
        self.edge_props.borrow().get(&id).cloned().map(Cow::Owned)
    }
}

// ---- VACUUM ----

impl Database {
    /// Physically removes deleted slots and compacts the database.
    /// For in-memory databases, simply reconstructs the database.
    pub fn vacuum(&mut self) -> Result<(), GraphError> {
        let mut new_db = if self.file.is_in_memory() {
            Database::open_in_memory()?
        } else {
            return Err(GraphError::SchemaError(
                "vacuum for file-backed DB must be called via vacuum_file()".to_string(),
            ));
        };

        if self.file.header.schema_root != 0 {
            if let Ok(ast) = crate::schema::store::load_schema(&mut self.file) {
                crate::schema::store::save_schema(&mut new_db.file, &ast)?;
            }
        }

        let mut rid_map: HashMap<RecordId, NodeRid> = HashMap::new();
        let node_rids = self.all_live_node_rids()?;
        for old_rid in node_rids {
            if let Ok(node) = self.topo.read_node(&mut self.file, old_rid) {
                let props = self.get_node_properties_raw(old_rid).unwrap_or_default();
                let new_rid = new_db.insert_node(node.node_type_id, props)?;
                rid_map.insert(old_rid, new_rid);
            }
        }

        let edge_rids = self.all_live_edge_rids()?;
        for old_eid in edge_rids {
            if let Ok(edge) = self.topo.read_edge(&mut self.file, old_eid) {
                if let (Some(&new_from), Some(&new_to)) =
                    (rid_map.get(&edge.from_node), rid_map.get(&edge.to_node))
                {
                    let props = self.get_edge_properties_raw(old_eid).unwrap_or_default();
                    new_db.insert_edge(edge.edge_type_id, new_from, new_to, props)?;
                }
            }
        }

        *self = new_db;
        Ok(())
    }

    /// VACUUM for a file-backed database.
    pub fn vacuum_file(path: &Path) -> Result<(), GraphError> {
        let mut db = Database::open(path)?;
        let tmp_path = path.with_extension("tdb.vacuum");
        let mut new_db = Database::create(&tmp_path)?;

        if db.file.header.schema_root != 0 {
            if let Ok(ast) = crate::schema::store::load_schema(&mut db.file) {
                crate::schema::store::save_schema(&mut new_db.file, &ast)?;
            }
        }

        let mut rid_map: HashMap<RecordId, NodeRid> = HashMap::new();
        let node_rids = db.all_live_node_rids()?;
        for old_rid in node_rids {
            if let Ok(node) = db.topo.read_node(&mut db.file, old_rid) {
                let props = db.get_node_properties_raw(old_rid).unwrap_or_default();
                let new_rid = new_db.insert_node(node.node_type_id, props)?;
                rid_map.insert(old_rid, new_rid);
            }
        }

        let edge_rids = db.all_live_edge_rids()?;
        for old_eid in edge_rids {
            if let Ok(edge) = db.topo.read_edge(&mut db.file, old_eid) {
                if let (Some(&new_from), Some(&new_to)) =
                    (rid_map.get(&edge.from_node), rid_map.get(&edge.to_node))
                {
                    let props = db.get_edge_properties_raw(old_eid).unwrap_or_default();
                    new_db.insert_edge(edge.edge_type_id, new_from, new_to, props)?;
                }
            }
        }

        new_db.flush()?;
        drop(new_db);
        drop(db);
        std::fs::rename(&tmp_path, path).map_err(GraphError::Io)?;
        Ok(())
    }

    fn all_live_node_rids(&mut self) -> Result<Vec<RecordId>, GraphError> {
        self.topo.live_node_rids(&mut self.file)
    }

    fn all_live_edge_rids(&mut self) -> Result<Vec<RecordId>, GraphError> {
        self.topo.live_edge_rids(&mut self.file)
    }

    /// Returns the primary type name of a node. Falls back to the string form of type_id when no schema is applied.
    pub fn get_node_type_name(&mut self, rid: NodeRid) -> Result<String, GraphError> {
        let rid = rid.0;
        let node = self.topo.read_node(&mut self.file, rid)?;
        let registry = self.load_schema_registry().ok();
        Ok(registry
            .as_ref()
            .and_then(|r| r.node_type_name(node.node_type_id))
            .map(|s| s.to_string())
            .unwrap_or_else(|| node.node_type_id.to_string()))
    }

    /// Returns the primary type name of an edge. Falls back to the string form of type_id when no schema is applied.
    pub fn get_edge_type_name(&mut self, eid: EdgeRid) -> Result<String, GraphError> {
        let eid = eid.0;
        let edge = self.topo.read_edge(&mut self.file, eid)?;
        let registry = self.load_schema_registry().ok();
        Ok(registry
            .as_ref()
            .and_then(|r| r.edge_type_name(edge.edge_type_id))
            .map(|s| s.to_string())
            .unwrap_or_else(|| edge.edge_type_id.to_string()))
    }

    // ---- Existence check ----

    /// Returns true if a node with the given NodeRid exists.
    pub fn node_exists(&mut self, rid: NodeRid) -> bool {
        self.node_exists_raw(rid.0)
    }

    /// Returns true if an edge with the given EdgeRid exists.
    pub fn edge_exists(&mut self, rid: EdgeRid) -> bool {
        self.edge_exists_raw(rid.0)
    }

    fn node_exists_raw(&mut self, rid: RecordId) -> bool {
        self.topo.node_slot_used(&mut self.file, rid)
    }

    fn edge_exists_raw(&mut self, rid: RecordId) -> bool {
        self.topo.edge_slot_used(&mut self.file, rid)
    }

    // ---- Count ----

    /// Returns the number of nodes, optionally filtered by type_name.
    pub fn count_nodes(&mut self, type_name: Option<&str>) -> Result<u64, GraphError> {
        if let Some(name) = type_name {
            let registry = self.load_schema_registry().ok();
            if let Some(tid) = registry.as_ref().and_then(|r| r.node_type_id(name)) {
                return Ok(index::rids_of_type(&self.topo, &mut self.file, tid)?.len() as u64);
            }
            return Ok(0);
        }
        Ok(self.topo.live_node_rids(&mut self.file)?.len() as u64)
    }

    /// Returns the number of edges, optionally filtered by type_name.
    pub fn count_edges(&mut self, type_name: Option<&str>) -> Result<u64, GraphError> {
        let type_id_filter: Option<u16> = if let Some(name) = type_name {
            let registry = self.load_schema_registry().ok();
            let tid = registry.as_ref().and_then(|r| r.edge_type_id(name));
            if tid.is_none() {
                return Ok(0);
            }
            tid
        } else {
            None
        };

        let mut count: u64 = 0;
        for rid in self.topo.live_edge_rids(&mut self.file)? {
            if let Some(filter_id) = type_id_filter {
                if let Ok(edge) = self.topo.read_edge(&mut self.file, rid) {
                    if edge.edge_type_id != filter_id {
                        continue;
                    }
                }
            }
            count += 1;
        }
        Ok(count)
    }

    // ---- Patch properties ----

    /// Updates only the specified keys in a node's properties. Unspecified keys are preserved.
    pub fn patch_node_properties(
        &mut self,
        rid: NodeRid,
        patch: HashMap<String, Value>,
    ) -> Result<(), GraphError> {
        let mut props = self.get_node_properties(rid)?;
        for (k, v) in patch {
            props.insert(k, v);
        }
        self.update_node_properties(rid, props)
    }

    /// Updates only the specified keys in an edge's properties. Unspecified keys are preserved.
    pub fn patch_edge_properties(
        &mut self,
        rid: EdgeRid,
        patch: HashMap<String, Value>,
    ) -> Result<(), GraphError> {
        let mut props = self.get_edge_properties(rid)?;
        for (k, v) in patch {
            props.insert(k, v);
        }
        self.update_edge_properties(rid, props)
    }

    // ---- Schema query ----

    /// Returns the names of all node types defined in the schema.
    pub fn node_type_names(&mut self) -> Result<Vec<String>, GraphError> {
        let ast = load_schema(&mut self.file)?;
        Ok(ast
            .definitions
            .iter()
            .filter_map(|d| {
                if let crate::schema::ast::Definition::Node(n) = d {
                    Some(n.name.clone())
                } else {
                    None
                }
            })
            .collect())
    }

    /// Returns the names of all edge types defined in the schema.
    pub fn edge_type_names(&mut self) -> Result<Vec<String>, GraphError> {
        let ast = load_schema(&mut self.file)?;
        Ok(ast
            .definitions
            .iter()
            .filter_map(|d| {
                if let crate::schema::ast::Definition::Edge(e) = d {
                    Some(e.name.clone())
                } else {
                    None
                }
            })
            .collect())
    }

    /// Returns the property definitions for a node type.
    pub fn node_type_schema(
        &mut self,
        type_name: &str,
    ) -> Result<Option<NodeTypeSchema>, GraphError> {
        let ast = load_schema(&mut self.file)?;
        Ok(ast.definitions.iter().find_map(|d| {
            if let crate::schema::ast::Definition::Node(n) = d {
                if n.name == type_name {
                    return Some(NodeTypeSchema {
                        name: n.name.clone(),
                        properties: n.fields.iter().map(PropertyDef::from_field).collect(),
                    });
                }
            }
            None
        }))
    }

    /// Returns the property definitions for an edge type.
    pub fn edge_type_schema(
        &mut self,
        type_name: &str,
    ) -> Result<Option<EdgeTypeSchema>, GraphError> {
        let ast = load_schema(&mut self.file)?;
        Ok(ast.definitions.iter().find_map(|d| {
            if let crate::schema::ast::Definition::Edge(e) = d {
                if e.name == type_name {
                    return Some(EdgeTypeSchema {
                        name: e.name.clone(),
                        properties: e.props.iter().map(PropertyDef::from_field).collect(),
                    });
                }
            }
            None
        }))
    }

    // ---- Shortest path ----

    /// Returns the shortest path using BFS. Returns `Ok(None)` if no path exists.
    pub fn shortest_path(
        &mut self,
        from: RecordId,
        to: RecordId,
        options: PathOptions,
    ) -> Result<Option<PathResult>, GraphError> {
        let mut engine = PathfindingEngine::new(&self.topo, &mut self.file);
        engine.shortest_path(from, to, &options)
    }

    /// Returns the connecting subgraph between multiple nodes.
    pub fn connecting_subgraph(
        &mut self,
        node_rids: Vec<RecordId>,
        options: PathOptions,
    ) -> Result<ConnectingSubgraph, GraphError> {
        let mut engine = PathfindingEngine::new(&self.topo, &mut self.file);
        engine.connecting_subgraph(&node_rids, &options)
    }

    /// Returns the source and destination node NodeRids for an edge.
    pub fn get_edge_endpoints(&mut self, eid: EdgeRid) -> Result<(NodeRid, NodeRid), GraphError> {
        let eid = eid.0;
        let edge = self.topo.read_edge(&mut self.file, eid)?;
        Ok((NodeRid(edge.from_node), NodeRid(edge.to_node)))
    }

    // ---- Transaction support ----

    /// Takes a snapshot of current mutable state for transaction rollback.
    pub(crate) fn take_snapshot(&self) -> DbSnapshot {
        DbSnapshot {
            topo: self.topo.clone(),
            file: self.file.snapshot(),
        }
    }

    /// Restores state from a snapshot (rollback).
    pub(crate) fn restore_snapshot(&mut self, snap: DbSnapshot) {
        self.topo = snap.topo;
        // `file.restore` reverts the WAL, page cache, logical page count, and the
        // in-memory header (including topology page counts) together, so no manual
        // header re-sync is needed here — the file layer is the single source of truth.
        // Property lookups scan the topology, so there is no separate index to restore;
        // once topology/WAL are rolled back, find/list/count results follow automatically.
        self.file.restore(snap.file);
    }

    // ---- Integrity check ----

    /// Checks database integrity and returns a list of issues. Returns an empty list if no issues are found.
    pub fn verify(&mut self) -> Result<Vec<String>, GraphError> {
        let mut warnings = Vec::new();
        let node_rids: std::collections::HashSet<RecordId> =
            self.all_live_node_rids()?.into_iter().collect();

        for eid in self.all_live_edge_rids()? {
            if let Ok(edge) = self.topo.read_edge(&mut self.file, eid) {
                if !node_rids.contains(&edge.from_node) {
                    warnings.push(format!(
                        "edge {:?}: from_node {:?} does not exist",
                        eid, edge.from_node
                    ));
                }
                if !node_rids.contains(&edge.to_node) {
                    warnings.push(format!(
                        "edge {:?}: to_node {:?} does not exist",
                        eid, edge.to_node
                    ));
                }
            }
        }

        for &nid in &node_rids {
            if let Ok(node) = self.topo.read_node(&mut self.file, nid) {
                let mut visited = std::collections::HashSet::new();
                let mut cur = node.first_out_edge;
                while let Some(eid) = cur {
                    if !visited.insert(eid) {
                        warnings.push(format!("node {:?}: cycle in out_edge chain", nid));
                        break;
                    }
                    cur = self
                        .topo
                        .read_edge(&mut self.file, eid)
                        .ok()
                        .and_then(|e| e.next_out_edge);
                }

                let mut visited = std::collections::HashSet::new();
                let mut cur = node.first_in_edge;
                while let Some(eid) = cur {
                    if !visited.insert(eid) {
                        warnings.push(format!("node {:?}: cycle in in_edge chain", nid));
                        break;
                    }
                    cur = self
                        .topo
                        .read_edge(&mut self.file, eid)
                        .ok()
                        .and_then(|e| e.next_in_edge);
                }
            }
        }

        // No separate property index exists anymore (lookups scan the topology), so there
        // is no index/topology consistency check to perform here.
        Ok(warnings)
    }
}

/// Returns the concrete node type name required by an edge endpoint constraint.
/// `Named(s)` where `s` matches a generic param is resolved to the param's bound.
/// Returns `None` if the constraint cannot be reduced to a concrete type name
/// (e.g. a structural type or an unbound generic).
fn resolve_endpoint_type<'a>(
    type_expr: &'a crate::schema::ast::TypeExpr,
    generic_params: &'a [crate::schema::ast::BoundParam],
) -> Option<&'a str> {
    match type_expr {
        crate::schema::ast::TypeExpr::Named(name) => {
            if let Some(bp) = generic_params.iter().find(|p| &p.name == name) {
                bp.bound.as_deref()
            } else {
                Some(name.as_str())
            }
        }
        crate::schema::ast::TypeExpr::NodeRef(name) => Some(name.as_str()),
        _ => None,
    }
}

// ---- Schema query types ----

pub struct NodeTypeSchema {
    pub name: String,
    pub properties: Vec<PropertyDef>,
}

pub struct EdgeTypeSchema {
    pub name: String,
    pub properties: Vec<PropertyDef>,
}

pub struct PropertyDef {
    pub name: String,
    pub value_type: String,
    pub nullable: bool,
}

impl PropertyDef {
    fn from_field(field: &crate::schema::ast::FieldDef) -> Self {
        use crate::schema::ast::TypeExpr;
        let (value_type, nullable) = match &field.type_expr {
            TypeExpr::Int => ("Int".to_string(), false),
            TypeExpr::Float => ("Float".to_string(), false),
            TypeExpr::Boolean => ("Boolean".to_string(), false),
            TypeExpr::DateTime => ("DateTime".to_string(), false),
            TypeExpr::String => ("String".to_string(), false),
            TypeExpr::Json => ("Json".to_string(), false),
            TypeExpr::Blob => ("Blob".to_string(), false),
            TypeExpr::Vector(n) => (format!("Vector({n})"), false),
            TypeExpr::List(inner) => (format!("List<{inner:?}>"), false),
            TypeExpr::Map(k, v) => (format!("Map<{k:?},{v:?}>"), false),
            TypeExpr::NodeRef(n) => (format!("NodeRef({n})"), false),
            TypeExpr::EdgeRef(n) => (format!("EdgeRef({n})"), false),
            TypeExpr::Named(n) => (n.clone(), false),
        };
        Self {
            name: field.name.clone(),
            value_type,
            nullable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::NamedTempFile;

    fn make_db() -> (Database, std::path::PathBuf) {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.into_temp_path().to_path_buf();
        std::fs::remove_file(&path).ok();
        let db = Database::create(&path).unwrap();
        (db, path)
    }

    #[test]
    fn bounded_cache_full_stack_roundtrip() {
        // Drive the whole stack (topology + property store + WAL + bounded cache)
        // with a deliberately tiny cache so most pages are evicted. Everything must
        // still read back correctly, proving reads fall through to WAL/main file.
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.into_temp_path().to_path_buf();
        std::fs::remove_file(&path).ok();
        let opts = OpenOptions {
            max_memory_bytes: crate::storage::page::PAGE_SIZE, // capacity = 1
        };
        let mut db = Database::create_with_options(&path, &opts).unwrap();

        let mut rids = Vec::new();
        for i in 0..100 {
            let props = HashMap::from([("id".to_string(), json!(format!("u{i}")))]);
            rids.push(db.insert_node(1, props).unwrap());
        }
        // Chain them with edges.
        for w in rids.windows(2) {
            db.insert_edge(1, w[0], w[1], HashMap::new()).unwrap();
        }
        db.flush().unwrap();

        for (i, &rid) in rids.iter().enumerate() {
            let props = db.get_node_properties(rid).unwrap();
            assert_eq!(props["id"], json!(format!("u{i}")));
        }
        assert_eq!(db.count_nodes(None).unwrap(), 100);
        assert_eq!(db.count_edges(None).unwrap(), 99);

        // Reopen with a tiny cache and re-verify durability.
        drop(db);
        let mut db2 = Database::open_with_options(&path, &opts).unwrap();
        assert_eq!(db2.count_nodes(None).unwrap(), 100);
        let last = *rids.last().unwrap();
        assert_eq!(db2.get_node_properties(last).unwrap()["id"], json!("u99"));
    }

    #[test]
    fn bounded_cache_shortest_path() {
        // Pathfinding now reads topology records through the bounded page cache.
        // With capacity 1, every hop faults a fresh page; the BFS must still find
        // the correct path along a 30-node chain.
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.into_temp_path().to_path_buf();
        std::fs::remove_file(&path).ok();
        let opts = OpenOptions {
            max_memory_bytes: crate::storage::page::PAGE_SIZE, // capacity = 1
        };
        let mut db = Database::create_with_options(&path, &opts).unwrap();

        let mut rids = Vec::new();
        for _ in 0..30 {
            rids.push(db.insert_node(1, HashMap::new()).unwrap());
        }
        for w in rids.windows(2) {
            db.insert_edge(1, w[0], w[1], HashMap::new()).unwrap();
        }
        db.flush().unwrap();

        let opts_path = crate::pathfinding::PathOptions {
            max_depth: 40,
            direction: crate::pathfinding::PathDirection::Outgoing,
            edge_type_ids: None,
        };
        let result = db
            .shortest_path(rids[0].0, rids[29].0, opts_path)
            .unwrap()
            .expect("path exists");
        assert_eq!(result.node_rids.len(), 30);
        assert_eq!(result.edge_rids.len(), 29);
    }

    #[test]
    fn bounded_cache_find_and_list_by_scan() {
        // Property lookups now scan the topology (no resident index). With a 1-page cache
        // every scanned node faults through the bounded cache; find/list/count must still
        // be correct.
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.into_temp_path().to_path_buf();
        std::fs::remove_file(&path).ok();
        let opts = OpenOptions {
            max_memory_bytes: crate::storage::page::PAGE_SIZE, // capacity = 1
        };
        let mut db = Database::create_with_options(&path, &opts).unwrap();

        let mut rids = Vec::new();
        for i in 0..50 {
            let props = HashMap::from([("id".to_string(), json!(format!("u{i}")))]);
            rids.push(db.insert_node(1, props).unwrap());
        }
        db.flush().unwrap();

        // find via scan returns the right node for each id.
        for (i, &rid) in rids.iter().enumerate() {
            let found = crate::storage::index::find(
                &db.topo,
                &mut db.file,
                1,
                "id",
                &json!(format!("u{i}")),
            )
            .unwrap();
            assert_eq!(found, Some(rid.0));
        }
        // A missing value yields None.
        assert_eq!(
            crate::storage::index::find(&db.topo, &mut db.file, 1, "id", &json!("absent")).unwrap(),
            None
        );
        // list/count by type are scan-backed and correct under the tiny cache.
        assert_eq!(db.count_nodes(None).unwrap(), 50);
        assert_eq!(
            crate::storage::index::rids_of_type(&db.topo, &mut db.file, 1)
                .unwrap()
                .len(),
            50
        );
    }

    #[test]
    fn bounded_cache_graph_view_all_nodes_has_label_traverse() {
        // DatabaseGraphView no longer keeps node/edge-count-sized label maps; labels and
        // property addresses are derived on demand from the topology. With a 1-page cache
        // every derivation faults a fresh page, so this pins down that an `AllNodes` start
        // followed by a `HasLabel` filter and an out-edge/out-node traversal still produces
        // the same result as before the bounded-ification.
        use crate::traversal::command::{
            CollectResult, CollectSpec, StartSpec, TraversalAction, TraversalCommand, TraversalStep,
        };
        use crate::traversal::executor::execute;

        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.into_temp_path().to_path_buf();
        std::fs::remove_file(&path).ok();
        let opts = OpenOptions {
            max_memory_bytes: crate::storage::page::PAGE_SIZE, // capacity = 1
        };
        let mut db = Database::create_with_options(&path, &opts).unwrap();
        let ast = crate::schema::parser::parse(SCHEMA).unwrap();
        crate::schema::store::save_schema(&mut db.file, &ast).unwrap();

        // admin: User + Admin label, owns p1. plain: User only, owns p2.
        let admin = db
            .insert_node_with_label_names(
                "User",
                vec!["Admin"],
                HashMap::from([
                    ("id".to_string(), json!("admin")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        let plain = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("plain")),
                    ("name".to_string(), json!("Bob")),
                ]),
            )
            .unwrap();
        let p1 = db
            .insert_node_by_name(
                "Project",
                HashMap::from([
                    ("id".to_string(), json!("p1")),
                    ("title".to_string(), json!("Alpha")),
                ]),
            )
            .unwrap();
        let p2 = db
            .insert_node_by_name(
                "Project",
                HashMap::from([
                    ("id".to_string(), json!("p2")),
                    ("title".to_string(), json!("Beta")),
                ]),
            )
            .unwrap();
        db.insert_edge_by_name(
            "OWNS",
            admin,
            p1,
            HashMap::from([("role".to_string(), json!("owner"))]),
        )
        .unwrap();
        db.insert_edge_by_name(
            "OWNS",
            plain,
            p2,
            HashMap::from([("role".to_string(), json!("owner"))]),
        )
        .unwrap();
        db.flush().unwrap();

        let view = DatabaseGraphView::load(&mut db).unwrap();

        // all_nodes_of_label scans live rids and derives labels on demand.
        let users = view.all_nodes_of_label("User");
        assert_eq!(users.len(), 2);
        let admins = view.all_nodes_of_label("Admin");
        assert_eq!(admins, vec![rid_to_id(admin.0)]);

        // AllNodes(User) -> HasLabel(Admin) -> OutEdges(OWNS) -> OutNodes(Project): only p1.
        let cmd = TraversalCommand {
            version: 1,
            bindings: Default::default(),
            start: StartSpec {
                kind: "AllNodes".into(),
                label: "User".into(),
                key: String::new(),
                value: json!(null),
            },
            steps: vec![
                TraversalStep {
                    action: TraversalAction::HasLabel,
                    labels: Some(vec!["Admin".into()]),
                    ..Default::default()
                },
                TraversalStep {
                    action: TraversalAction::OutEdges,
                    label: Some("OWNS".into()),
                    ..Default::default()
                },
                TraversalStep {
                    action: TraversalAction::OutNodes,
                    label: Some("Project".into()),
                    ..Default::default()
                },
            ],
            collect: CollectSpec::Nodes {
                properties: vec!["id".into()],
                with_edges: None,
            },
        };
        let result = match execute(&view, &cmd).unwrap() {
            CollectResult::Rows(r) => r,
            other => panic!("expected Rows, got {other:?}"),
        };
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], json!("p1"));
    }

    #[test]
    fn graph_view_node_labels_inline_decode() {
        // read_node_all_labels now decodes additional labels inline from the record's
        // label_ref (no second read_node). Verify both branches: a node with extra labels
        // (label_ref = Some) and one without (label_ref = None) report the right label set,
        // and that all_nodes_of_label filters correctly across both.
        let (mut db, _) = make_db_with_schema();
        let admin = db
            .insert_node_with_label_names(
                "User",
                vec!["Admin"],
                HashMap::from([
                    ("id".to_string(), json!("admin")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        let plain = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("plain")),
                    ("name".to_string(), json!("Bob")),
                ]),
            )
            .unwrap();
        db.flush().unwrap();

        let view = DatabaseGraphView::load(&mut db).unwrap();

        // With additional label (label_ref = Some): both labels, primary first.
        let admin_labels = view.node_labels(rid_to_id(admin.0));
        assert_eq!(admin_labels.first().map(String::as_str), Some("User"));
        assert!(admin_labels.iter().any(|l| l == "Admin"));
        assert_eq!(admin_labels.len(), 2);

        // Without additional label (label_ref = None): primary only.
        let plain_labels = view.node_labels(rid_to_id(plain.0));
        assert_eq!(plain_labels, vec!["User".to_string()]);

        // all_nodes_of_label sees the additional label only on the admin node.
        let mut users = view.all_nodes_of_label("User");
        users.sort_unstable();
        let mut expected = vec![rid_to_id(admin.0), rid_to_id(plain.0)];
        expected.sort_unstable();
        assert_eq!(users, expected);
        assert_eq!(view.all_nodes_of_label("Admin"), vec![rid_to_id(admin.0)]);
    }

    #[test]
    fn rollback_across_new_topology_page() {
        // NODE_RECORD_SIZE = 64 → ~63 node slots per page. Inserting past that allocates
        // a 2nd logical node page and bumps node_page_count. Rolling back such a
        // transaction must revert the count (and the persisted header) and stay reopenable.
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.into_temp_path().to_path_buf();
        std::fs::remove_file(&path).ok();
        let mut db = Database::create(&path).unwrap();

        // Commit a small baseline first.
        for _ in 0..3 {
            db.insert_node(1, HashMap::new()).unwrap();
        }
        db.flush().unwrap();
        let baseline = db.count_nodes(None).unwrap();
        assert_eq!(baseline, 3);

        // Transaction: insert enough to spill onto a 2nd node page, then roll back.
        let snap = db.take_snapshot();
        for _ in 0..80 {
            db.insert_node(1, HashMap::new()).unwrap();
        }
        assert!(
            db.count_nodes(None).unwrap() > 63,
            "should span 2 node pages"
        );
        db.restore_snapshot(snap);

        // Count reverts, and further allocation still works (counts are consistent).
        assert_eq!(db.count_nodes(None).unwrap(), baseline);
        let r = db.insert_node(1, HashMap::new()).unwrap();
        assert!(db.node_exists(r));
        db.flush().unwrap();

        // Reopen: persisted header count must reflect the post-rollback reality.
        drop(db);
        let mut db2 = Database::open(&path).unwrap();
        assert_eq!(db2.count_nodes(None).unwrap(), baseline + 1);
    }

    #[test]
    fn rollback_across_new_edge_page() {
        // Edge-side counterpart of rollback_across_new_topology_page. alloc/header re-sync
        // are separate functions for nodes vs edges, so cover the edge path too.
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.into_temp_path().to_path_buf();
        std::fs::remove_file(&path).ok();
        let mut db = Database::create(&path).unwrap();

        // A hub plus a few spokes; baseline edges committed.
        let hub = db.insert_node(1, HashMap::new()).unwrap();
        let mut spokes = Vec::new();
        for _ in 0..3 {
            let n = db.insert_node(1, HashMap::new()).unwrap();
            db.insert_edge(1, hub, n, HashMap::new()).unwrap();
            spokes.push(n);
        }
        db.flush().unwrap();
        let baseline_edges = db.count_edges(None).unwrap();
        assert_eq!(baseline_edges, 3);

        // Transaction: add enough edges to spill onto a 2nd edge page, then roll back.
        let snap = db.take_snapshot();
        for _ in 0..80 {
            db.insert_edge(1, hub, spokes[0], HashMap::new()).unwrap();
        }
        assert!(
            db.count_edges(None).unwrap() > 63,
            "should span 2 edge pages"
        );
        db.restore_snapshot(snap);

        assert_eq!(db.count_edges(None).unwrap(), baseline_edges);
        // Further edge allocation still works after rollback.
        db.insert_edge(1, hub, spokes[1], HashMap::new()).unwrap();
        db.flush().unwrap();

        drop(db);
        let mut db2 = Database::open(&path).unwrap();
        assert_eq!(db2.count_edges(None).unwrap(), baseline_edges + 1);
    }

    #[test]
    fn insert_node_and_read_properties() {
        let (mut db, _) = make_db();
        let props = HashMap::from([
            ("id".to_string(), json!("u1")),
            ("name".to_string(), json!("Alice")),
            ("age".to_string(), json!(30)),
        ]);
        let rid = db.insert_node(1, props.clone()).unwrap();
        let loaded = db.get_node_properties(rid).unwrap();
        assert_eq!(props, loaded);
    }

    #[test]
    fn insert_edge_and_traverse() {
        let (mut db, _) = make_db();
        let n1 = db
            .insert_node(1, HashMap::from([("id".to_string(), json!("u1"))]))
            .unwrap();
        let n2 = db
            .insert_node(2, HashMap::from([("id".to_string(), json!("p1"))]))
            .unwrap();
        let edge_props = HashMap::from([("role".to_string(), json!("owner"))]);
        let eid = db.insert_edge(1, n1, n2, edge_props.clone()).unwrap();

        let out: Vec<_> = db.topo.collect_out_edges(&mut db.file, n1.0).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, eid.0);

        let loaded = db.get_edge_properties(eid).unwrap();
        assert_eq!(edge_props, loaded);
    }

    #[test]
    fn delete_node_cascades_edges() {
        let (mut db, _) = make_db();
        let n1 = db.insert_node(1, HashMap::new()).unwrap();
        let n2 = db.insert_node(2, HashMap::new()).unwrap();
        let _e1 = db.insert_edge(1, n1, n2, HashMap::new()).unwrap();
        let _e2 = db.insert_edge(1, n1, n2, HashMap::new()).unwrap();

        db.delete_node(n1).unwrap();

        let out: Vec<_> = db.topo.collect_out_edges(&mut db.file, n1.0).unwrap();
        assert!(out.is_empty());
        let in_: Vec<_> = db.topo.collect_in_edges(&mut db.file, n2.0).unwrap();
        assert!(in_.is_empty());
    }

    #[test]
    fn persist_across_reopen() {
        let (mut db, path) = make_db();
        let props = HashMap::from([("id".to_string(), json!("u1"))]);
        let rid = db.insert_node(1, props.clone()).unwrap();
        db.flush().unwrap();
        drop(db);

        let mut db2 = Database::open(&path).unwrap();
        let loaded = db2.get_node_properties(rid).unwrap();
        assert_eq!(props, loaded);
    }

    // ---- Multi-label tests ----

    const SCHEMA: &str = r#"
        node User { id: String  name: String }
        node Admin { role: String }
        node Project { id: String  title: String }
        edge OWNS { from: User  to: Project  props: { role: String } }
    "#;

    fn make_db_with_schema() -> (Database, std::path::PathBuf) {
        let (mut db, path) = make_db();
        let ast = crate::schema::parser::parse(SCHEMA).unwrap();
        crate::schema::store::save_schema(&mut db.file, &ast).unwrap();
        (db, path)
    }

    #[test]
    fn multi_label_insert_and_get() {
        let (mut db, _) = make_db_with_schema();
        // Insert a node with both User and Admin labels
        let rid = db
            .insert_node_with_label_names(
                "User",
                vec!["Admin"],
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();

        let labels = db.get_node_label_names(rid).unwrap();
        assert!(labels.contains(&"User".to_string()));
        assert!(labels.contains(&"Admin".to_string()));
        assert_eq!(labels.len(), 2);
    }

    #[test]
    fn dynamic_label_insert_registers_unknown_and_returns_assignments() {
        let (mut db, _) = make_db_with_schema();
        // "Person" and "VIP" are not in the schema; they must be registered on the fly.
        let (rid, assignments) = db
            .insert_node_with_dynamic_labels(
                "User",
                vec!["Person", "VIP"],
                HashMap::from([("id".to_string(), json!("u1"))]),
            )
            .unwrap();

        // "User" already exists in the schema.
        assert!(!assignments["User"].created);
        // "Person"/"VIP" are newly minted.
        assert!(assignments["Person"].created);
        assert!(assignments["VIP"].created);
        assert_ne!(assignments["Person"].id, assignments["VIP"].id);

        // The dynamically registered labels resolve back to their names.
        let labels = db.get_node_label_names(rid).unwrap();
        assert!(labels.contains(&"Person".to_string()));
        assert!(labels.contains(&"VIP".to_string()));

        // Re-inserting with the same label reuses the id and reports created=false.
        let (_, assignments2) = db
            .insert_node_with_dynamic_labels(
                "User",
                vec!["Person"],
                HashMap::from([("id".to_string(), json!("u2"))]),
            )
            .unwrap();
        assert!(!assignments2["Person"].created);
        assert_eq!(assignments2["Person"].id, assignments["Person"].id);
    }

    #[test]
    fn dynamic_label_filtering_via_has_label() {
        let (mut db, _) = make_db_with_schema();
        db.insert_node_with_dynamic_labels(
            "User",
            vec!["Person"],
            HashMap::from([("id".to_string(), json!("u1"))]),
        )
        .unwrap();
        db.insert_node_with_dynamic_labels(
            "User",
            vec![],
            HashMap::from([("id".to_string(), json!("u2"))]),
        )
        .unwrap();
        db.flush().unwrap();

        // HasLabel filtering reaches dynamically registered labels with no property scan.
        let view = DatabaseGraphView::load(&mut db).unwrap();
        let person_nodes = view.all_nodes_of_label("Person");
        assert_eq!(person_nodes.len(), 1);
    }

    #[test]
    fn dynamic_label_id_persists_across_reopen() {
        let (mut db, path) = make_db_with_schema();
        let (_, assignments) = db
            .insert_node_with_dynamic_labels(
                "User",
                vec!["Person"],
                HashMap::from([("id".to_string(), json!("u1"))]),
            )
            .unwrap();
        let person_id = assignments["Person"].id;
        db.flush().unwrap();
        drop(db);

        let mut db2 = Database::open(&path).unwrap();
        // The id is stable: registering the same name again returns the persisted id.
        let (_, assignments2) = db2
            .insert_node_with_dynamic_labels(
                "User",
                vec!["Person"],
                HashMap::from([("id".to_string(), json!("u2"))]),
            )
            .unwrap();
        assert!(!assignments2["Person"].created);
        assert_eq!(assignments2["Person"].id, person_id);
    }

    #[test]
    fn dynamic_label_survives_later_apply_schema() {
        // A schema migration must not drop dynamically registered labels.
        let (mut db, _) = make_db_with_schema();
        let (_, assignments) = db
            .insert_node_with_dynamic_labels(
                "User",
                vec!["Person"],
                HashMap::from([("id".to_string(), json!("u1"))]),
            )
            .unwrap();
        let person_id = assignments["Person"].id;

        // Apply a compatible schema migration (add a field).
        db.apply_schema(
            "node User { id: String  name: String  email: String } \
             node Admin { role: String } \
             node Project { id: String  title: String } \
             edge OWNS { from: User  to: Project  props: { role: String } }",
        )
        .unwrap();

        // The dynamic label and its id are preserved.
        let (_, assignments2) = db
            .insert_node_with_dynamic_labels(
                "User",
                vec!["Person"],
                HashMap::from([("id".to_string(), json!("u2"))]),
            )
            .unwrap();
        assert!(!assignments2["Person"].created);
        assert_eq!(assignments2["Person"].id, person_id);
    }

    #[test]
    fn additional_labels_normalized_to_a_set() {
        // Multi-label semantics are set-like: passing the primary again or repeating an
        // additional label must not pollute the persisted additional-label list.
        let (mut db, _) = make_db_with_schema();
        let (rid, assignments) = db
            .insert_node_with_dynamic_labels(
                "User",
                vec!["User", "X", "X"],
                HashMap::from([("id".to_string(), json!("u1"))]),
            )
            .unwrap();
        let x_id = assignments["X"].id;

        // Names: primary "User" appears once, "X" once, no duplicates.
        let mut names = db.get_node_label_names(rid).unwrap();
        names.sort();
        assert_eq!(names, vec!["User".to_string(), "X".to_string()]);

        // Additional ids: deduplicated and the primary id excluded.
        assert_eq!(db.get_additional_labels(rid).unwrap(), vec![x_id]);
    }

    #[test]
    fn add_and_remove_node_label() {
        let (mut db, _) = make_db_with_schema();
        let rid = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();

        // Initially only the primary label
        let labels = db.get_node_label_names(rid).unwrap();
        assert_eq!(labels, vec!["User".to_string()]);

        // Add the Admin label
        db.add_node_label_by_name(rid, "Admin").unwrap();
        let labels = db.get_node_label_names(rid).unwrap();
        assert!(labels.contains(&"User".to_string()));
        assert!(labels.contains(&"Admin".to_string()));

        // Remove the Admin label
        db.remove_node_label_by_name(rid, "Admin").unwrap();
        let labels = db.get_node_label_names(rid).unwrap();
        assert_eq!(labels, vec!["User".to_string()]);
    }

    #[test]
    fn multi_label_persists_across_reopen() {
        let (mut db, path) = make_db_with_schema();
        let rid = db
            .insert_node_with_label_names(
                "User",
                vec!["Admin"],
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        db.flush().unwrap();
        drop(db);

        let mut db2 = Database::open(&path).unwrap();
        let labels = db2.get_node_label_names(rid).unwrap();
        assert!(labels.contains(&"User".to_string()));
        assert!(labels.contains(&"Admin".to_string()));
    }

    // ---- In-memory DB tests ----

    // ---- VACUUM tests ----

    #[test]
    fn vacuum_compacts_deleted_nodes() {
        let mut db = Database::open_in_memory().unwrap();
        let n1 = db
            .insert_node(1, HashMap::from([("id".to_string(), json!("u1"))]))
            .unwrap();
        let n2 = db
            .insert_node(1, HashMap::from([("id".to_string(), json!("u2"))]))
            .unwrap();
        let n3 = db
            .insert_node(1, HashMap::from([("id".to_string(), json!("u3"))]))
            .unwrap();
        let _ = (n1, n3);

        db.delete_node(n2).unwrap();
        db.vacuum().unwrap();

        // After vacuum, n2 no longer exists (IDs change, so verify by count)
        let nodes = db.list_nodes(None).unwrap();
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn vacuum_file_roundtrip() {
        let (mut db, path) = make_db();
        let n1 = db
            .insert_node(1, HashMap::from([("id".to_string(), json!("u1"))]))
            .unwrap();
        let n2 = db
            .insert_node(1, HashMap::from([("id".to_string(), json!("u2"))]))
            .unwrap();
        let _ = n1;
        db.delete_node(n2).unwrap();
        db.flush().unwrap();
        drop(db);

        Database::vacuum_file(&path).unwrap();

        let mut db2 = Database::open(&path).unwrap();
        let nodes = db2.list_nodes(None).unwrap();
        assert_eq!(nodes.len(), 1);
    }

    // ---- Integrity check tests ----

    #[test]
    fn verify_clean_db_returns_empty() {
        let (mut db, _) = make_db_with_schema();
        let u = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        let p = db
            .insert_node_by_name(
                "Project",
                HashMap::from([
                    ("id".to_string(), json!("p1")),
                    ("title".to_string(), json!("Alpha")),
                ]),
            )
            .unwrap();
        db.insert_edge_by_name(
            "OWNS",
            u,
            p,
            HashMap::from([("role".to_string(), json!("owner"))]),
        )
        .unwrap();
        let warnings = db.verify().unwrap();
        assert!(warnings.is_empty(), "unexpected warnings: {:?}", warnings);
    }

    // ---- Lazy-load tests ----

    #[test]
    fn lazy_load_properties_on_demand() {
        let (mut db, _) = make_db();
        let n1 = db
            .insert_node(
                1,
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        let n2 = db
            .insert_node(
                2,
                HashMap::from([
                    ("id".to_string(), json!("p1")),
                    ("title".to_string(), json!("Alpha")),
                ]),
            )
            .unwrap();
        db.insert_edge(1, n1, n2, HashMap::new()).unwrap();

        let view = DatabaseGraphView::load(&mut db).unwrap();

        // Cache is empty right after load
        assert!(view.node_props.borrow().is_empty());
        assert!(view.edge_props.borrow().is_empty());

        // Only the node whose properties were accessed gets cached
        let _ = view.node_properties(rid_to_id(n1.0));
        assert_eq!(view.node_props.borrow().len(), 1);

        // The other node has not been accessed yet, so it is not cached
        assert!(!view.node_props.borrow().contains_key(&rid_to_id(n2.0)));

        // Second call returns from cache (no file I/O)
        let props = view.node_properties(rid_to_id(n1.0)).unwrap();
        assert_eq!(props["id"], json!("u1"));
    }

    #[test]
    fn in_memory_crud() {
        let mut db = Database::open_in_memory().unwrap();
        let props = HashMap::from([
            ("id".to_string(), json!("u1")),
            ("name".to_string(), json!("Alice")),
        ]);
        let rid = db.insert_node(1, props.clone()).unwrap();
        let loaded = db.get_node_properties(rid).unwrap();
        assert_eq!(props, loaded);
    }

    #[test]
    fn in_memory_traversal() {
        let mut db = Database::open_in_memory().unwrap();
        let n1 = db
            .insert_node(1, HashMap::from([("id".to_string(), json!("u1"))]))
            .unwrap();
        let n2 = db
            .insert_node(2, HashMap::from([("id".to_string(), json!("p1"))]))
            .unwrap();
        db.insert_edge(1, n1, n2, HashMap::new()).unwrap();

        let out: Vec<_> = db.topo.collect_out_edges(&mut db.file, n1.0).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to_node, n2.0);
    }

    #[test]
    fn in_memory_does_not_persist() {
        // In-memory DB does not create any file
        let db = Database::open_in_memory().unwrap();
        assert!(db.file.is_in_memory());
        assert!(db.file.wal_path().is_none());
    }

    // ---- Index tests (Step 27) ----

    #[test]
    fn index_find_after_insert() {
        let (mut db, _) = make_db_with_schema();
        let rid = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        let registry = db.load_schema_registry().unwrap();
        let type_id = registry.node_type_id("User").unwrap();
        assert_eq!(
            crate::storage::index::find(&db.topo, &mut db.file, type_id, "id", &json!("u1"))
                .unwrap(),
            Some(rid.0)
        );
    }

    #[test]
    fn index_cleared_after_delete() {
        let (mut db, _) = make_db_with_schema();
        let rid = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        let registry = db.load_schema_registry().unwrap();
        let type_id = registry.node_type_id("User").unwrap();
        db.delete_node(rid).unwrap();
        assert_eq!(
            crate::storage::index::find(&db.topo, &mut db.file, type_id, "id", &json!("u1"))
                .unwrap(),
            None
        );
    }

    #[test]
    fn index_updated_after_update_properties() {
        let (mut db, _) = make_db_with_schema();
        let rid = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        db.update_node_properties(
            rid,
            HashMap::from([
                ("id".to_string(), json!("u1")),
                ("name".to_string(), json!("Alicia")),
            ]),
        )
        .unwrap();
        let registry = db.load_schema_registry().unwrap();
        let type_id = registry.node_type_id("User").unwrap();
        assert_eq!(
            crate::storage::index::find(&db.topo, &mut db.file, type_id, "name", &json!("Alice"))
                .unwrap(),
            None
        );
        assert_eq!(
            crate::storage::index::find(&db.topo, &mut db.file, type_id, "name", &json!("Alicia"))
                .unwrap(),
            Some(rid.0)
        );
    }

    #[test]
    fn index_rebuilt_after_reopen() {
        let (mut db, path) = make_db_with_schema();
        let rid = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        db.flush().unwrap();
        drop(db);

        let mut db2 = Database::open(&path).unwrap();
        let registry = db2.load_schema_registry().unwrap();
        let type_id = registry.node_type_id("User").unwrap();
        assert_eq!(
            crate::storage::index::find(&db2.topo, &mut db2.file, type_id, "id", &json!("u1"))
                .unwrap(),
            Some(rid.0)
        );
    }

    use proptest::prelude::*;

    // ---- Edge multi-label tests ----

    const SCHEMA2: &str = r#"
        node User { id: String }
        node Project { id: String }
        edge OWNS { from: User  to: Project  props: { role: String } }
        edge MANAGES { from: User  to: Project  props: { since: String } }
    "#;

    fn make_db_with_schema2() -> (Database, std::path::PathBuf) {
        let (mut db, path) = make_db();
        let ast = crate::schema::parser::parse(SCHEMA2).unwrap();
        crate::schema::store::save_schema(&mut db.file, &ast).unwrap();
        (db, path)
    }

    #[test]
    fn edge_multi_label_insert_and_get() {
        let (mut db, _) = make_db_with_schema2();
        let u = db
            .insert_node_by_name("User", HashMap::from([("id".to_string(), json!("u1"))]))
            .unwrap();
        let p = db
            .insert_node_by_name("Project", HashMap::from([("id".to_string(), json!("p1"))]))
            .unwrap();
        let eid = db
            .insert_edge_by_name(
                "OWNS",
                u,
                p,
                HashMap::from([("role".to_string(), json!("owner"))]),
            )
            .unwrap();

        db.add_edge_label_by_name(eid, "MANAGES").unwrap();
        let labels = db.get_edge_label_names(eid).unwrap();
        assert!(labels.contains(&"OWNS".to_string()));
        assert!(labels.contains(&"MANAGES".to_string()));
    }

    #[test]
    fn edge_additional_labels_normalized_to_a_set() {
        // Edge multi-labels are set-like, mirroring the node-side invariant: the primary
        // edge type must not appear in the additional list, and ids must not repeat.
        let (mut db, _) = make_db_with_schema2();
        let u = db
            .insert_node_by_name("User", HashMap::from([("id".to_string(), json!("u1"))]))
            .unwrap();
        let p = db
            .insert_node_by_name("Project", HashMap::from([("id".to_string(), json!("p1"))]))
            .unwrap();

        let registry = db.load_schema_registry().unwrap();
        let owns_id = registry.edge_type_id("OWNS").unwrap();
        let manages_id = registry.edge_type_id("MANAGES").unwrap();

        // Pass the primary type ("OWNS") and a duplicated additional ("MANAGES") in the set.
        let eid = db
            .insert_edge_with_labels(
                owns_id,
                u,
                p,
                vec![owns_id, manages_id, manages_id],
                HashMap::from([("role".to_string(), json!("owner"))]),
            )
            .unwrap();

        // Additional ids: deduplicated and the primary id excluded.
        assert_eq!(
            db.get_edge_additional_labels(eid).unwrap(),
            vec![manages_id]
        );

        // Names: primary "OWNS" once, "MANAGES" once, no duplicates.
        let mut names = db.get_edge_label_names(eid).unwrap();
        names.sort();
        assert_eq!(names, vec!["MANAGES".to_string(), "OWNS".to_string()]);
    }

    #[test]
    fn edge_label_remove() {
        let (mut db, _) = make_db_with_schema2();
        let u = db
            .insert_node_by_name("User", HashMap::from([("id".to_string(), json!("u1"))]))
            .unwrap();
        let p = db
            .insert_node_by_name("Project", HashMap::from([("id".to_string(), json!("p1"))]))
            .unwrap();
        let eid = db
            .insert_edge_by_name("OWNS", u, p, HashMap::new())
            .unwrap();

        db.add_edge_label_by_name(eid, "MANAGES").unwrap();
        db.remove_edge_label_by_name(eid, "MANAGES").unwrap();
        let labels = db.get_edge_label_names(eid).unwrap();
        assert_eq!(labels, vec!["OWNS".to_string()]);
    }

    #[test]
    fn edge_multi_label_persists_across_reopen() {
        let (mut db, path) = make_db_with_schema2();
        let u = db
            .insert_node_by_name("User", HashMap::from([("id".to_string(), json!("u1"))]))
            .unwrap();
        let p = db
            .insert_node_by_name("Project", HashMap::from([("id".to_string(), json!("p1"))]))
            .unwrap();
        let eid = db
            .insert_edge_by_name("OWNS", u, p, HashMap::new())
            .unwrap();
        db.add_edge_label_by_name(eid, "MANAGES").unwrap();
        db.flush().unwrap();
        drop(db);

        let mut db2 = Database::open(&path).unwrap();
        let labels = db2.get_edge_label_names(eid).unwrap();
        assert!(labels.contains(&"OWNS".to_string()));
        assert!(labels.contains(&"MANAGES".to_string()));
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(50))]
        #[test]
        fn index_consistent_with_linear_scan(
            names in proptest::collection::vec("[a-z]{3,8}", 1..10usize),
        ) {
            let (mut db, _) = make_db_with_schema();
            let mut rids = Vec::new();
            for name in &names {
                let rid = db.insert_node_by_name("User", HashMap::from([
                    ("id".to_string(), json!(name.clone())),
                    ("name".to_string(), json!(name.clone())),
                ])).unwrap();
                rids.push(rid);
            }

            let registry = db.load_schema_registry().unwrap();
            let type_id = registry.node_type_id("User").unwrap();

            for name in &names {
                // `find` returns the first live match (same as the former index's `.first()`).
                // The expected RID is therefore the earliest-inserted node with this name.
                let expected = names
                    .iter()
                    .position(|n| n == name)
                    .map(|pos| rids[pos].0);
                let by_scan = crate::storage::index::find(
                    &db.topo,
                    &mut db.file,
                    type_id,
                    "id",
                    &json!(name.clone()),
                )
                .unwrap();
                prop_assert_eq!(by_scan, expected);
            }
        }
    }

    #[test]
    fn graph_view_find_and_traverse() {
        let (mut db, _) = make_db();
        let n1 = db
            .insert_node(
                1,
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        let n2 = db
            .insert_node(
                2,
                HashMap::from([
                    ("id".to_string(), json!("p1")),
                    ("title".to_string(), json!("Alpha")),
                    ("isArchived".to_string(), json!(false)),
                ]),
            )
            .unwrap();
        db.insert_edge(1, n1, n2, HashMap::new()).unwrap();

        let view = DatabaseGraphView::load(&mut db).unwrap();
        let found = view.find_node_by("1", "id", &json!("u1"));
        assert!(found.is_some());

        let out_edges = view.out_edges(found.unwrap(), None);
        assert_eq!(out_edges.len(), 1);
        let to_node = view.edge_to_node(out_edges[0]).unwrap();
        let props = view.node_properties(to_node).unwrap();
        assert_eq!(props["id"], json!("p1"));
    }

    #[test]
    fn node_exists_and_edge_exists() {
        let (mut db, _) = make_db();
        let n1 = db.insert_node(1, HashMap::new()).unwrap();
        let n2 = db.insert_node(2, HashMap::new()).unwrap();
        let eid = db.insert_edge(1, n1, n2, HashMap::new()).unwrap();

        assert!(db.node_exists(n1));
        assert!(db.edge_exists(eid));
        assert!(!db.node_exists(NodeRid::new(99, 99)));
        assert!(!db.edge_exists(EdgeRid::new(99, 99)));

        db.delete_node(n1).unwrap();
        assert!(!db.node_exists(n1));
    }

    #[test]
    fn count_nodes_and_edges() {
        let (mut db, _) = make_db();
        let schema = r#"node A {} node B {} edge E { from: A to: B }"#;
        db.apply_schema(schema).unwrap();

        assert_eq!(db.count_nodes(None).unwrap(), 0);
        assert_eq!(db.count_edges(None).unwrap(), 0);

        let n1 = db.insert_node_by_name("A", HashMap::new()).unwrap();
        let n2 = db.insert_node_by_name("B", HashMap::new()).unwrap();
        db.insert_edge_by_name("E", n1, n2, HashMap::new()).unwrap();

        assert_eq!(db.count_nodes(None).unwrap(), 2);
        assert_eq!(db.count_nodes(Some("A")).unwrap(), 1);
        assert_eq!(db.count_nodes(Some("Missing")).unwrap(), 0);
        assert_eq!(db.count_edges(None).unwrap(), 1);
        assert_eq!(db.count_edges(Some("E")).unwrap(), 1);
        assert_eq!(db.count_edges(Some("Missing")).unwrap(), 0);
    }

    #[test]
    fn patch_node_and_edge_properties() {
        let (mut db, _) = make_db();
        let n1 = db
            .insert_node(1, HashMap::from([("a".to_string(), json!(1))]))
            .unwrap();
        let n2 = db.insert_node(1, HashMap::new()).unwrap();
        let eid = db
            .insert_edge(1, n1, n2, HashMap::from([("x".to_string(), json!(10))]))
            .unwrap();

        db.patch_node_properties(n1, HashMap::from([("b".to_string(), json!(2))]))
            .unwrap();
        let props = db.get_node_properties(n1).unwrap();
        assert_eq!(props["a"], json!(1));
        assert_eq!(props["b"], json!(2));

        db.patch_edge_properties(eid, HashMap::from([("y".to_string(), json!(20))]))
            .unwrap();
        let eprops = db.get_edge_properties(eid).unwrap();
        assert_eq!(eprops["x"], json!(10));
        assert_eq!(eprops["y"], json!(20));
    }

    #[test]
    fn schema_query_methods() {
        let (mut db, _) = make_db();
        let schema = r#"node Person { id: String } edge KNOWS { from: Person to: Person }"#;
        db.apply_schema(schema).unwrap();

        let node_names = db.node_type_names().unwrap();
        assert_eq!(node_names, vec!["Person"]);

        let edge_names = db.edge_type_names().unwrap();
        assert_eq!(edge_names, vec!["KNOWS"]);

        let ns = db.node_type_schema("Person").unwrap().unwrap();
        assert_eq!(ns.name, "Person");
        assert_eq!(ns.properties.len(), 1);
        assert_eq!(ns.properties[0].name, "id");
        assert_eq!(ns.properties[0].value_type, "String");

        assert!(db.node_type_schema("Unknown").unwrap().is_none());

        let es = db.edge_type_schema("KNOWS").unwrap().unwrap();
        assert_eq!(es.name, "KNOWS");
    }

    #[test]
    fn insert_node_returns_error_when_node_capacity_exceeded() {
        let (mut db, _) = make_db();
        // The topology segment holds a fixed number of pages. Exceeding it must surface
        // as a CapacityExceeded error from insert_node (enforced at allocation time).
        let node_capacity = (db.file.header.property_segment_start
            - db.file.header.topology_segment_start)
            .div_ceil(2) as usize;
        // Keep allocating nodes until we hit capacity.
        let mut result = Ok(NodeRid::new(0, 0));
        while db.topo.node_page_count() <= node_capacity {
            result = db.insert_node(1, HashMap::new());
            if result.is_err() {
                break;
            }
        }
        assert!(
            matches!(
                result,
                Err(GraphError::CapacityExceeded { kind: "node", .. })
            ),
            "expected CapacityExceeded, got {result:?}"
        );
    }

    #[test]
    fn type_ids_are_stable_when_a_type_is_inserted_in_the_middle() {
        let (mut db, _) = make_db();
        db.apply_schema("node User { id: String } node Project { id: String }")
            .unwrap();

        let registry = db.load_schema_registry().unwrap();
        let project_id_before = registry.node_type_id("Project").unwrap();

        // Insert a Project node, then add a new type *before* Project in the schema.
        db.insert_node_by_name("Project", HashMap::from([("id".to_string(), json!("p1"))]))
            .unwrap();

        db.apply_schema(
            "node User { id: String } node Admin { id: String } node Project { id: String }",
        )
        .unwrap();

        let registry = db.load_schema_registry().unwrap();
        // Project's id must not have shifted, and Admin must get a fresh id.
        assert_eq!(registry.node_type_id("Project"), Some(project_id_before));
        assert_ne!(registry.node_type_id("Admin"), Some(project_id_before));

        // The existing Project node must still be readable as a Project, not as Admin.
        let projects = db.list_nodes(Some("Project")).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].1.get("id"), Some(&json!("p1")));

        let admins = db.list_nodes(Some("Admin")).unwrap();
        assert!(admins.is_empty(), "no Admin nodes were ever inserted");
    }

    #[test]
    fn removed_type_id_is_not_reused() {
        let (mut db, _) = make_db();
        db.apply_schema("node A { id: String } node B { id: String }")
            .unwrap();
        let a_id = db
            .load_schema_registry()
            .unwrap()
            .node_type_id("A")
            .unwrap();
        let b_id = db
            .load_schema_registry()
            .unwrap()
            .node_type_id("B")
            .unwrap();

        // Remove A, then add a new type C. C must not inherit A's id.
        db.apply_schema("node B { id: String } node C { id: String }")
            .unwrap();
        let registry = db.load_schema_registry().unwrap();
        assert_eq!(registry.node_type_id("B"), Some(b_id));
        let c_id = registry.node_type_id("C").unwrap();
        assert_ne!(c_id, a_id, "a removed type's id must not be reused");
        assert_ne!(c_id, b_id);
    }

    // ---- Schema validation on insert ----

    #[test]
    fn insert_node_rejects_unknown_field() {
        let (mut db, _) = make_db_with_schema();
        let err = db
            .insert_node_by_name("User", HashMap::from([("ghost".to_string(), json!("x"))]))
            .unwrap_err();
        assert!(
            matches!(err, GraphError::ValidationError(_)),
            "expected ValidationError, got {err:?}"
        );
    }

    #[test]
    fn insert_node_rejects_type_mismatch() {
        let (mut db, _) = make_db_with_schema();
        let err = db
            .insert_node_by_name("User", HashMap::from([("id".to_string(), json!(42))]))
            .unwrap_err();
        assert!(matches!(err, GraphError::ValidationError(_)));
    }

    #[test]
    fn insert_node_accepts_valid_props() {
        let (mut db, _) = make_db_with_schema();
        db.insert_node_by_name(
            "User",
            HashMap::from([
                ("id".to_string(), json!("u1")),
                ("name".to_string(), json!("Alice")),
            ]),
        )
        .unwrap();
    }

    #[test]
    fn insert_edge_rejects_wrong_from_type() {
        let (mut db, _) = make_db_with_schema();
        // Project → Project: violates OWNS { from: User, to: Project }
        let p1 = db
            .insert_node_by_name(
                "Project",
                HashMap::from([
                    ("id".to_string(), json!("p1")),
                    ("title".to_string(), json!("Alpha")),
                ]),
            )
            .unwrap();
        let p2 = db
            .insert_node_by_name(
                "Project",
                HashMap::from([
                    ("id".to_string(), json!("p2")),
                    ("title".to_string(), json!("Beta")),
                ]),
            )
            .unwrap();
        let err = db
            .insert_edge_by_name("OWNS", p1, p2, HashMap::new())
            .unwrap_err();
        assert!(
            matches!(err, GraphError::ValidationError(_)),
            "expected ValidationError, got {err:?}"
        );
    }

    #[test]
    fn insert_edge_rejects_wrong_to_type() {
        let (mut db, _) = make_db_with_schema();
        // User → User: violates OWNS { from: User, to: Project }
        let u1 = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        let u2 = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u2")),
                    ("name".to_string(), json!("Bob")),
                ]),
            )
            .unwrap();
        let err = db
            .insert_edge_by_name("OWNS", u1, u2, HashMap::new())
            .unwrap_err();
        assert!(matches!(err, GraphError::ValidationError(_)));
    }

    #[test]
    fn insert_edge_rejects_nonexistent_from_node() {
        let (mut db, _) = make_db_with_schema();
        let p = db
            .insert_node_by_name(
                "Project",
                HashMap::from([
                    ("id".to_string(), json!("p1")),
                    ("title".to_string(), json!("Alpha")),
                ]),
            )
            .unwrap();
        // Use a NodeRid that was never inserted
        let ghost = NodeRid::new(9999, 0);
        let err = db
            .insert_edge_by_name("OWNS", ghost, p, HashMap::new())
            .unwrap_err();
        assert!(
            matches!(
                err,
                GraphError::NodeNotFound(_) | GraphError::ValidationError(_)
            ),
            "expected NodeNotFound or ValidationError, got {err:?}"
        );
    }

    #[test]
    fn insert_edge_rejects_nonexistent_to_node() {
        let (mut db, _) = make_db_with_schema();
        let u = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        let ghost = NodeRid::new(9999, 0);
        let err = db
            .insert_edge_by_name("OWNS", u, ghost, HashMap::new())
            .unwrap_err();
        assert!(
            matches!(
                err,
                GraphError::NodeNotFound(_) | GraphError::ValidationError(_)
            ),
            "expected NodeNotFound or ValidationError, got {err:?}"
        );
    }

    #[test]
    fn insert_edge_rejects_unknown_prop() {
        let (mut db, _) = make_db_with_schema();
        let u = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        let p = db
            .insert_node_by_name(
                "Project",
                HashMap::from([
                    ("id".to_string(), json!("p1")),
                    ("title".to_string(), json!("Alpha")),
                ]),
            )
            .unwrap();
        let err = db
            .insert_edge_by_name(
                "OWNS",
                u,
                p,
                HashMap::from([("ghost".to_string(), json!("x"))]),
            )
            .unwrap_err();
        assert!(matches!(err, GraphError::ValidationError(_)));
    }

    #[test]
    fn insert_edge_rejects_deleted_from_node() {
        let (mut db, _) = make_db_with_schema();
        let u = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        let p = db
            .insert_node_by_name(
                "Project",
                HashMap::from([
                    ("id".to_string(), json!("p1")),
                    ("title".to_string(), json!("Alpha")),
                ]),
            )
            .unwrap();
        db.delete_node(u).unwrap();
        let err = db
            .insert_edge_by_name("OWNS", u, p, HashMap::new())
            .unwrap_err();
        assert!(
            matches!(err, GraphError::NodeNotFound(_)),
            "expected NodeNotFound for deleted from-node, got {err:?}"
        );
    }

    #[test]
    fn insert_edge_rejects_deleted_to_node() {
        let (mut db, _) = make_db_with_schema();
        let u = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        let p = db
            .insert_node_by_name(
                "Project",
                HashMap::from([
                    ("id".to_string(), json!("p1")),
                    ("title".to_string(), json!("Alpha")),
                ]),
            )
            .unwrap();
        db.delete_node(p).unwrap();
        let err = db
            .insert_edge_by_name("OWNS", u, p, HashMap::new())
            .unwrap_err();
        assert!(
            matches!(err, GraphError::NodeNotFound(_)),
            "expected NodeNotFound for deleted to-node, got {err:?}"
        );
    }

    #[test]
    fn update_node_properties_rejects_deleted_node() {
        let (mut db, _) = make_db_with_schema();
        let u = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        db.delete_node(u).unwrap();
        let err = db
            .update_node_properties(u, HashMap::from([("id".to_string(), json!("x"))]))
            .unwrap_err();
        assert!(
            matches!(err, GraphError::NodeNotFound(_)),
            "expected NodeNotFound for deleted node, got {err:?}"
        );
    }

    #[test]
    fn update_edge_properties_rejects_deleted_edge() {
        let (mut db, _) = make_db_with_schema();
        let u = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        let p = db
            .insert_node_by_name(
                "Project",
                HashMap::from([
                    ("id".to_string(), json!("p1")),
                    ("title".to_string(), json!("Alpha")),
                ]),
            )
            .unwrap();
        let eid = db
            .insert_edge_by_name("OWNS", u, p, HashMap::new())
            .unwrap();
        db.delete_edge(eid).unwrap();
        let err = db.update_edge_properties(eid, HashMap::new()).unwrap_err();
        assert!(
            matches!(err, GraphError::EdgeNotFound(_)),
            "expected EdgeNotFound for deleted edge, got {err:?}"
        );
    }

    #[test]
    fn delete_node_twice_returns_error() {
        let (mut db, _) = make_db_with_schema();
        let u = db
            .insert_node_by_name(
                "User",
                HashMap::from([
                    ("id".to_string(), json!("u1")),
                    ("name".to_string(), json!("Alice")),
                ]),
            )
            .unwrap();
        db.delete_node(u).unwrap();
        let err = db.delete_node(u).unwrap_err();
        assert!(
            matches!(err, GraphError::NodeNotFound(_)),
            "expected NodeNotFound on double-delete, got {err:?}"
        );
    }
}
