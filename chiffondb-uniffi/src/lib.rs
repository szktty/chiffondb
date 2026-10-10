//! UniFFI surface for ChiffonDB.
//!
//! This crate is a thin adapter over `chiffondb_core::api`. It exists alongside
//! `chiffondb-ffi` (flutter_rust_bridge) so that both clients can be served
//! without touching `chiffondb-core`.
//!
//! Scope is deliberately minimal for now: open a database and read nodes. The
//! remaining API is added as the Kotlin client needs it.

use std::sync::Mutex;

use chiffondb_core::api;

uniffi::setup_scaffolding!();

/// Errors crossing the FFI boundary.
///
/// `chiffondb_core::api` reports failures as `String`, so there is nothing
/// finer to map onto yet. A single variant keeps the Kotlin side to one
/// exception type; splitting it later is a source-compatible change on Rust's
/// side but not on Kotlin's, so it is worth doing before the API spreads.
///
/// The field is `reason`, not `message`: the generated Kotlin error extends
/// `Exception`, and a `message` field collides with `Throwable.message`.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ChiffonError {
    #[error("{reason}")]
    Db { reason: String },
}

impl From<String> for ChiffonError {
    fn from(reason: String) -> Self {
        ChiffonError::Db { reason }
    }
}

type Result<T> = std::result::Result<T, ChiffonError>;

/// A node or edge address.
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct RecordId {
    pub page: u32,
    pub slot: u16,
}

impl From<api::RecordId> for RecordId {
    fn from(rid: api::RecordId) -> Self {
        RecordId {
            page: rid.page,
            slot: rid.slot,
        }
    }
}

impl From<RecordId> for api::RecordId {
    fn from(rid: RecordId) -> Self {
        api::RecordId {
            page: rid.page,
            slot: rid.slot,
        }
    }
}

/// An open database.
///
/// UniFFI hands objects to the foreign side as `Arc<Self>` and therefore only
/// exposes `&self` methods, while every `api::Connection` method needs
/// `&mut self`. The `Mutex` bridges that gap and makes the handle `Sync`, which
/// UniFFI also requires.
#[derive(uniffi::Object)]
pub struct Connection {
    inner: Mutex<api::Connection>,
}

impl Connection {
    /// Runs `f` against the locked connection.
    ///
    /// A poisoned lock means another thread panicked mid-operation, so the
    /// database state is not trustworthy; it is surfaced as an error rather
    /// than recovered.
    fn with<T>(
        &self,
        f: impl FnOnce(&mut api::Connection) -> std::result::Result<T, String>,
    ) -> Result<T> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "connection lock is poisoned".to_string())?;
        Ok(f(&mut guard)?)
    }
}

#[uniffi::export]
impl Connection {
    /// Opens an existing database file.
    #[uniffi::constructor]
    pub fn open(path: String) -> Result<Connection> {
        let inner = api::Connection::open(path)?;
        Ok(Connection {
            inner: Mutex::new(inner),
        })
    }

    /// Creates a new database file and opens it.
    #[uniffi::constructor]
    pub fn create(path: String) -> Result<Connection> {
        let inner = api::Connection::create(path)?;
        Ok(Connection {
            inner: Mutex::new(inner),
        })
    }

    /// Opens a database that lives only in memory.
    #[uniffi::constructor]
    pub fn open_in_memory() -> Result<Connection> {
        let inner = api::Connection::open_in_memory()?;
        Ok(Connection {
            inner: Mutex::new(inner),
        })
    }

    /// Flushes pending writes and releases the file lock.
    ///
    /// Named `shutdown` rather than `close`: the generated Kotlin object already
    /// has a `close()` from `AutoCloseable`, which disposes the handle. This is
    /// the database-level flush and is a separate concern.
    pub fn shutdown(&self) -> Result<()> {
        self.with(|c| c.close())
    }

    /// Applies a schema definition.
    pub fn apply_schema(&self, schema_text: String) -> Result<()> {
        self.with(|c| c.apply_schema(schema_text))
    }

    /// Inserts a node of `type_name` with `props_json` as its properties.
    pub fn insert_node(&self, type_name: String, props_json: String) -> Result<RecordId> {
        self.with(|c| c.insert_node(type_name, props_json))
            .map(RecordId::from)
    }

