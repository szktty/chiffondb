use std::collections::HashSet;
use std::fs::{File, OpenOptions as FsOpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::GraphError;
use crate::storage::buffer::PageCache;
use crate::storage::page::PAGE_SIZE;
use crate::storage::wal::{wal_path, WalFile};

/// Default in-memory page-cache budget for file-backed databases.
///
/// 4 MiB ≈ 1024 pages. Larger than SQLite's ~2 MiB default to suit an embedded
/// graph workload while still keeping the footprint small and bounded.
pub const DEFAULT_MAX_MEMORY_BYTES: usize = 4 * 1024 * 1024;

/// Tunable options for opening or creating a database.
///
/// Shaped as a struct so future settings can be added as fields without breaking
/// callers (cf. sled's `Config`, SQLite's `open_v2`).
#[derive(Debug, Clone)]
pub struct OpenOptions {
    /// Upper bound on the page cache's in-memory footprint, in bytes.
    /// Converted to a page count via `max_memory_bytes / PAGE_SIZE`.
    pub max_memory_bytes: usize,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
        }
    }
}

impl OpenOptions {
    /// Number of pages the cache may hold for the given memory budget (at least 1).
    pub fn cache_capacity(&self) -> usize {
        (self.max_memory_bytes / PAGE_SIZE).max(1)
    }
}

/// Tracks canonical paths of file-backed databases currently open in this process.
/// Supplements the OS-level flock, which is unreliable within a single process on POSIX.
static OPEN_FILES: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

fn register_path(path: &Path) -> Result<(), GraphError> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut guard = OPEN_FILES.lock().unwrap();
    let set = guard.get_or_insert_with(HashSet::new);
    if !set.insert(canonical) {
        return Err(GraphError::Io(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!(
                "database file '{}' is already open in this process",
                path.display()
            ),
        )));
    }
    Ok(())
}

fn unregister_path(path: &Path) {
    if let Ok(mut guard) = OPEN_FILES.lock() {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(set) = guard.as_mut() {
            set.remove(&canonical);
        }
    }
}

pub const MAGIC: &[u8; 8] = b"CHIFFON\0";
/// On-disk format version. v2 added topology page counts at header offsets 40..48
/// (file-backed topology, Phase 2). v3 accompanies the product rename to ChiffonDB,
/// which also changed the magic from `TANABATA` to `CHIFFON\0`; old files are rejected.
/// A mismatch is rejected by `FileHeader::deserialize` to avoid silently misreading
/// an older layout.
pub const VERSION: u32 = 3;
pub const TOPOLOGY_SEGMENT_DEFAULT_START: u32 = 1;
pub const PROPERTY_SEGMENT_DEFAULT_START: u32 = 64;
pub const VECTOR_SEGMENT_DEFAULT_START: u32 = 0;

/// Contents of the header page (Page 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHeader {
    pub version: u32,
    pub topology_segment_start: u32,
    pub property_segment_start: u32,
    pub vector_segment_start: u32,
    pub page_directory_root: u32,
    pub schema_root: u32,
    /// Version number incremented on every schema change.
    pub schema_version: u32,
    /// Number of logical node pages in use within the topology segment.
    /// Persisted so reopen restores the actual used count rather than inferring
    /// the segment-wide maximum from the pre-allocated layout.
    pub node_page_count: u32,
    /// Number of logical edge pages in use within the topology segment.
    pub edge_page_count: u32,
}

impl Default for FileHeader {
    fn default() -> Self {
        Self::new()
    }
}

impl FileHeader {
    pub fn new() -> Self {
        Self {
            version: VERSION,
            topology_segment_start: TOPOLOGY_SEGMENT_DEFAULT_START,
            property_segment_start: PROPERTY_SEGMENT_DEFAULT_START,
            vector_segment_start: VECTOR_SEGMENT_DEFAULT_START,
            page_directory_root: 0,
            schema_root: 0,
            schema_version: 0,
            node_page_count: 1,
            edge_page_count: 1,
        }
    }

