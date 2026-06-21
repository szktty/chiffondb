use std::collections::HashMap;

use serde_json::Value;

use crate::db::{Database, DatabaseGraphView, DbSnapshot};
use crate::pathfinding::{PathDirection, PathOptions};
use crate::traversal::executor::execute;

// ---- DTOs for Dart ----

pub struct RecordId {
    pub page: u32,
    pub slot: u16,
}

pub struct NodeResult {
    pub properties: String, // JSON
}

// ---- Database handle ----

pub struct Connection {
    inner: Option<Database>,
    /// Snapshot held for the current pending transaction. None when idle.
    pending_snapshot: Option<DbSnapshot>,
}

impl Connection {
    fn db_mut(&mut self) -> Result<&mut Database, String> {
        self.inner
            .as_mut()
            .ok_or_else(|| "connection is already closed".to_string())
    }

    /// Opens an existing database.
    pub fn open(path: String) -> Result<Connection, String> {
        Database::open(std::path::Path::new(&path))
            .map(|db| Connection {
                inner: Some(db),
                pending_snapshot: None,
            })
            .map_err(|e| e.to_string())
    }

    /// Opens an existing database with a custom in-memory page-cache budget (bytes).
    pub fn open_with_max_memory(
        path: String,
        max_memory_bytes: usize,
    ) -> Result<Connection, String> {
        let opts = crate::storage::file::OpenOptions { max_memory_bytes };
        Database::open_with_options(std::path::Path::new(&path), &opts)
            .map(|db| Connection {
                inner: Some(db),
                pending_snapshot: None,
            })
            .map_err(|e| e.to_string())
    }

    /// Creates a new database and opens it.
    pub fn create(path: String) -> Result<Connection, String> {
        Database::create(std::path::Path::new(&path))
            .map(|db| Connection {
                inner: Some(db),
                pending_snapshot: None,
            })
            .map_err(|e| e.to_string())
    }

    /// Creates a new database with a custom in-memory page-cache budget (bytes).
    pub fn create_with_max_memory(
        path: String,
        max_memory_bytes: usize,
    ) -> Result<Connection, String> {
        let opts = crate::storage::file::OpenOptions { max_memory_bytes };
        Database::create_with_options(std::path::Path::new(&path), &opts)
            .map(|db| Connection {
                inner: Some(db),
                pending_snapshot: None,
            })
            .map_err(|e| e.to_string())
    }

    /// Opens an in-memory database (no file created).
    pub fn open_in_memory() -> Result<Connection, String> {
        Database::open_in_memory()
            .map(|db| Connection {
                inner: Some(db),
                pending_snapshot: None,
            })
            .map_err(|e| e.to_string())
    }

    /// Flushes pending writes and releases the database (file lock included).
    /// After this call the connection is closed; any further method call returns an error.
    /// If a transaction is pending, it is rolled back before the flush.
    pub fn close(&mut self) -> Result<(), String> {
        // Roll back any pending transaction before flushing; no-op if none is active.
        let _ = self.rollback_transaction();
        if let Some(mut db) = self.inner.take() {
            db.flush().map_err(|e| e.to_string())?;
            // db is dropped here, releasing DatabaseFile and its file lock.
        }
        Ok(())
    }

    /// Inserts a node by type name.
    pub fn insert_node(
        &mut self,
        type_name: String,
        props_json: String,
    ) -> Result<RecordId, String> {
        let props: HashMap<String, Value> =
            serde_json::from_str(&props_json).map_err(|e| e.to_string())?;
        self.db_mut()?
            .insert_node_by_name(&type_name, props)
            .map(|rid| RecordId {
                page: rid.page_id().0,
                slot: rid.slot_id().0,
            })
            .map_err(|e| e.to_string())
    }

    /// Inserts an edge by type name.
    pub fn insert_edge(
        &mut self,
        type_name: String,
        from: RecordId,
        to: RecordId,
        props_json: String,
    ) -> Result<RecordId, String> {
        let props: HashMap<String, Value> =
            serde_json::from_str(&props_json).map_err(|e| e.to_string())?;
        let from_rid = crate::storage::page::NodeRid::new(from.page, from.slot);
        let to_rid = crate::storage::page::NodeRid::new(to.page, to.slot);
        self.db_mut()?
            .insert_edge_by_name(&type_name, from_rid, to_rid, props)
            .map(|rid| RecordId {
                page: rid.page_id().0,
                slot: rid.slot_id().0,
            })
            .map_err(|e| e.to_string())
    }

    /// Deletes a node (with cascade deletion of associated edges).
    pub fn delete_node(&mut self, rid: RecordId) -> Result<(), String> {
        let rid = crate::storage::page::NodeRid::new(rid.page, rid.slot);
        self.db_mut()?.delete_node(rid).map_err(|e| e.to_string())
    }

    /// Deletes an edge.
    pub fn delete_edge(&mut self, rid: RecordId) -> Result<(), String> {
        let rid = crate::storage::page::EdgeRid::new(rid.page, rid.slot);
        self.db_mut()?.delete_edge(rid).map_err(|e| e.to_string())
    }

    /// Updates node properties (overwrites existing values).
    pub fn update_node_properties(
        &mut self,
        rid: RecordId,
        props_json: String,
    ) -> Result<(), String> {
        let rid = crate::storage::page::NodeRid::new(rid.page, rid.slot);
        let props: HashMap<String, Value> =
            serde_json::from_str(&props_json).map_err(|e| e.to_string())?;
        self.db_mut()?
            .update_node_properties(rid, props)
            .map_err(|e| e.to_string())
    }

