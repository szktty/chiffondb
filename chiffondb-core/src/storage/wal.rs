/// WAL (Write-Ahead Log) implementation.
///
/// Design:
/// - Writes are appended to .chiffon-wal instead of being written directly to .chiffon.
/// - The WAL keeps only a small index (page_id → byte offset of the latest entry)
///   in memory; the page data itself lives in the WAL file. The hot page data is
///   cached separately by the bounded `PageCache`, so the WAL never holds an
///   unbounded amount of page bytes in memory.
/// - On read, the WAL returns the latest entry for a page_id by seeking to its
///   offset in the file. It takes priority over the main file.
/// - On checkpoint (flush), WAL pages are written back to the main file, sync_all
///   is called, and the WAL is cleared.
/// - On open, if a WAL file exists, recovery is performed (the index is rebuilt).
///
/// WAL entry format (fixed 4104 bytes):
///   [0..4]    page_id: u32 LE
///   [4..8]    checksum: u32 LE (simple additive checksum over page_id + data)
///   [8..4104] data: [u8; PAGE_SIZE]
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::GraphError;
use crate::storage::page::PAGE_SIZE;

pub const WAL_ENTRY_SIZE: usize = 4 + 4 + PAGE_SIZE; // page_id + checksum + data

/// Computes the WAL entry checksum (simple additive sum).
fn checksum(page_id: u32, data: &[u8; PAGE_SIZE]) -> u32 {
    let mut sum: u32 = page_id;
    for &b in data.iter() {
        sum = sum.wrapping_add(b as u32);
    }
    sum
}

/// Manages the WAL file (.chiffon-wal).
///
/// Only an index (`page_id → byte offset`) is kept in memory; page data is read
/// from the file on demand. This keeps WAL memory bounded by the number of
/// distinct dirty pages rather than by their total byte size.
pub struct WalFile {
    file: File,
    path: PathBuf,
    /// page_id → byte offset of the latest entry for that page in the WAL file.
    offsets: HashMap<u32, u64>,
    /// Total bytes currently in the WAL file (offset for the next append).
    len: u64,
}