    pub fn serialize(&self) -> [u8; PAGE_SIZE] {
        let mut buf = [0u8; PAGE_SIZE];
        buf[0..8].copy_from_slice(MAGIC);
        buf[8..12].copy_from_slice(&self.version.to_le_bytes());
        buf[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        buf[16..20].copy_from_slice(&self.topology_segment_start.to_le_bytes());
        buf[20..24].copy_from_slice(&self.property_segment_start.to_le_bytes());
        buf[24..28].copy_from_slice(&self.vector_segment_start.to_le_bytes());
        buf[28..32].copy_from_slice(&self.page_directory_root.to_le_bytes());
        buf[32..36].copy_from_slice(&self.schema_root.to_le_bytes());
        buf[36..40].copy_from_slice(&self.schema_version.to_le_bytes());
        buf[40..44].copy_from_slice(&self.node_page_count.to_le_bytes());
        buf[44..48].copy_from_slice(&self.edge_page_count.to_le_bytes());
        buf
    }

    pub fn deserialize(buf: &[u8; PAGE_SIZE]) -> Result<Self, GraphError> {
        if &buf[0..8] != MAGIC {
            return Err(GraphError::SchemaError("invalid magic number".to_string()));
        }
        let version = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        // Reject any version other than the current one. The on-disk format is not yet
        // stable (e.g. header offsets 40..48 changed meaning from "reserved" to topology
        // page counts), so an unrecognized version must error rather than be misread.
        if version != VERSION {
            return Err(GraphError::UnsupportedVersion {
                found: version,
                supported: VERSION,
            });
        }
        let topology_segment_start = u32::from_le_bytes(buf[16..20].try_into().unwrap());
        let property_segment_start = u32::from_le_bytes(buf[20..24].try_into().unwrap());
        let vector_segment_start = u32::from_le_bytes(buf[24..28].try_into().unwrap());
        let page_directory_root = u32::from_le_bytes(buf[28..32].try_into().unwrap());
        let schema_root = u32::from_le_bytes(buf[32..36].try_into().unwrap());
        let schema_version = u32::from_le_bytes(buf[36..40].try_into().unwrap());
        // node/edge page counts. `.max(1)` defends against a zeroed value (the topology
        // segment always has at least one logical node and edge page).
        let node_page_count = u32::from_le_bytes(buf[40..44].try_into().unwrap()).max(1);
        let edge_page_count = u32::from_le_bytes(buf[44..48].try_into().unwrap()).max(1);
        Ok(Self {
            version,
            topology_segment_start,
            property_segment_start,
            vector_segment_start,
            page_directory_root,
            schema_root,
            schema_version,
            node_page_count,
            edge_page_count,
        })
    }
}

// ---- Storage backend ----

/// The storage engine. Supports both file and in-memory backends.
enum StorageBackend {
    /// File backend: writes go through the WAL; checkpoint on flush.
    File {
        file: File,
        db_path: PathBuf,
        wal: WalFile,
        /// Bounded in-memory page cache. Reads are served from here when hot,
        /// keeping the resident footprint independent of database size.
        cache: PageCache,
        /// Exclusive advisory lock held for the lifetime of this DatabaseFile.
        /// Prevents two processes from opening the same .chiffon simultaneously.
        _lock: File,
    },
    /// Memory backend: writes directly into a Vec<u8>. No WAL or flush needed.
    Memory { data: Vec<u8> },
}

impl Drop for StorageBackend {
    fn drop(&mut self) {
        if let StorageBackend::File { db_path, .. } = self {
            unregister_path(db_path);
        }
    }
}

impl StorageBackend {
    fn read_page(&mut self, page_id: u32) -> Result<[u8; PAGE_SIZE], GraphError> {
        match self {
            StorageBackend::File {
                file, wal, cache, ..
            } => {
                // Read priority: bounded cache → WAL (durable, uncheckpointed) → main file.
                if let Some(cached) = cache.get(page_id) {
                    return Ok(cached);
                }
                let data = if let Some(from_wal) = wal.read(page_id)? {
                    from_wal
                } else {
                    let offset = page_id as u64 * PAGE_SIZE as u64;
                    file.seek(SeekFrom::Start(offset))?;
                    let mut buf = [0u8; PAGE_SIZE];
                    file.read_exact(&mut buf)?;
                    buf
                };
                // The loaded page matches its durable copy, so cache it clean. A clean
                // insert can only evict another clean page, so there is never a victim
                // to persist; we drop the (always-None) return explicitly.
                let victim = cache.put(page_id, data, false);
                debug_assert!(victim.is_none());
                Ok(data)
            }
            StorageBackend::Memory { data } => {
                let offset = page_id as usize * PAGE_SIZE;
                if offset + PAGE_SIZE > data.len() {
                    return Err(GraphError::StorageCorrupted(page_id));
                }
                let mut buf = [0u8; PAGE_SIZE];
                buf.copy_from_slice(&data[offset..offset + PAGE_SIZE]);
                Ok(buf)
            }
        }
    }