    /// Updates edge properties (overwrites existing values).
    pub fn update_edge_properties(
        &mut self,
        rid: RecordId,
        props_json: String,
    ) -> Result<(), String> {
        let rid = crate::storage::page::EdgeRid::new(rid.page, rid.slot);
        let props: HashMap<String, Value> =
            serde_json::from_str(&props_json).map_err(|e| e.to_string())?;
        self.db_mut()?
            .update_edge_properties(rid, props)
            .map_err(|e| e.to_string())
    }

    /// Returns all node RecordIds and their properties as a JSON string. Optionally filtered by type_name.
    pub fn list_nodes(&mut self, type_name: Option<String>) -> Result<String, String> {
        let nodes = self
            .db_mut()?
            .list_nodes(type_name.as_deref())
            .map_err(|e| e.to_string())?;
        let items: Vec<serde_json::Value> = nodes
            .into_iter()
            .map(|(rid, props)| {
                serde_json::json!({
                    "rid": { "page": rid.page_id().0, "slot": rid.slot_id().0 },
                    "props": props,
                })
            })
            .collect();
        serde_json::to_string(&items).map_err(|e| e.to_string())
    }

    /// Returns all edge RecordIds and their properties as a JSON string. Optionally filtered by type_name.
    pub fn list_edges(&mut self, type_name: Option<String>) -> Result<String, String> {
        let edges = self
            .db_mut()?
            .list_edges(type_name.as_deref())
            .map_err(|e| e.to_string())?;
        let mut items = Vec::with_capacity(edges.len());
        for (rid, props) in edges {
            let (src_rid, dst_rid) = self
                .db_mut()?
                .get_edge_endpoints(rid)
                .map_err(|e| e.to_string())?;
            items.push(serde_json::json!({
                "rid": { "page": rid.page_id().0, "slot": rid.slot_id().0 },
                "src_rid": { "page": src_rid.page_id().0, "slot": src_rid.slot_id().0 },
                "dst_rid": { "page": dst_rid.page_id().0, "slot": dst_rid.slot_id().0 },
                "props": props,
            }));
        }
        serde_json::to_string(&items).map_err(|e| e.to_string())
    }

    /// Parses, validates, and saves the schema text to the database.
    pub fn apply_schema(&mut self, schema_text: String) -> Result<(), String> {
        let ast = crate::schema::parser::parse(&schema_text).map_err(|e| e.to_string())?;
        crate::schema::validator::validate(&ast).map_err(|e| e.to_string())?;
        crate::schema::store::save_schema(&mut self.db_mut()?.file, &ast).map_err(|e| e.to_string())
    }

    /// Returns the schema stored in the database as DSL source text.
    pub fn get_schema_text(&mut self) -> Result<String, String> {
        self.db_mut()?.get_schema_text().map_err(|e| e.to_string())
    }