impl WalFile {
    /// Opens the WAL file (creates it if it does not exist). Rebuilds the index from existing entries.
    pub fn open(wal_path: &Path) -> Result<Self, GraphError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(wal_path)?;
        let mut wal = Self {
            file,
            path: wal_path.to_path_buf(),
            offsets: HashMap::new(),
            len: 0,
        };
        wal.load_index()?;
        Ok(wal)
    }

    /// Returns true if the WAL contains an entry for the given page_id.
    pub fn contains(&self, page_id: u32) -> bool {
        self.offsets.contains_key(&page_id)
    }

    /// Reads the latest data for the given page_id from the WAL file.
    pub fn read(&mut self, page_id: u32) -> Result<Option<[u8; PAGE_SIZE]>, GraphError> {
        let Some(&offset) = self.offsets.get(&page_id) else {
            return Ok(None);
        };
        self.file.seek(SeekFrom::Start(offset))?;
        let mut entry = [0u8; WAL_ENTRY_SIZE];
        self.file.read_exact(&mut entry)?;
        let data: [u8; PAGE_SIZE] = entry[8..WAL_ENTRY_SIZE]
            .try_into()
            .map_err(|_| GraphError::StorageCorrupted(page_id))?;
        Ok(Some(data))
    }

    /// Appends a page entry to the WAL file and records its offset.
    pub fn write(&mut self, page_id: u32, data: &[u8; PAGE_SIZE]) -> Result<(), GraphError> {
        let cs = checksum(page_id, data);
        let mut entry = [0u8; WAL_ENTRY_SIZE];
        entry[0..4].copy_from_slice(&page_id.to_le_bytes());
        entry[4..8].copy_from_slice(&cs.to_le_bytes());
        entry[8..WAL_ENTRY_SIZE].copy_from_slice(data);

        let offset = self.len;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&entry)?;
        self.offsets.insert(page_id, offset);
        self.len += WAL_ENTRY_SIZE as u64;
        Ok(())
    }

    /// Returns true if the WAL has no entries.
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Returns the page_ids currently recorded in the WAL.
    pub fn page_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.offsets.keys().copied()
    }

    /// Returns the current byte length of the WAL file (used for rollback snapshots).
    pub fn current_len(&self) -> u64 {
        self.len
    }

    /// Writes all pending WAL pages back to the main file, syncs, and clears the WAL.
    pub fn checkpoint(&mut self, db_file: &mut File) -> Result<(), GraphError> {
        // Collect offsets first to avoid borrowing self while reading from the file.
        let entries: Vec<(u32, u64)> = self.offsets.iter().map(|(&p, &o)| (p, o)).collect();
        for (page_id, offset) in entries {
            self.file.seek(SeekFrom::Start(offset))?;
            let mut entry = [0u8; WAL_ENTRY_SIZE];
            self.file.read_exact(&mut entry)?;
            let data = &entry[8..WAL_ENTRY_SIZE];
            let db_offset = page_id as u64 * PAGE_SIZE as u64;
            db_file.seek(SeekFrom::Start(db_offset))?;
            db_file.write_all(data)?;
        }
        // Sync the main file to physical disk
        db_file.sync_all()?;

        // Clear the WAL file by truncating it
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.sync_all()?;
        self.offsets.clear();
        self.len = 0;
        Ok(())
    }

    /// Truncates the WAL file back to `target_len` bytes and rebuilds the index from
    /// the remaining entries. Called on rollback to discard entries written after a
    /// snapshot was taken.
    ///
    /// This works because the WAL is append-only: every entry written after the
    /// snapshot lies beyond `target_len`, and rebuilding the index from the prefix
    /// restores the page→offset map (and thus the latest pre-snapshot value of each
    /// page) exactly.
    pub fn truncate_to(&mut self, target_len: u64) -> Result<(), GraphError> {
        self.file.set_len(target_len)?;
        self.file.sync_all()?;
        self.offsets.clear();
        self.len = 0;
        self.load_index()
    }

    /// Deletes the WAL file (call after a completed checkpoint if it is no longer needed).
    pub fn remove(self) -> Result<(), GraphError> {
        drop(self.file);
        std::fs::remove_file(&self.path)?;
        Ok(())
    }

    /// Rebuilds the index from the file (called on open and during recovery/rollback).
    /// Entries with an invalid checksum stop the scan (treated as a partial trailing write).
    fn load_index(&mut self) -> Result<(), GraphError> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut entry = [0u8; WAL_ENTRY_SIZE];
        let mut offset: u64 = 0;
        loop {
            match self.file.read_exact(&mut entry) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(GraphError::Io(e)),
            }
            let page_id = u32::from_le_bytes(entry[0..4].try_into().unwrap());
            let stored_cs = u32::from_le_bytes(entry[4..8].try_into().unwrap());
            let data: [u8; PAGE_SIZE] = entry[8..WAL_ENTRY_SIZE].try_into().unwrap();
            let expected_cs = checksum(page_id, &data);
            if stored_cs != expected_cs {
                // A partial trailing write corrupts everything after it; stop here.
                break;
            }
            self.offsets.insert(page_id, offset);
            offset += WAL_ENTRY_SIZE as u64;
        }
        self.len = offset;
        Ok(())
    }
}