    fn write_page(&mut self, page_id: u32, page: &[u8; PAGE_SIZE]) -> Result<(), GraphError> {
        match self {
            StorageBackend::File { wal, cache, .. } => {
                // Write-through to the WAL keeps every write durable immediately, so the
                // cached copy is always backed by the WAL file and can be evicted freely.
                wal.write(page_id, page)?;
                let victim = cache.put(page_id, *page, false);
                debug_assert!(victim.is_none());
                Ok(())
            }
            StorageBackend::Memory { data } => {
                let offset = page_id as usize * PAGE_SIZE;
                if offset + PAGE_SIZE > data.len() {
                    return Err(GraphError::StorageCorrupted(page_id));
                }
                data[offset..offset + PAGE_SIZE].copy_from_slice(page);
                Ok(())
            }
        }
    }

    fn append_page(
        &mut self,
        page: &[u8; PAGE_SIZE],
        logical_page_count: u32,
    ) -> Result<(), GraphError> {
        match self {
            StorageBackend::File { wal, cache, .. } => {
                wal.write(logical_page_count, page)?;
                let victim = cache.put(logical_page_count, *page, false);
                debug_assert!(victim.is_none());
                Ok(())
            }
            StorageBackend::Memory { data } => {
                data.extend_from_slice(page);
                Ok(())
            }
        }
    }

    fn flush(&mut self, logical_page_count: u32) -> Result<(), GraphError> {
        match self {
            StorageBackend::File { file, wal, .. } => {
                if wal.is_empty() {
                    return Ok(());
                }
                // Pre-allocate file space for pages added via append_page.
                let file_len = file.seek(SeekFrom::End(0))?;
                let file_pages = (file_len / PAGE_SIZE as u64) as u32;
                if logical_page_count > file_pages {
                    let empty = [0u8; PAGE_SIZE];
                    for _ in file_pages..logical_page_count {
                        file.seek(SeekFrom::End(0))?;
                        file.write_all(&empty)?;
                    }
                }
                wal.checkpoint(file)
            }
            StorageBackend::Memory { .. } => Ok(()), // Memory is always up-to-date.
        }
    }

    fn wal_is_empty(&self) -> bool {
        match self {
            StorageBackend::File { wal, .. } => wal.is_empty(),
            StorageBackend::Memory { .. } => true,
        }
    }
}

// ---- DatabaseFile ----

/// Snapshot of the mutable storage state used for transaction rollback.
pub struct FileSnapshot {
    /// WAL file length at snapshot time. Rollback truncates the WAL back to this
    /// offset, discarding every entry appended during the transaction.
    pub(crate) wal_len: u64,
    pub(crate) logical_page_count: u32,
    /// In-memory header at snapshot time. `restore` reverts the header *in full* on
    /// rollback, so the whole header is treated as transaction state and reverts in
    /// lockstep with the storage — keeping the header the single in-memory source of
    /// truth (e.g. topology page counts, schema_root/schema_version).
    ///
    /// Discipline: any field added to `FileHeader` must be safe to revert on rollback.
    /// Do not add fields that should survive a rollback (cumulative stats, last-access
    /// time, a global generation counter, etc.) without reworking this to revert
    /// selectively.
    pub(crate) header: FileHeader,
    pub(crate) memory_data: Option<Vec<u8>>,
}

/// Storage layer for a single database. Supports both file and in-memory backends.
pub struct DatabaseFile {
    backend: StorageBackend,
    pub header: FileHeader,
    /// Total logical page count, including pages pending in the WAL.
    logical_page_count: u32,
}

impl DatabaseFile {
    /// Creates a new DB file with default options. Returns an error if it already exists.
    pub fn create(path: &Path) -> Result<Self, GraphError> {
        Self::create_with_options(path, &OpenOptions::default())
    }

    /// Creates a new DB file with the given options.
    pub fn create_with_options(path: &Path, opts: &OpenOptions) -> Result<Self, GraphError> {
        register_path(path)?;
        match Self::create_inner(path, opts) {
            Ok(db) => Ok(db),
            Err(e) => {
                unregister_path(path);
                Err(e)
            }
        }
    }