    /// Returns node properties as a JSON string.
    pub fn get_node_properties(&mut self, rid: RecordId) -> Result<String, String> {
        let rid = crate::storage::page::NodeRid::new(rid.page, rid.slot);
        let props = self
            .db_mut()?
            .get_node_properties(rid)
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&props).map_err(|e| e.to_string())
    }

    /// Returns edge properties as a JSON string.
    pub fn get_edge_properties(&mut self, rid: RecordId) -> Result<String, String> {
        let rid = crate::storage::page::EdgeRid::new(rid.page, rid.slot);
        let props = self
            .db_mut()?
            .get_edge_properties(rid)
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&props).map_err(|e| e.to_string())
    }

    /// Executes a Cypher query and returns the result as JSON.
    pub fn execute_cypher(&mut self, query: String) -> Result<String, String> {
        crate::cypher::execute_cypher_query(&mut *self.db_mut()?, &query)
            .map_err(|e| e.to_string())
            .and_then(|r| r.to_json_string().map_err(|e| e.to_string()))
    }

    /// Executes a traversal command and returns the result as JSON.
    /// Returns a JSON array for CollectResult::Rows, or `{"count": N}` for Count.
    pub fn execute_traversal(&mut self, command_json: String) -> Result<String, String> {
        use crate::traversal::command::CollectResult;
        let cmd = serde_json::from_str(&command_json).map_err(|e| e.to_string())?;
        let view = DatabaseGraphView::load(&mut *self.db_mut()?).map_err(|e| e.to_string())?;
        let result = execute(&view, &cmd).map_err(|e| e.to_string())?;
        match result {
            CollectResult::Rows(rows) => serde_json::to_string(&rows).map_err(|e| e.to_string()),
            CollectResult::Count(n) => {
                serde_json::to_string(&serde_json::json!({ "count": n })).map_err(|e| e.to_string())
            }
            CollectResult::Exists(b) => serde_json::to_string(&serde_json::json!({ "exists": b }))
                .map_err(|e| e.to_string()),
            CollectResult::Aggregate(map) => serde_json::to_string(&map).map_err(|e| e.to_string()),
            CollectResult::Groups(groups) => {
                serde_json::to_string(&groups).map_err(|e| e.to_string())
            }
            CollectResult::Path(paths) => serde_json::to_string(&paths).map_err(|e| e.to_string()),
        }
    }

    /// Returns all label names of a node as a JSON array string.
    pub fn get_node_labels(&mut self, rid: RecordId) -> Result<String, String> {
        let rid = crate::storage::page::NodeRid::new(rid.page, rid.slot);
        let labels = self
            .db_mut()?
            .get_node_label_names(rid)
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&labels).map_err(|e| e.to_string())
    }

    /// Adds an additional label to a node (by type name).
    pub fn add_node_label(&mut self, rid: RecordId, type_name: String) -> Result<(), String> {
        let rid = crate::storage::page::NodeRid::new(rid.page, rid.slot);
        self.db_mut()?
            .add_node_label_by_name(rid, &type_name)
            .map_err(|e| e.to_string())
    }

    /// Removes a label from a node (by type name).
    pub fn remove_node_label(&mut self, rid: RecordId, type_name: String) -> Result<(), String> {
        let rid = crate::storage::page::NodeRid::new(rid.page, rid.slot);
        self.db_mut()?
            .remove_node_label_by_name(rid, &type_name)
            .map_err(|e| e.to_string())
    }

    /// Returns all label names of an edge as a JSON array string.
    pub fn get_edge_labels(&mut self, rid: RecordId) -> Result<String, String> {
        let rid = crate::storage::page::EdgeRid::new(rid.page, rid.slot);
        let labels = self
            .db_mut()?
            .get_edge_label_names(rid)
            .map_err(|e| e.to_string())?;
        serde_json::to_string(&labels).map_err(|e| e.to_string())
    }

    /// Adds an additional label to an edge (by type name).
    pub fn add_edge_label(&mut self, rid: RecordId, type_name: String) -> Result<(), String> {
        let rid = crate::storage::page::EdgeRid::new(rid.page, rid.slot);
        self.db_mut()?
            .add_edge_label_by_name(rid, &type_name)
            .map_err(|e| e.to_string())
    }

    /// Removes a label from an edge (by type name).
    pub fn remove_edge_label(&mut self, rid: RecordId, type_name: String) -> Result<(), String> {
        let rid = crate::storage::page::EdgeRid::new(rid.page, rid.slot);
        self.db_mut()?
            .remove_edge_label_by_name(rid, &type_name)
            .map_err(|e| e.to_string())
    }

    /// Finds the shortest path using BFS.
    /// Returns JSON in the form `{"nodes":[{"page":N,"slot":N},...], "edges":[...]}`.
    /// Returns `None` if no path exists.
    pub fn shortest_path(
        &mut self,
        from: RecordId,
        to: RecordId,
        max_depth: usize,
        direction: String,
        edge_labels: Option<Vec<String>>,
    ) -> Result<Option<String>, String> {
        let from_rid = crate::storage::page::RecordId::new(from.page, from.slot);
        let to_rid = crate::storage::page::RecordId::new(to.page, to.slot);
        let dir = parse_direction(&direction)?;
        let edge_type_ids = resolve_edge_type_ids(&mut *self.db_mut()?, &edge_labels)?;
        let opts = PathOptions {
            max_depth,
            direction: dir,
            edge_type_ids,
        };

        match self
            .db_mut()?
            .shortest_path(from_rid, to_rid, opts)
            .map_err(|e| e.to_string())?
        {
            None => Ok(None),
            Some(path) => Ok(Some(path_result_to_json(&path.node_rids, &path.edge_rids))),
        }
    }

    /// Returns the connecting subgraph between multiple nodes.
    /// Result is JSON in the form `{"nodes":[...],"edges":[...]}`.
    pub fn connecting_subgraph(
        &mut self,
        node_rids: Vec<RecordId>,
        max_depth: usize,
        direction: String,
        edge_labels: Option<Vec<String>>,
    ) -> Result<String, String> {
        let rids: Vec<crate::storage::page::RecordId> = node_rids
            .into_iter()
            .map(|r| crate::storage::page::RecordId::new(r.page, r.slot))
            .collect();
        let dir = parse_direction(&direction)?;
        let edge_type_ids = resolve_edge_type_ids(&mut *self.db_mut()?, &edge_labels)?;
        let opts = PathOptions {
            max_depth,
            direction: dir,
            edge_type_ids,
        };

        let sg = self
            .db_mut()?
            .connecting_subgraph(rids, opts)
            .map_err(|e| e.to_string())?;
        Ok(path_result_to_json(&sg.node_rids, &sg.edge_rids))
    }

    /// Inserts a node with multiple labels.
    pub fn insert_node_with_labels(
        &mut self,
        primary_type: String,
        additional_labels: Vec<String>,
        props_json: String,
    ) -> Result<RecordId, String> {
        let props: HashMap<String, Value> =
            serde_json::from_str(&props_json).map_err(|e| e.to_string())?;
        let additional_refs: Vec<&str> = additional_labels.iter().map(|s| s.as_str()).collect();
        self.db_mut()?
            .insert_node_with_label_names(&primary_type, additional_refs, props)
            .map(|rid| RecordId {
                page: rid.page_id().0,
                slot: rid.slot_id().0,
            })
            .map_err(|e| e.to_string())
    }

    /// Returns the primary type name of a node.
    pub fn get_node_type_name(&mut self, rid: RecordId) -> Result<String, String> {
        let rid = crate::storage::page::NodeRid::new(rid.page, rid.slot);
        self.db_mut()?
            .get_node_type_name(rid)
            .map_err(|e| e.to_string())
    }

    /// Returns the primary type name of an edge.
    pub fn get_edge_type_name(&mut self, rid: RecordId) -> Result<String, String> {
        let rid = crate::storage::page::EdgeRid::new(rid.page, rid.slot);
        self.db_mut()?
            .get_edge_type_name(rid)
            .map_err(|e| e.to_string())
    }

    /// Returns the source and destination RecordIds of an edge as a JSON string: `{"from":{...},"to":{...}}`.
    pub fn get_edge_endpoints(&mut self, rid: RecordId) -> Result<String, String> {
        let rid = crate::storage::page::EdgeRid::new(rid.page, rid.slot);
        let (from, to) = self
            .db_mut()?
            .get_edge_endpoints(rid)
            .map_err(|e| e.to_string())?;
        let json = serde_json::json!({
            "from": { "page": from.page_id().0, "slot": from.slot_id().0 },
            "to":   { "page": to.page_id().0,   "slot": to.slot_id().0   },
        });
        serde_json::to_string(&json).map_err(|e| e.to_string())
    }

    /// Vacuums the in-memory database (compacts deleted slots).
    /// For file-backed databases, use `vacuum_file` instead.
    pub fn vacuum(&mut self) -> Result<(), String> {
        self.db_mut()?.vacuum().map_err(|e| e.to_string())
    }

    /// Vacuums a file-backed database at the given path.
    /// The database must not be open by any other process.
    pub fn vacuum_file(path: String) -> Result<(), String> {
        Database::vacuum_file(std::path::Path::new(&path)).map_err(|e| e.to_string())
    }

    /// Runs integrity checks and returns a list of warning strings.
    /// An empty list means the database is healthy.
    pub fn verify(&mut self) -> Result<Vec<String>, String> {
        self.db_mut()?.verify().map_err(|e| e.to_string())
    }

    // ---- Existence check ----

    /// Returns true if a node with the given RecordId exists.
    pub fn node_exists(&mut self, rid: RecordId) -> bool {
        let rid = crate::storage::page::NodeRid::new(rid.page, rid.slot);
        self.db_mut().map(|db| db.node_exists(rid)).unwrap_or(false)
    }

    /// Returns true if an edge with the given RecordId exists.
    pub fn edge_exists(&mut self, rid: RecordId) -> bool {
        let rid = crate::storage::page::EdgeRid::new(rid.page, rid.slot);
        self.db_mut().map(|db| db.edge_exists(rid)).unwrap_or(false)
    }

    // ---- Count ----

    /// Returns the number of nodes, optionally filtered by type_name.
    pub fn count_nodes(&mut self, type_name: Option<String>) -> Result<u64, String> {
        self.db_mut()?
            .count_nodes(type_name.as_deref())
            .map_err(|e| e.to_string())
    }

    /// Returns the number of edges, optionally filtered by type_name.
    pub fn count_edges(&mut self, type_name: Option<String>) -> Result<u64, String> {
        self.db_mut()?
            .count_edges(type_name.as_deref())
            .map_err(|e| e.to_string())
    }

    // ---- Patch properties ----

    /// Updates only the specified keys in a node's properties. Unspecified keys are preserved.
    pub fn patch_node_properties(
        &mut self,
        rid: RecordId,
        patch_json: String,
    ) -> Result<(), String> {
        let rid = crate::storage::page::NodeRid::new(rid.page, rid.slot);
        let patch: HashMap<String, Value> =
            serde_json::from_str(&patch_json).map_err(|e| e.to_string())?;
        self.db_mut()?
            .patch_node_properties(rid, patch)
            .map_err(|e| e.to_string())
    }

    /// Updates only the specified keys in an edge's properties. Unspecified keys are preserved.
    pub fn patch_edge_properties(
        &mut self,
        rid: RecordId,
        patch_json: String,
    ) -> Result<(), String> {
        let rid = crate::storage::page::EdgeRid::new(rid.page, rid.slot);
        let patch: HashMap<String, Value> =
            serde_json::from_str(&patch_json).map_err(|e| e.to_string())?;
        self.db_mut()?
            .patch_edge_properties(rid, patch)
            .map_err(|e| e.to_string())
    }

    // ---- Schema query ----

    /// Returns the names of all node types defined in the schema.
    pub fn node_type_names(&mut self) -> Result<Vec<String>, String> {
        self.db_mut()?.node_type_names().map_err(|e| e.to_string())
    }

    /// Returns the names of all edge types defined in the schema.
    pub fn edge_type_names(&mut self) -> Result<Vec<String>, String> {
        self.db_mut()?.edge_type_names().map_err(|e| e.to_string())
    }

    /// Returns the property definitions for a node type as a JSON string.
    /// Returns `"null"` if the type is not found.
    pub fn node_type_schema(&mut self, type_name: String) -> Result<String, String> {
        match self
            .db_mut()?
            .node_type_schema(&type_name)
            .map_err(|e| e.to_string())?
        {
            None => Ok("null".to_string()),
            Some(s) => schema_to_json(&s.name, &s.properties),
        }
    }

    /// Returns the property definitions for an edge type as a JSON string.
    /// Returns `"null"` if the type is not found.
    pub fn edge_type_schema(&mut self, type_name: String) -> Result<String, String> {
        match self
            .db_mut()?
            .edge_type_schema(&type_name)
            .map_err(|e| e.to_string())?
        {
            None => Ok("null".to_string()),
            Some(s) => schema_to_json(&s.name, &s.properties),
        }
    }

    // ---- Transaction (flat state machine — consumed by FFI) ----

    /// Begins a transaction. Returns an error if one is already in progress.
    pub fn begin_transaction(&mut self) -> Result<(), String> {
        if self.pending_snapshot.is_some() {
            return Err("transaction already in progress".to_string());
        }
        let snap = self.db_mut()?.take_snapshot();
        self.pending_snapshot = Some(snap);
        Ok(())
    }

    /// Commits the current transaction, flushing all writes to disk.
    pub fn commit_transaction(&mut self) -> Result<(), String> {
        if self.pending_snapshot.take().is_none() {
            return Err("no transaction in progress".to_string());
        }
        self.db_mut()?.flush().map_err(|e| e.to_string())
    }

    /// Rolls back the current transaction, restoring the pre-begin snapshot.
    pub fn rollback_transaction(&mut self) -> Result<(), String> {
        match self.pending_snapshot.take() {
            None => Err("no transaction in progress".to_string()),
            Some(snap) => {
                self.db_mut()?.restore_snapshot(snap);
                Ok(())
            }
        }
    }

    // ---- Transaction (RAII wrapper — type-safe API) ----

    /// Begins a transaction and returns a `Transaction` RAII guard.
    /// Returns an error if a transaction is already in progress.
    /// Dropping the guard without committing automatically rolls back.
    #[cfg(test)]
    pub(crate) fn begin(&mut self) -> Result<Transaction<'_>, String> {
        self.begin_transaction()?;
        Ok(Transaction { conn: self })
    }
}