    /// Returns one node's properties as a JSON object string.
    pub fn get_node_properties(&self, rid: RecordId) -> Result<String> {
        self.with(|c| c.get_node_properties(rid.into()))
    }

    /// Returns all nodes as a JSON array string, optionally filtered by type.
    pub fn list_nodes(&self, type_name: Option<String>) -> Result<String> {
        self.with(|c| c.list_nodes(type_name))
    }

    /// Counts nodes, optionally filtered by type.
    pub fn count_nodes(&self, type_name: Option<String>) -> Result<u64> {
        self.with(|c| c.count_nodes(type_name))
    }

    /// Replaces a node's properties with `props_json`.
    pub fn update_node_properties(&self, rid: RecordId, props_json: String) -> Result<()> {
        self.with(|c| c.update_node_properties(rid.into(), props_json))
    }

    /// Merges `props_json` into a node's properties, leaving the rest alone.
    pub fn patch_node_properties(&self, rid: RecordId, props_json: String) -> Result<()> {
        self.with(|c| c.patch_node_properties(rid.into(), props_json))
    }

    /// Deletes a node. Its edges go with it.
    pub fn delete_node(&self, rid: RecordId) -> Result<()> {
        self.with(|c| c.delete_node(rid.into()))
    }

    pub fn node_exists(&self, rid: RecordId) -> Result<bool> {
        self.with(|c| Ok(c.node_exists(rid.into())))
    }

    /// Returns a node's label names as a JSON array string.
    pub fn get_node_labels(&self, rid: RecordId) -> Result<String> {
        self.with(|c| c.get_node_labels(rid.into()))
    }

    /// Inserts an edge of `type_name` between two nodes.
    pub fn insert_edge(
        &self,
        type_name: String,
        from: RecordId,
        to: RecordId,
        props_json: String,
    ) -> Result<RecordId> {
        self.with(|c| c.insert_edge(type_name, from.into(), to.into(), props_json))
            .map(RecordId::from)
    }

    /// Returns all edges as a JSON array string, optionally filtered by type.
    ///
    /// Each entry carries its endpoints, so the client can rebuild the graph
    /// without asking for them one at a time.
    pub fn list_edges(&self, type_name: Option<String>) -> Result<String> {
        self.with(|c| c.list_edges(type_name))
    }

    /// Returns one edge's properties as a JSON object string.
    pub fn get_edge_properties(&self, rid: RecordId) -> Result<String> {
        self.with(|c| c.get_edge_properties(rid.into()))
    }

    /// Replaces an edge's properties with `props_json`.
    pub fn update_edge_properties(&self, rid: RecordId, props_json: String) -> Result<()> {
        self.with(|c| c.update_edge_properties(rid.into(), props_json))
    }

    pub fn delete_edge(&self, rid: RecordId) -> Result<()> {
        self.with(|c| c.delete_edge(rid.into()))
    }

    pub fn edge_exists(&self, rid: RecordId) -> Result<bool> {
        self.with(|c| Ok(c.edge_exists(rid.into())))
    }

    /// Returns an edge's endpoints as a JSON object string.
    pub fn get_edge_endpoints(&self, rid: RecordId) -> Result<String> {
        self.with(|c| c.get_edge_endpoints(rid.into()))
    }

    /// Returns the schema definition text stored in the database.
    pub fn get_schema_text(&self) -> Result<String> {
        self.with(|c| c.get_schema_text())
    }

    /// Returns the declared node type names.
    pub fn node_type_names(&self) -> Result<Vec<String>> {
        self.with(|c| c.node_type_names())
    }

    /// Returns the declared edge type names.
    pub fn edge_type_names(&self) -> Result<Vec<String>> {
        self.with(|c| c.edge_type_names())
    }

    /// Starts a transaction. Changes are held until `commit_transaction`.
    pub fn begin_transaction(&self) -> Result<()> {
        self.with(|c| c.begin_transaction())
    }

    pub fn commit_transaction(&self) -> Result<()> {
        self.with(|c| c.commit_transaction())
    }

    pub fn rollback_transaction(&self) -> Result<()> {
        self.with(|c| c.rollback_transaction())
    }
}

/// Returns the ChiffonDB version this binding was built against.
///
/// Used by the client as a cheap end-to-end check that the native library
/// loaded and the ABI matches.
#[uniffi::export]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