    fn create_inner(path: &Path, opts: &OpenOptions) -> Result<Self, GraphError> {
        // Acquire lock before creating the .chiffon file so a failed lock leaves no empty file.
        let lock = acquire_lock(path)?;
        let mut file = FsOpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;

        let header = FileHeader::new();
        let header_bytes = header.serialize();
        file.write_all(&header_bytes)?;
        file.sync_all()?;

        let wp = wal_path(path);
        let wal = WalFile::open(&wp)?;

        Ok(Self {
            backend: StorageBackend::File {
                file,
                db_path: path.to_path_buf(),
                wal,
                cache: PageCache::new(opts.cache_capacity()),
                _lock: lock,
            },
            header,
            logical_page_count: 1,
        })
    }

    /// Opens an existing DB file with default options. Runs recovery if a WAL file is present.
    pub fn open(path: &Path) -> Result<Self, GraphError> {
        Self::open_with_options(path, &OpenOptions::default())
    }

    /// Opens an existing DB file with the given options.
    pub fn open_with_options(path: &Path, opts: &OpenOptions) -> Result<Self, GraphError> {
        register_path(path)?;
        match Self::open_inner(path, opts) {
            Ok(db) => Ok(db),
            Err(e) => {
                unregister_path(path);
                Err(e)
            }
        }
    }

    fn open_inner(path: &Path, opts: &OpenOptions) -> Result<Self, GraphError> {
        // Acquire an exclusive lock before reading, to prevent two processes from
        // opening the same file simultaneously.
        let lock = acquire_lock(path)?;

        let mut file = FsOpenOptions::new().read(true).write(true).open(path)?;
        let mut buf = [0u8; PAGE_SIZE];
        file.read_exact(&mut buf)?;
        let header = FileHeader::deserialize(&buf)?;

        let wp = wal_path(path);
        let mut wal = WalFile::open(&wp)?;

        if !wal.is_empty() {
            wal.checkpoint(&mut file)?;
        }

        let file_len = file.seek(SeekFrom::End(0))?;
        let logical_page_count = (file_len / PAGE_SIZE as u64) as u32;

        Ok(Self {
            backend: StorageBackend::File {
                file,
                cache: PageCache::new(opts.cache_capacity()),
                db_path: path.to_path_buf(),
                wal,
                _lock: lock,
            },
            header,
            logical_page_count,
        })
    }

    /// Creates an in-memory database that operates entirely without a file.
    pub fn create_in_memory() -> Result<Self, GraphError> {
        let header = FileHeader::new();
        let header_bytes = header.serialize();

        let mut data = Vec::with_capacity(PAGE_SIZE * 2);
        data.extend_from_slice(&header_bytes);

        Ok(Self {
            backend: StorageBackend::Memory { data },
            header,
            logical_page_count: 1,
        })
    }

    /// Reads the specified page.
    pub fn read_page(&mut self, page_id: u32) -> Result<[u8; PAGE_SIZE], GraphError> {
        self.backend.read_page(page_id)
    }

    /// Writes data to the specified page.
    pub fn write_page(&mut self, page_id: u32, data: &[u8; PAGE_SIZE]) -> Result<(), GraphError> {
        self.backend.write_page(page_id, data)
    }

    /// Appends a new page to the end of the file and returns its page ID.
    pub fn append_page(&mut self, data: &[u8; PAGE_SIZE]) -> Result<u32, GraphError> {
        let page_id = self.logical_page_count;
        self.backend.append_page(data, page_id)?;
        self.logical_page_count += 1;
        Ok(page_id)
    }

    /// Returns the total logical page count.
    pub fn page_count(&mut self) -> Result<u32, GraphError> {
        Ok(self.logical_page_count)
    }

    /// Checkpoints the WAL and syncs to disk (file backend only).
    pub fn flush(&mut self) -> Result<(), GraphError> {
        self.backend.flush(self.logical_page_count)
    }

    /// Writes the header back to page 0.
    pub fn write_header(&mut self) -> Result<(), GraphError> {
        let buf = self.header.serialize();
        self.write_page(0, &buf)
    }

    /// Returns the WAL file path (file backend only; intended for tests).
    pub fn wal_path(&self) -> Option<PathBuf> {
        match &self.backend {
            StorageBackend::File { db_path, .. } => Some(wal_path(db_path)),
            StorageBackend::Memory { .. } => None,
        }
    }