// ---- Transaction ----

/// An active transaction on a `Connection`.
///
/// All write operations go through the transaction. Call `commit()` to persist
/// them, or `rollback()` (or simply drop) to discard them.
///
/// This is a thin RAII wrapper over `Connection::begin_transaction` /
/// `commit_transaction` / `rollback_transaction`. The snapshot is held inside
/// `Connection::pending_snapshot` — not here — so both APIs share a single state.
pub struct Transaction<'a> {
    conn: &'a mut Connection,
}

impl<'a> Transaction<'a> {
    /// Commits the transaction. All writes become permanent (flushed to disk).
    pub fn commit(self) -> Result<(), String> {
        self.conn.commit_transaction()
    }

    /// Rolls back the transaction, discarding all writes since `begin()`.
    pub fn rollback(self) -> Result<(), String> {
        self.conn.rollback_transaction()
    }

    // Delegate all write (and read) methods to the inner Connection.

    pub fn insert_node(
        &mut self,
        type_name: String,
        props_json: String,
    ) -> Result<RecordId, String> {
        self.conn.insert_node(type_name, props_json)
    }

    pub fn insert_edge(
        &mut self,
        type_name: String,
        from: RecordId,
        to: RecordId,
        props_json: String,
    ) -> Result<RecordId, String> {
        self.conn.insert_edge(type_name, from, to, props_json)
    }