/// Derives the WAL path from the database path (e.g. `foo.chiffon` → `foo.chiffon-wal`).
pub fn wal_path(db_path: &Path) -> PathBuf {
    let mut p = db_path.to_path_buf();
    let name = p
        .file_name()
        .map(|n| format!("{}-wal", n.to_string_lossy()))
        .unwrap_or_else(|| "db-wal".to_string());
    p.set_file_name(name);
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_wal(dir: &TempDir) -> WalFile {
        let path = dir.path().join("test.chiffon-wal");
        WalFile::open(&path).unwrap()
    }

    #[test]
    fn write_and_read_back() {
        let dir = TempDir::new().unwrap();
        let mut wal = make_wal(&dir);

        let mut data = [0u8; PAGE_SIZE];
        data[0] = 0xAB;
        data[4095] = 0xCD;

        wal.write(42, &data).unwrap();
        assert!(wal.contains(42));
        let read = wal.read(42).unwrap().unwrap();
        assert_eq!(read[0], 0xAB);
        assert_eq!(read[4095], 0xCD);
    }

    #[test]
    fn latest_write_wins() {
        let dir = TempDir::new().unwrap();
        let mut wal = make_wal(&dir);

        let mut data1 = [0u8; PAGE_SIZE];
        data1[0] = 1;
        let mut data2 = [0u8; PAGE_SIZE];
        data2[0] = 2;

        wal.write(0, &data1).unwrap();
        wal.write(0, &data2).unwrap();
        assert_eq!(wal.read(0).unwrap().unwrap()[0], 2);
    }

    #[test]
    fn persist_and_reopen_recovers_entries() {
        let dir = TempDir::new().unwrap();
        let wal_path = dir.path().join("test.chiffon-wal");

        let mut data = [0u8; PAGE_SIZE];
        data[10] = 0xFF;
        {
            let mut wal = WalFile::open(&wal_path).unwrap();
            wal.write(5, &data).unwrap();
        }

        // Verify entries are restored after reopen
        let mut wal2 = WalFile::open(&wal_path).unwrap();
        assert!(wal2.contains(5));
        assert_eq!(wal2.read(5).unwrap().unwrap()[10], 0xFF);
    }

    #[test]
    fn corrupted_entry_is_skipped() {
        let dir = TempDir::new().unwrap();
        let wal_path = dir.path().join("test.chiffon-wal");

        // Write an entry with a corrupted checksum directly
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&wal_path)
                .unwrap();
            let mut entry = [0u8; WAL_ENTRY_SIZE];
            entry[0..4].copy_from_slice(&7u32.to_le_bytes()); // page_id = 7
            entry[4..8].copy_from_slice(&0xDEADBEEFu32.to_le_bytes()); // invalid checksum
            f.write_all(&entry).unwrap();
        }

        let wal = WalFile::open(&wal_path).unwrap();
        // Entry is skipped because the checksum is invalid
        assert!(!wal.contains(7));
    }

    #[test]
    fn checkpoint_writes_to_db_and_clears_wal() {
        let dir = TempDir::new().unwrap();
        let wal_path = dir.path().join("test.chiffon-wal");
        let db_path = dir.path().join("test.chiffon");

        // Create the DB file (2 pages)
        {
            let mut f = File::create(&db_path).unwrap();
            f.write_all(&[0u8; PAGE_SIZE * 2]).unwrap();
        }

        let mut data = [0u8; PAGE_SIZE];
        data[0] = 0x42;

        let mut wal = WalFile::open(&wal_path).unwrap();
        wal.write(1, &data).unwrap();

        let mut db_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&db_path)
            .unwrap();
        wal.checkpoint(&mut db_file).unwrap();

        // WAL should now be empty
        assert!(wal.is_empty());

        // Data should be written back to the DB file
        let mut buf = [0u8; PAGE_SIZE];
        db_file.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        db_file.read_exact(&mut buf).unwrap();
        assert_eq!(buf[0], 0x42);
    }

    #[test]
    fn truncate_to_discards_later_entries() {
        let dir = TempDir::new().unwrap();
        let mut wal = make_wal(&dir);

        let mut a = [0u8; PAGE_SIZE];
        a[0] = 1;
        wal.write(0, &a).unwrap();
        let snapshot_len = wal.current_len();

        let mut b = [0u8; PAGE_SIZE];
        b[0] = 2;
        wal.write(1, &b).unwrap();
        let mut c = [0u8; PAGE_SIZE];
        c[0] = 3;
        wal.write(0, &c).unwrap(); // overwrite page 0

        wal.truncate_to(snapshot_len).unwrap();
        // Page 1 (written after the snapshot) is gone.
        assert!(!wal.contains(1));
        // Page 0 reverts to its pre-snapshot value.
        assert_eq!(wal.read(0).unwrap().unwrap()[0], 1);
    }

    #[test]
    fn wal_path_generation() {
        let p = Path::new("/tmp/mydb.chiffon");
        assert_eq!(wal_path(p), PathBuf::from("/tmp/mydb.chiffon-wal"));
    }
}