    /// Returns true if the WAL is empty (intended for tests).
    pub fn wal_is_empty(&self) -> bool {
        self.backend.wal_is_empty()
    }

    /// Returns true if this database uses the in-memory backend.
    pub fn is_in_memory(&self) -> bool {
        matches!(self.backend, StorageBackend::Memory { .. })
    }

    /// Returns `(resident_pages, capacity)` for the file backend's page cache.
    /// Returns `None` for the memory backend (which holds everything resident).
    /// Intended for diagnostics and memory-footprint benchmarks.
    pub fn cache_stats(&self) -> Option<(usize, usize)> {
        match &self.backend {
            StorageBackend::File { cache, .. } => Some((cache.len(), cache.capacity())),
            StorageBackend::Memory { .. } => None,
        }
    }

    /// Takes a snapshot of the mutable storage state for transaction rollback.
    pub fn snapshot(&self) -> FileSnapshot {
        match &self.backend {
            StorageBackend::File { wal, .. } => FileSnapshot {
                wal_len: wal.current_len(),
                logical_page_count: self.logical_page_count,
                header: self.header.clone(),
                memory_data: None,
            },
            StorageBackend::Memory { data } => FileSnapshot {
                wal_len: 0,
                logical_page_count: self.logical_page_count,
                header: self.header.clone(),
                memory_data: Some(data.clone()),
            },
        }
    }