    pub fn delete_node(&mut self, rid: RecordId) -> Result<(), String> {
        self.conn.delete_node(rid)
    }

    pub fn delete_edge(&mut self, rid: RecordId) -> Result<(), String> {
        self.conn.delete_edge(rid)
    }

    pub fn update_node_properties(
        &mut self,
        rid: RecordId,
        props_json: String,
    ) -> Result<(), String> {
        self.conn.update_node_properties(rid, props_json)
    }

    pub fn update_edge_properties(
        &mut self,
        rid: RecordId,
        props_json: String,
    ) -> Result<(), String> {
        self.conn.update_edge_properties(rid, props_json)
    }

    pub fn patch_node_properties(
        &mut self,
        rid: RecordId,
        patch_json: String,
    ) -> Result<(), String> {
        self.conn.patch_node_properties(rid, patch_json)
    }

    pub fn patch_edge_properties(
        &mut self,
        rid: RecordId,
        patch_json: String,
    ) -> Result<(), String> {
        self.conn.patch_edge_properties(rid, patch_json)
    }

    pub fn apply_schema(&mut self, schema_text: String) -> Result<(), String> {
        self.conn.apply_schema(schema_text)
    }

    pub fn get_node_properties(&mut self, rid: RecordId) -> Result<String, String> {
        self.conn.get_node_properties(rid)
    }

    pub fn get_edge_properties(&mut self, rid: RecordId) -> Result<String, String> {
        self.conn.get_edge_properties(rid)
    }

    pub fn list_nodes(&mut self, type_name: Option<String>) -> Result<String, String> {
        self.conn.list_nodes(type_name)
    }

    pub fn list_edges(&mut self, type_name: Option<String>) -> Result<String, String> {
        self.conn.list_edges(type_name)
    }

    pub fn node_exists(&mut self, rid: RecordId) -> bool {
        self.conn.node_exists(rid)
    }

    pub fn edge_exists(&mut self, rid: RecordId) -> bool {
        self.conn.edge_exists(rid)
    }

    pub fn count_nodes(&mut self, type_name: Option<String>) -> Result<u64, String> {
        self.conn.count_nodes(type_name)
    }

    pub fn count_edges(&mut self, type_name: Option<String>) -> Result<u64, String> {
        self.conn.count_edges(type_name)
    }
}

impl<'a> Drop for Transaction<'a> {
    fn drop(&mut self) {
        // If already committed, pending_snapshot is None and rollback_transaction
        // returns an error which we intentionally ignore here.
        let _ = self.conn.rollback_transaction();
    }
}

// ---- Helper functions ----

fn parse_direction(s: &str) -> Result<PathDirection, String> {
    match s {
        "Outgoing" => Ok(PathDirection::Outgoing),
        "Incoming" => Ok(PathDirection::Incoming),
        "Both" => Ok(PathDirection::Both),
        other => Err(format!(
            "unknown direction: '{other}'. Use Outgoing/Incoming/Both"
        )),
    }
}

/// Converts a list of edge label names to type_id values.
/// Returns `None` (no filter) or `Some(Vec<u16>)`.
fn resolve_edge_type_ids(
    db: &mut Database,
    edge_labels: &Option<Vec<String>>,
) -> Result<Option<Vec<u16>>, String> {
    match edge_labels {
        None => Ok(None),
        Some(labels) => {
            let registry = db.load_schema_registry().map_err(|e| e.to_string())?;
            let ids: Result<Vec<u16>, String> = labels
                .iter()
                .map(|name| {
                    registry
                        .edge_type_id(name)
                        .ok_or_else(|| format!("unknown edge type: '{name}'"))
                })
                .collect();
            Ok(Some(ids?))
        }
    }
}

fn schema_to_json(name: &str, properties: &[crate::db::PropertyDef]) -> Result<String, String> {
    let props: Vec<serde_json::Value> = properties
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.name,
                "value_type": p.value_type,
                "nullable": p.nullable,
            })
        })
        .collect();
    serde_json::to_string(&serde_json::json!({
        "name": name,
        "properties": props,
    }))
    .map_err(|e| e.to_string())
}

fn path_result_to_json(
    node_rids: &[crate::storage::page::RecordId],
    edge_rids: &[crate::storage::page::RecordId],
) -> String {
    let nodes: Vec<serde_json::Value> = node_rids
        .iter()
        .map(|r| serde_json::json!({"page": r.page_id.0, "slot": r.slot_id.0}))
        .collect();
    let edges: Vec<serde_json::Value> = edge_rids
        .iter()
        .map(|r| serde_json::json!({"page": r.page_id.0, "slot": r.slot_id.0}))
        .collect();
    serde_json::json!({"nodes": nodes, "edges": edges}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    const SCHEMA: &str = "
node Person {
  id: String
  name: String
}

edge FOLLOWS {
  from: Person
  to: Person
  props: {
    weight: Int
  }
}
";

    fn make_connection() -> Connection {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.into_temp_path().to_path_buf();
        std::fs::remove_file(&path).ok();
        let mut conn =
            Connection::create(path.to_string_lossy().into_owned()).expect("create connection");
        conn.apply_schema(SCHEMA.to_string()).expect("apply schema");
        conn
    }

    #[test]
    fn list_edges_includes_endpoint_rids() {
        let mut conn = make_connection();
        let alice = conn
            .insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
            .expect("insert alice");
        let bob = conn
            .insert_node("Person".to_string(), r#"{"id": "u2"}"#.to_string())
            .expect("insert bob");
        let edge = conn
            .insert_edge(
                "FOLLOWS".to_string(),
                RecordId {
                    page: alice.page,
                    slot: alice.slot,
                },
                RecordId {
                    page: bob.page,
                    slot: bob.slot,
                },
                "{}".to_string(),
            )
            .expect("insert edge");

        let json = conn
            .list_edges(Some("FOLLOWS".to_string()))
            .expect("list edges");
        let items: Vec<serde_json::Value> = serde_json::from_str(&json).expect("parse json");
        assert_eq!(items.len(), 1);

        let item = &items[0];
        assert_eq!(item["rid"]["page"], serde_json::json!(edge.page));
        assert_eq!(item["rid"]["slot"], serde_json::json!(edge.slot));
        assert_eq!(item["src_rid"]["page"], serde_json::json!(alice.page));
        assert_eq!(item["src_rid"]["slot"], serde_json::json!(alice.slot));
        assert_eq!(item["dst_rid"]["page"], serde_json::json!(bob.page));
        assert_eq!(item["dst_rid"]["slot"], serde_json::json!(bob.slot));
    }

    #[test]
    fn node_exists_and_edge_exists() {
        let mut conn = make_connection();
        let alice = conn
            .insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
            .expect("insert alice");
        let bob = conn
            .insert_node("Person".to_string(), r#"{"id": "u2"}"#.to_string())
            .expect("insert bob");
        let edge = conn
            .insert_edge(
                "FOLLOWS".to_string(),
                RecordId {
                    page: alice.page,
                    slot: alice.slot,
                },
                RecordId {
                    page: bob.page,
                    slot: bob.slot,
                },
                "{}".to_string(),
            )
            .expect("insert edge");

        assert!(conn.node_exists(RecordId {
            page: alice.page,
            slot: alice.slot
        }));
        assert!(conn.edge_exists(RecordId {
            page: edge.page,
            slot: edge.slot
        }));
        assert!(!conn.node_exists(RecordId { page: 99, slot: 99 }));
        assert!(!conn.edge_exists(RecordId { page: 99, slot: 99 }));

        conn.delete_node(RecordId {
            page: alice.page,
            slot: alice.slot,
        })
        .expect("delete alice");
        assert!(!conn.node_exists(RecordId {
            page: alice.page,
            slot: alice.slot
        }));
    }

    #[test]
    fn count_nodes_and_edges() {
        let mut conn = make_connection();
        assert_eq!(conn.count_nodes(None).unwrap(), 0);
        assert_eq!(conn.count_edges(None).unwrap(), 0);

        let alice = conn
            .insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
            .expect("insert alice");
        let bob = conn
            .insert_node("Person".to_string(), r#"{"id": "u2"}"#.to_string())
            .expect("insert bob");
        conn.insert_edge(
            "FOLLOWS".to_string(),
            RecordId {
                page: alice.page,
                slot: alice.slot,
            },
            RecordId {
                page: bob.page,
                slot: bob.slot,
            },
            "{}".to_string(),
        )
        .expect("insert edge");

        assert_eq!(conn.count_nodes(None).unwrap(), 2);
        assert_eq!(conn.count_nodes(Some("Person".to_string())).unwrap(), 2);
        assert_eq!(conn.count_nodes(Some("Unknown".to_string())).unwrap(), 0);
        assert_eq!(conn.count_edges(None).unwrap(), 1);
        assert_eq!(conn.count_edges(Some("FOLLOWS".to_string())).unwrap(), 1);
        assert_eq!(conn.count_edges(Some("Unknown".to_string())).unwrap(), 0);
    }

    #[test]
    fn patch_node_and_edge_properties() {
        let mut conn = make_connection();
        let alice = conn
            .insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
            .expect("insert alice");
        let bob = conn
            .insert_node("Person".to_string(), r#"{"id": "u2"}"#.to_string())
            .expect("insert bob");
        let edge = conn
            .insert_edge(
                "FOLLOWS".to_string(),
                RecordId {
                    page: alice.page,
                    slot: alice.slot,
                },
                RecordId {
                    page: bob.page,
                    slot: bob.slot,
                },
                r#"{"weight": 1}"#.to_string(),
            )
            .expect("insert edge");

        conn.patch_node_properties(
            RecordId {
                page: alice.page,
                slot: alice.slot,
            },
            r#"{"name": "Alice"}"#.to_string(),
        )
        .expect("patch node");
        let props: serde_json::Value = serde_json::from_str(
            &conn
                .get_node_properties(RecordId {
                    page: alice.page,
                    slot: alice.slot,
                })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(props["id"], "u1");
        assert_eq!(props["name"], "Alice");

        conn.patch_edge_properties(
            RecordId {
                page: edge.page,
                slot: edge.slot,
            },
            r#"{"weight": 2}"#.to_string(),
        )
        .expect("patch edge");
        let eprops: serde_json::Value = serde_json::from_str(
            &conn
                .get_edge_properties(RecordId {
                    page: edge.page,
                    slot: edge.slot,
                })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(eprops["weight"], 2);
    }

    #[test]
    fn schema_query_methods() {
        let mut conn = make_connection();

        let node_names = conn.node_type_names().unwrap();
        assert_eq!(node_names, vec!["Person"]);

        let edge_names = conn.edge_type_names().unwrap();
        assert_eq!(edge_names, vec!["FOLLOWS"]);

        let schema_json = conn.node_type_schema("Person".to_string()).unwrap();
        let schema: serde_json::Value = serde_json::from_str(&schema_json).unwrap();
        assert_eq!(schema["name"], "Person");
        assert_eq!(schema["properties"][0]["name"], "id");
        assert_eq!(schema["properties"][0]["value_type"], "String");

        let not_found = conn.node_type_schema("Unknown".to_string()).unwrap();
        assert_eq!(not_found, "null");

        let edge_schema_json = conn.edge_type_schema("FOLLOWS".to_string()).unwrap();
        let edge_schema: serde_json::Value = serde_json::from_str(&edge_schema_json).unwrap();
        assert_eq!(edge_schema["name"], "FOLLOWS");
    }

    #[test]
    fn transaction_commit_persists_writes() {
        let mut conn = make_connection();
        {
            let mut tx = conn.begin().expect("begin");
            tx.insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
                .expect("insert in tx");
            tx.commit().expect("commit");
        }
        assert_eq!(conn.count_nodes(None).unwrap(), 1);
    }

    #[test]
    fn transaction_rollback_discards_writes() {
        let mut conn = make_connection();
        {
            let mut tx = conn.begin().expect("begin");
            tx.insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
                .expect("insert in tx");
            tx.rollback().expect("rollback");
        }
        assert_eq!(conn.count_nodes(None).unwrap(), 0);
    }

    #[test]
    fn transaction_drop_rolls_back() {
        let mut conn = make_connection();
        {
            let mut tx = conn.begin().expect("begin");
            tx.insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
                .expect("insert in tx");
            // tx dropped here without commit → rollback
        }
        assert_eq!(conn.count_nodes(None).unwrap(), 0);
    }

    #[test]
    fn transaction_rollback_restores_deleted_nodes() {
        let mut conn = make_connection();
        let alice = conn
            .insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
            .expect("insert alice");

        {
            let mut tx = conn.begin().expect("begin");
            tx.delete_node(RecordId {
                page: alice.page,
                slot: alice.slot,
            })
            .expect("delete in tx");
            assert!(!tx.node_exists(RecordId {
                page: alice.page,
                slot: alice.slot
            }));
            tx.rollback().expect("rollback");
        }
        assert!(conn.node_exists(RecordId {
            page: alice.page,
            slot: alice.slot
        }));
    }

    #[test]
    fn transaction_commit_batch_insert() {
        let mut conn = make_connection();
        {
            let mut tx = conn.begin().expect("begin");
            for i in 0..5 {
                tx.insert_node("Person".to_string(), format!(r#"{{"id": "u{i}"}}"#))
                    .expect("insert");
            }
            tx.commit().expect("commit");
        }
        assert_eq!(conn.count_nodes(None).unwrap(), 5);
    }

    // ---- Transaction durability (file backend) ----

    fn make_file_connection() -> (Connection, tempfile::TempPath) {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.into_temp_path();
        std::fs::remove_file(&path).ok();
        let mut conn =
            Connection::create(path.to_string_lossy().into_owned()).expect("create connection");
        conn.apply_schema(SCHEMA.to_string()).expect("apply schema");
        (conn, path)
    }

    #[test]
    fn commit_flushes_to_disk() {
        let (mut conn, path) = make_file_connection();
        {
            let mut tx = conn.begin().expect("begin");
            tx.insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
                .expect("insert in tx");
            tx.commit().expect("commit");
        }
        drop(conn);

        // Re-open and verify the node survived
        let mut conn2 = Connection::open(path.to_string_lossy().into_owned()).expect("reopen");
        assert_eq!(conn2.count_nodes(None).unwrap(), 1);
    }

    #[test]
    fn rollback_does_not_persist_to_disk() {
        let (mut conn, path) = make_file_connection();
        {
            let mut tx = conn.begin().expect("begin");
            tx.insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
                .expect("insert in tx");
            tx.rollback().expect("rollback");
        }
        drop(conn);

        // Re-open and verify the node was NOT persisted
        let mut conn2 = Connection::open(path.to_string_lossy().into_owned()).expect("reopen");
        assert_eq!(conn2.count_nodes(None).unwrap(), 0);
    }

    // ---- File lock ----

    #[test]
    fn reopen_after_close_succeeds() {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.into_temp_path();
        std::fs::remove_file(&path).ok();
        let path_str = path.to_string_lossy().into_owned();

        let mut conn = Connection::create(path_str.clone()).expect("create");
        conn.apply_schema(SCHEMA.to_string()).expect("apply schema");
        conn.insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
            .expect("insert");
        conn.close().expect("close");

        // After close(), the same process must be able to reopen the file.
        let mut conn2 = Connection::open(path_str).expect("reopen after close");
        assert_eq!(conn2.count_nodes(None).unwrap(), 1);
    }

    #[test]
    fn method_after_close_returns_error() {
        let mut conn = Connection::open_in_memory().expect("open in memory");
        conn.close().expect("close");
        assert!(conn.count_nodes(None).is_err());
    }

    #[test]
    fn double_open_is_rejected() {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.into_temp_path();
        std::fs::remove_file(&path).ok();
        let _conn1 = Connection::create(path.to_string_lossy().into_owned()).expect("first open");
        // A second open on the same file must fail with an error.
        let result = Connection::open(path.to_string_lossy().into_owned());
        assert!(
            result.is_err(),
            "expected second open to fail, but it succeeded"
        );
    }

    // ---- close() with pending transaction ----

    #[test]
    fn close_with_pending_transaction_discards_writes() {
        let (mut conn, path) = make_file_connection();
        conn.begin_transaction().expect("begin");
        conn.insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
            .expect("insert");
        conn.close().expect("close");

        let mut conn2 = Connection::open(path.to_string_lossy().into_owned()).expect("reopen");
        assert_eq!(conn2.count_nodes(None).unwrap(), 0);
    }

    // ---- Plan C regression tests ----

    // 1. commit persists to disk (begin→insert→commit→reopen verifies count)
    #[test]
    fn flat_commit_persists_to_disk() {
        let (mut conn, path) = make_file_connection();
        conn.begin_transaction().expect("begin_transaction");
        conn.insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
            .expect("insert");
        conn.commit_transaction().expect("commit_transaction");
        drop(conn);

        let mut conn2 = Connection::open(path.to_string_lossy().into_owned()).expect("reopen");
        assert_eq!(conn2.count_nodes(None).unwrap(), 1);
    }

    // 2. rollback discards writes (begin→insert→rollback→count returns 0)
    #[test]
    fn flat_rollback_discards_writes() {
        let mut conn = make_connection();
        conn.begin_transaction().expect("begin_transaction");
        conn.insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
            .expect("insert");
        conn.rollback_transaction().expect("rollback_transaction");
        assert_eq!(conn.count_nodes(None).unwrap(), 0);
    }

    // 3. double begin_transaction returns an error
    #[test]
    fn flat_double_begin_is_error() {
        let mut conn = make_connection();
        conn.begin_transaction().expect("first begin_transaction");
        let result = conn.begin_transaction();
        assert!(
            result.is_err(),
            "expected error on double begin_transaction"
        );
    }

    // 4. commit_transaction / rollback_transaction without a transaction return errors
    #[test]
    fn flat_commit_without_transaction_is_error() {
        let mut conn = make_connection();
        assert!(conn.commit_transaction().is_err());
    }

    #[test]
    fn flat_rollback_without_transaction_is_error() {
        let mut conn = make_connection();
        assert!(conn.rollback_transaction().is_err());
    }

    // 5. rollback after commit is an error (pending_snapshot is already None)
    #[test]
    fn flat_rollback_after_commit_is_error() {
        let mut conn = make_connection();
        conn.begin_transaction().expect("begin_transaction");
        conn.commit_transaction().expect("commit_transaction");
        assert!(conn.rollback_transaction().is_err());
    }

    // 6. Plan-C specific: RAII begin() delegates to the flat state machine.
    //    Verifies commit, explicit rollback, and drop-based auto-rollback all
    //    go through the shared pending_snapshot in Connection.
    #[test]
    fn raii_and_flat_share_same_state_commit() {
        let mut conn = make_connection();
        {
            let mut tx = conn.begin().expect("begin");
            tx.insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
                .expect("insert");
            tx.commit().expect("commit via RAII");
        }
        // After RAII commit, no pending transaction remains.
        assert!(
            conn.commit_transaction().is_err(),
            "no tx after RAII commit"
        );
        assert_eq!(conn.count_nodes(None).unwrap(), 1);
    }

    #[test]
    fn raii_and_flat_share_same_state_rollback() {
        let mut conn = make_connection();
        {
            let mut tx = conn.begin().expect("begin");
            tx.insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
                .expect("insert");
            tx.rollback().expect("rollback via RAII");
        }
        // After RAII rollback, no pending transaction remains.
        assert!(
            conn.rollback_transaction().is_err(),
            "no tx after RAII rollback"
        );
        assert_eq!(conn.count_nodes(None).unwrap(), 0);
    }

    #[test]
    fn raii_drop_auto_rollback_via_flat_state() {
        let mut conn = make_connection();
        {
            let mut tx = conn.begin().expect("begin");
            tx.insert_node("Person".to_string(), r#"{"id": "u1"}"#.to_string())
                .expect("insert");
            // tx dropped without commit → Drop calls rollback_transaction()
        }
        // After drop-based rollback, no pending transaction remains.
        assert!(
            conn.rollback_transaction().is_err(),
            "no tx after drop rollback"
        );
        assert_eq!(conn.count_nodes(None).unwrap(), 0);
    }
}