    /// Restores mutable storage state from a snapshot (discards writes since the snapshot).
    /// For the file backend, truncates the WAL file back to its snapshot length so that
    /// rolled-back entries are neither read nor replayed on a subsequent crash recovery,
    /// and invalidates the page cache (which may hold post-snapshot writes).
    pub fn restore(&mut self, snap: FileSnapshot) {
        self.logical_page_count = snap.logical_page_count;
        self.header = snap.header;
        match &mut self.backend {
            StorageBackend::File { wal, cache, .. } => {
                // Drop cached pages first: any of them may reflect a write made after
                // the snapshot, so reads must fall back to the truncated WAL / main file.
                cache.clear();
                // Best-effort: if the truncate fails we still have the correct logical
                // state, so only the crash-recovery path could surface stale entries.
                let _ = wal.truncate_to(snap.wal_len);
            }
            StorageBackend::Memory { data } => {
                if let Some(saved) = snap.memory_data {
                    *data = saved;
                }
            }
        }
    }
}

/// Returns the lock-file path for a database path (e.g. `foo.chiffon` → `foo.chiffon-lock`).
fn lock_path(db_path: &Path) -> PathBuf {
    let mut p = db_path.to_path_buf();
    let name = p
        .file_name()
        .map(|n| format!("{}-lock", n.to_string_lossy()))
        .unwrap_or_else(|| "db-lock".to_string());
    p.set_file_name(name);
    p
}

/// Opens (or creates) the lock file and acquires an exclusive non-blocking lock.
/// Returns `Err` if another process already holds the lock.
fn acquire_lock(db_path: &Path) -> Result<File, GraphError> {
    let lp = lock_path(db_path);
    let f = FsOpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lp)?;
    f.try_lock().map_err(|e| {
        let io_err: std::io::Error = e.into();
        if io_err.kind() == std::io::ErrorKind::WouldBlock {
            GraphError::Io(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!(
                    "database file '{}' is already open by another process",
                    db_path.display()
                ),
            ))
        } else {
            GraphError::Io(io_err)
        }
    })?;
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_db_path(dir: &TempDir) -> PathBuf {
        dir.path().join("test.chiffon")
    }

    #[test]
    fn reopen_after_drop_succeeds() {
        let dir = TempDir::new().unwrap();
        let path = make_db_path(&dir);
        {
            let _db = DatabaseFile::create(&path).unwrap();
        }
        // After drop, the same process must be able to reopen the file.
        let db2 = DatabaseFile::open(&path);
        assert!(
            db2.is_ok(),
            "reopen after drop should succeed: {:?}",
            db2.err()
        );
    }

    #[test]
    fn open_while_open_same_process_fails() {
        let dir = TempDir::new().unwrap();
        let path = make_db_path(&dir);
        let _db1 = DatabaseFile::create(&path).unwrap();
        let result = DatabaseFile::open(&path);
        assert!(result.is_err(), "opening an already-open file should fail");
    }

    #[test]
    fn create_and_reopen_reads_correct_header() {
        let dir = TempDir::new().unwrap();
        let path = make_db_path(&dir);

        {
            let db = DatabaseFile::create(&path).unwrap();
            assert_eq!(db.header.version, VERSION);
        }

        let db = DatabaseFile::open(&path).unwrap();
        assert_eq!(db.header.version, VERSION);
        assert_eq!(
            db.header.topology_segment_start,
            TOPOLOGY_SEGMENT_DEFAULT_START
        );
        assert_eq!(
            db.header.property_segment_start,
            PROPERTY_SEGMENT_DEFAULT_START
        );
    }

    #[test]
    fn open_invalid_magic_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = make_db_path(&dir);

        {
            let mut f = FsOpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            let mut buf = [0u8; PAGE_SIZE];
            buf[0..8].copy_from_slice(b"INVALID!");
            f.write_all(&buf).unwrap();
        }

        assert!(DatabaseFile::open(&path).is_err());
    }

    #[test]
    fn open_unsupported_version_returns_error() {
        let dir = TempDir::new().unwrap();
        let path = make_db_path(&dir);

        // Valid magic but a version we do not support (current VERSION + 1).
        {
            let mut f = FsOpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            let mut buf = [0u8; PAGE_SIZE];
            buf[0..8].copy_from_slice(MAGIC);
            buf[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
            f.write_all(&buf).unwrap();
        }

        match DatabaseFile::open(&path) {
            Err(GraphError::UnsupportedVersion { found, supported }) => {
                assert_eq!(found, VERSION + 1);
                assert_eq!(supported, VERSION);
            }
            Err(e) => panic!("expected UnsupportedVersion, got error {e:?}"),
            Ok(_) => panic!("expected UnsupportedVersion, but open succeeded"),
        }
    }

    #[test]
    fn open_legacy_v1_returns_error_without_misreading() {
        // The core purpose of the version check: a pre-Phase-2 v1 file (offsets 40..48
        // meant "reserved", not topology page counts) must be rejected rather than
        // silently misread under the v2 layout. Magic matches and the segment fields look
        // plausible, so only the version guard stands between this file and a misread.
        let dir = TempDir::new().unwrap();
        let path = make_db_path(&dir);
        {
            let mut f = FsOpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            let mut buf = [0u8; PAGE_SIZE];
            buf[0..8].copy_from_slice(MAGIC);
            buf[8..12].copy_from_slice(&1u32.to_le_bytes()); // legacy v1
            buf[16..20].copy_from_slice(&TOPOLOGY_SEGMENT_DEFAULT_START.to_le_bytes());
            buf[20..24].copy_from_slice(&PROPERTY_SEGMENT_DEFAULT_START.to_le_bytes());
            f.write_all(&buf).unwrap();
        }

        match DatabaseFile::open(&path) {
            Err(GraphError::UnsupportedVersion { found, supported }) => {
                assert_eq!(found, 1);
                assert_eq!(supported, VERSION);
            }
            Err(e) => panic!("expected UnsupportedVersion for v1, got error {e:?}"),
            Ok(_) => panic!("v1 file was not rejected — silent misread risk"),
        }
    }

    #[test]
    fn write_and_read_page() {
        let dir = TempDir::new().unwrap();
        let path = make_db_path(&dir);

        let mut db = DatabaseFile::create(&path).unwrap();
        let mut data = [0u8; PAGE_SIZE];
        data[0] = 0xDE;
        data[PAGE_SIZE - 1] = 0xAD;
        let pid = db.append_page(&data).unwrap();

        let read_back = db.read_page(pid).unwrap();
        assert_eq!(read_back[0], 0xDE);
        assert_eq!(read_back[PAGE_SIZE - 1], 0xAD);
    }

    #[test]
    fn page_count_correct_after_appends() {
        let dir = TempDir::new().unwrap();
        let path = make_db_path(&dir);

        let mut db = DatabaseFile::create(&path).unwrap();
        let initial = db.page_count().unwrap();
        let empty = [0u8; PAGE_SIZE];
        db.append_page(&empty).unwrap();
        db.append_page(&empty).unwrap();
        assert_eq!(db.page_count().unwrap(), initial + 2);
    }

    #[test]
    fn flush_checkpoints_wal_to_db() {
        let dir = TempDir::new().unwrap();
        let path = make_db_path(&dir);

        let pid;
        {
            let mut db = DatabaseFile::create(&path).unwrap();
            let mut data = [0u8; PAGE_SIZE];
            data[0] = 0x99;
            pid = db.append_page(&data).unwrap();
            db.flush().unwrap();
            assert!(db.wal_is_empty());
            // db dropped here, releasing the lock
        }

        let mut db2 = DatabaseFile::open(&path).unwrap();
        let read_back = db2.read_page(pid).unwrap();
        assert_eq!(read_back[0], 0x99);
    }

    #[test]
    fn crash_recovery_via_wal() {
        let dir = TempDir::new().unwrap();
        let path = make_db_path(&dir);

        let pid;
        {
            let mut db = DatabaseFile::create(&path).unwrap();
            let mut data = [0u8; PAGE_SIZE];
            data[0] = 0x77;
            pid = db.append_page(&data).unwrap();
            // Do not flush
        }

        let wp = wal_path(&path);
        assert!(wp.exists());

        let mut db2 = DatabaseFile::open(&path).unwrap();
        let read_back = db2.read_page(pid).unwrap();
        assert_eq!(read_back[0], 0x77);
        assert!(db2.wal_is_empty());
    }

    // ---- In-memory backend tests ----

    #[test]
    fn in_memory_write_and_read() {
        let mut db = DatabaseFile::create_in_memory().unwrap();
        assert!(db.is_in_memory());

        let mut data = [0u8; PAGE_SIZE];
        data[0] = 0xAB;
        let pid = db.append_page(&data).unwrap();

        let read_back = db.read_page(pid).unwrap();
        assert_eq!(read_back[0], 0xAB);
    }

    #[test]
    fn in_memory_header_roundtrip() {
        let mut db = DatabaseFile::create_in_memory().unwrap();
        db.header.schema_root = 42;
        db.write_header().unwrap();

        // Re-read the header and verify.
        let buf = db.read_page(0).unwrap();
        let header = FileHeader::deserialize(&buf).unwrap();
        assert_eq!(header.schema_root, 42);
    }

    #[test]
    fn in_memory_page_count() {
        let mut db = DatabaseFile::create_in_memory().unwrap();
        assert_eq!(db.page_count().unwrap(), 1);
        let empty = [0u8; PAGE_SIZE];
        db.append_page(&empty).unwrap();
        db.append_page(&empty).unwrap();
        assert_eq!(db.page_count().unwrap(), 3);
    }

    #[test]
    fn in_memory_flush_is_noop() {
        let mut db = DatabaseFile::create_in_memory().unwrap();
        let empty = [0u8; PAGE_SIZE];
        db.append_page(&empty).unwrap();
        // flush should succeed without error (no-op for memory backend).
        db.flush().unwrap();
    }

    // ---- Bounded page cache ----

    // A 1-page cache forces an eviction on essentially every distinct read/write,
    // so these tests exercise the read fall-through (cache → WAL → main file).

    fn tiny_cache_opts() -> OpenOptions {
        // 1 page worth of memory → capacity 1.
        OpenOptions {
            max_memory_bytes: PAGE_SIZE,
        }
    }

    fn distinct_page(byte: u8) -> [u8; PAGE_SIZE] {
        let mut p = [0u8; PAGE_SIZE];
        p[0] = byte;
        p[PAGE_SIZE - 1] = byte;
        p
    }

    #[test]
    fn cache_capacity_from_memory_budget() {
        let opts = OpenOptions {
            max_memory_bytes: PAGE_SIZE * 10,
        };
        assert_eq!(opts.cache_capacity(), 10);
        // Always at least one page, even for a tiny budget.
        assert_eq!(
            OpenOptions {
                max_memory_bytes: 0
            }
            .cache_capacity(),
            1
        );
    }

    #[test]
    fn reads_correct_after_eviction_from_wal() {
        // With a 1-page cache, writing many pages then reading them back must still
        // return correct data: evicted pages are served from the WAL file.
        let dir = TempDir::new().unwrap();
        let path = make_db_path(&dir);
        let mut db = DatabaseFile::create_with_options(&path, &tiny_cache_opts()).unwrap();

        let mut pids = Vec::new();
        for i in 0..32u8 {
            pids.push(db.append_page(&distinct_page(i)).unwrap());
        }
        // Not flushed: pages live in the WAL, not the main file.
        for (i, &pid) in pids.iter().enumerate() {
            let p = db.read_page(pid).unwrap();
            assert_eq!(p[0], i as u8);
            assert_eq!(p[PAGE_SIZE - 1], i as u8);
        }
    }

    #[test]
    fn reads_correct_after_eviction_from_main_file() {
        // After a checkpoint the pages are in the main file; a tiny cache must still
        // serve them correctly via the main-file fall-through.
        let dir = TempDir::new().unwrap();
        let path = make_db_path(&dir);
        let mut pids = Vec::new();
        {
            let mut db = DatabaseFile::create_with_options(&path, &tiny_cache_opts()).unwrap();
            for i in 0..32u8 {
                pids.push(db.append_page(&distinct_page(i)).unwrap());
            }
            db.flush().unwrap();
            assert!(db.wal_is_empty());
            for (i, &pid) in pids.iter().enumerate() {
                assert_eq!(db.read_page(pid).unwrap()[0], i as u8);
            }
        }
        // Reopen with a tiny cache and verify durability + fall-through still hold.
        let mut db2 = DatabaseFile::open_with_options(&path, &tiny_cache_opts()).unwrap();
        for (i, &pid) in pids.iter().enumerate() {
            assert_eq!(db2.read_page(pid).unwrap()[PAGE_SIZE - 1], i as u8);
        }
    }

    #[test]
    fn overwrite_then_read_is_latest_under_tiny_cache() {
        let dir = TempDir::new().unwrap();
        let path = make_db_path(&dir);
        let mut db = DatabaseFile::create_with_options(&path, &tiny_cache_opts()).unwrap();

        let pid = db.append_page(&distinct_page(1)).unwrap();
        // Evict pid by touching another page, then overwrite pid.
        let _other = db.append_page(&distinct_page(2)).unwrap();
        db.write_page(pid, &distinct_page(9)).unwrap();
        // Evict again, then read: must see the latest value 9.
        let _other2 = db.append_page(&distinct_page(3)).unwrap();
        assert_eq!(db.read_page(pid).unwrap()[0], 9);
    }

    #[test]
    fn cache_is_populated_and_bounded() {
        // Guards against the cache silently doing nothing: after writing and reading
        // many pages, occupancy must be non-zero and capped at capacity.
        let dir = TempDir::new().unwrap();
        let path = make_db_path(&dir);
        let mut db = DatabaseFile::create_with_options(&path, &tiny_cache_opts()).unwrap();
        let (_, cap) = db.cache_stats().unwrap();
        assert_eq!(cap, 1);

        let mut pids = Vec::new();
        for i in 0..16u8 {
            pids.push(db.append_page(&distinct_page(i)).unwrap());
        }
        for &pid in &pids {
            let _ = db.read_page(pid).unwrap();
        }
        let (resident, cap) = db.cache_stats().unwrap();
        assert!(resident >= 1, "cache must hold at least the last read page");
        assert!(resident <= cap, "resident pages must not exceed capacity");
    }

    #[test]
    fn rollback_via_wal_truncate_discards_writes() {
        // snapshot() + restore() must discard post-snapshot writes even when those
        // writes have been evicted from the page cache.
        let dir = TempDir::new().unwrap();
        let path = make_db_path(&dir);
        let mut db = DatabaseFile::create_with_options(&path, &tiny_cache_opts()).unwrap();

        let pid = db.append_page(&distinct_page(1)).unwrap();
        db.flush().unwrap(); // page 1 is now durable in the main file

        let snap = db.snapshot();
        // Make several writes (overflowing the 1-page cache) after the snapshot.
        db.write_page(pid, &distinct_page(7)).unwrap();
        let extra = db.append_page(&distinct_page(8)).unwrap();
        assert_eq!(db.read_page(pid).unwrap()[0], 7);

        db.restore(snap);
        // pid reverts to its pre-snapshot value, and the appended page is gone from the WAL.
        assert_eq!(db.read_page(pid).unwrap()[0], 1);
        assert!(!db.backend_wal_contains(extra));
    }

    // Test-only helper to assert WAL membership after rollback.
    impl DatabaseFile {
        fn backend_wal_contains(&self, page_id: u32) -> bool {
            match &self.backend {
                StorageBackend::File { wal, .. } => wal.contains(page_id),
                StorageBackend::Memory { .. } => false,
            }
        }
    }
}
