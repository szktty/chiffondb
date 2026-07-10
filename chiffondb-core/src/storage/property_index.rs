//! Tier-2 property index: a declared `(type, PropertyPath) value → {RecordId}` index that
//! accelerates equality `find` / `find_all` from O(all-nodes-of-type) to O(matches).
//!
//! Declared via `@index` in the schema (design §11 tier 2). The key is a `PropertyPath` value
//! (flat or nested scalar; arrays / object-terminal / `None` are excluded — partial index).
//!
//! ## Structure (equality-only; the B+tree is deferred per design §9 cost discipline)
//!
//! An entry is **fixed size** so it reuses the tier-1 page-chain machinery: rather than storing
//! the variable-length value inline, it stores a 64-bit hash of `(type_id, path, value)`:
//!
//! ```text
//! entry (24 bytes):
//!   [0..2]   type_id (u16)
//!   [2..10]  path_hash (u64)   — hash of the PropertyPath's display form
//!   [10..18] value_hash (u64)  — hash of the encoded scalar value bytes
//!   [18..24] RecordId (6 bytes)
//! ```
//!
//! Entries live in a fixed number of **hash buckets**; a key hashes to a bucket, whose chain of
//! pages holds the entries. `get` returns **candidate** RecordIds (hash collisions can produce a
//! few extras); the caller (`find`/`find_all`) re-reads each candidate's real property value and
//! confirms equality, so results are always exact. This keeps the on-disk structure as simple as
//! tier-1 while giving O(matches + collisions) lookups.
//!
//! Bucket head pids live in a directory page chain rooted at the header's `property_index_root`.
//! All pages go through `read_page`/`write_page` → WAL, so the index has no in-memory state and
//! rolls back with the data (design §11.3).

use crate::error::GraphError;
use crate::storage::file::DatabaseFile;
use crate::storage::page::{RecordId, PAGE_SIZE};
use crate::storage::record::{decode_record_id_6, encode_record_id_6};

/// Sentinel for "no next page" / "no root yet".
pub const NO_ROOT: u32 = 0xFFFF_FFFF;

const HEADER: usize = 8; // next link (4) + count (4)
const ENTRY_SIZE: usize = 24; // type_id(2) + path_hash(8) + value_hash(8) + rid(6)
const ENTRIES_PER_PAGE: usize = (PAGE_SIZE - HEADER) / ENTRY_SIZE;
/// Number of hash buckets. Fixed and modest: buckets are just chain heads, and the caller
/// confirms candidates, so a small count trades a slightly longer chain for a tiny directory.
const BUCKET_COUNT: usize = 256;
/// Bucket-directory page: [0..4] next, [4..8] count, then u32 bucket-head pids.
const DIR_SLOTS_PER_PAGE: usize = (PAGE_SIZE - HEADER) / 4;

/// A fixed-size index key: which `(type_id, path)` index, and which value.
#[derive(Clone, Copy)]
pub struct IndexKey {
    pub type_id: u16,
    pub path_hash: u64,
    pub value_hash: u64,
}

impl IndexKey {
    /// Builds a key from a type, a `PropertyPath` display form, and encoded scalar value bytes.
    pub fn new(type_id: u16, path_display: &str, value_bytes: &[u8]) -> Self {
        Self {
            type_id,
            path_hash: hash_bytes(path_display.as_bytes()),
            value_hash: hash_bytes(value_bytes),
        }
    }

    fn bucket(&self) -> usize {
        // Combine the three components so different (type, path, value) spread across buckets.
        // Uses FNV-1a (see `fnv1a`) — a fixed algorithm, unlike `DefaultHasher` whose output may
        // change across Rust releases and would silently break a persisted index (M-3).
        let mut buf = [0u8; 18];
        buf[0..2].copy_from_slice(&self.type_id.to_le_bytes());
        buf[2..10].copy_from_slice(&self.path_hash.to_le_bytes());
        buf[10..18].copy_from_slice(&self.value_hash.to_le_bytes());
        (fnv1a(&buf) as usize) % BUCKET_COUNT
    }

    fn matches(&self, type_id: u16, path_hash: u64, value_hash: u64) -> bool {
        self.type_id == type_id && self.path_hash == path_hash && self.value_hash == value_hash
    }
}

/// 64-bit FNV-1a hash. A fixed, specified algorithm so hashes persisted in the on-disk index stay
/// valid across Rust toolchain updates (unlike `std`'s `DefaultHasher`, which is explicitly not
/// guaranteed stable between releases — M-3).
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    fnv1a(bytes)
}

/// A persisted equality property index. Holds no state beyond the root pid it reads from / writes
/// back to the file header; all data lives on disk.
pub struct PropertyIndex<'a> {
    file: &'a mut DatabaseFile,
}

impl<'a> PropertyIndex<'a> {
    pub fn new(file: &'a mut DatabaseFile) -> Self {
        Self { file }
    }

    /// Adds `(key → rid)` (idempotent for an identical entry).
    pub fn add(&mut self, key: IndexKey, rid: RecordId) -> Result<(), GraphError> {
        let head = self.bucket_head_ensure(key.bucket())?;
        let limit = self.file.page_count()?;
        let mut steps = 0u32;
        let mut pid = head;
        let mut first_free: Option<u32> = None;
        loop {
            let page = self.file.read_page(pid)?;
            let count = read_count(&page);
            for i in 0..count {
                let (t, ph, vh, r) = read_entry(&page, i);
                if key.matches(t, ph, vh) && r == rid {
                    return Ok(());
                }
            }
            if count < ENTRIES_PER_PAGE && first_free.is_none() {
                first_free = Some(pid);
            }
            let next = read_next(&page);
            if next == NO_ROOT {
                break;
            }
            steps += 1;
            if steps > limit {
                return Err(GraphError::StorageCorrupted(pid));
            }
            pid = next;
        }
        match first_free {
            Some(free_pid) => {
                let mut page = self.file.read_page(free_pid)?;
                let count = read_count(&page);
                write_entry(&mut page, count, key, rid);
                write_count(&mut page, count + 1);
                self.file.write_page(free_pid, &page)?;
            }
            None => {
                let new_pid = self.append_empty_page()?;
                let mut new_page = self.file.read_page(new_pid)?;
                write_entry(&mut new_page, 0, key, rid);
                write_count(&mut new_page, 1);
                self.file.write_page(new_pid, &new_page)?;
                self.link_tail(head, new_pid)?;
            }
        }
        Ok(())
    }

    /// Removes `(key → rid)` (no-op if absent).
    pub fn remove(&mut self, key: IndexKey, rid: RecordId) -> Result<(), GraphError> {
        let head = match self.bucket_head(key.bucket())? {
            Some(pid) => pid,
            None => return Ok(()),
        };
        let limit = self.file.page_count()?;
        let mut steps = 0u32;
        let mut pid = head;
        loop {
            let mut page = self.file.read_page(pid)?;
            let count = read_count(&page);
            for i in 0..count {
                let (t, ph, vh, r) = read_entry(&page, i);
                if key.matches(t, ph, vh) && r == rid {
                    let last = read_entry(&page, count - 1);
                    write_entry_raw(&mut page, i, last);
                    write_count(&mut page, count - 1);
                    self.file.write_page(pid, &page)?;
                    return Ok(());
                }
            }
            let next = read_next(&page);
            if next == NO_ROOT {
                return Ok(());
            }
            steps += 1;
            if steps > limit {
                return Err(GraphError::StorageCorrupted(pid));
            }
            pid = next;
        }
    }

    /// Returns candidate RecordIds for `key`. Hash collisions may include a few extras, so the
    /// caller must confirm each candidate's real value.
    pub fn candidates(&mut self, key: IndexKey) -> Result<Vec<RecordId>, GraphError> {
        let mut out = Vec::new();
        let mut pid = match self.bucket_head(key.bucket())? {
            Some(pid) => pid,
            None => return Ok(out),
        };
        let limit = self.file.page_count()?;
        let mut steps = 0u32;
        loop {
            let page = self.file.read_page(pid)?;
            let count = read_count(&page);
            for i in 0..count {
                let (t, ph, vh, r) = read_entry(&page, i);
                if key.matches(t, ph, vh) {
                    out.push(r);
                }
            }
            let next = read_next(&page);
            if next == NO_ROOT {
                break;
            }
            steps += 1;
            if steps > limit {
                return Err(GraphError::StorageCorrupted(pid));
            }
            pid = next;
        }
        Ok(out)
    }

    // ---- Bucket directory (bucket index → head pid) ----

    /// Reads bucket `b`'s head pid, or `None` if unset.
    fn bucket_head(&mut self, b: usize) -> Result<Option<u32>, GraphError> {
        let root = self.file.header.property_index_root;
        if root == NO_ROOT {
            return Ok(None);
        }
        let (dir_pid, slot) = self.dir_location(b);
        let mut pid = root;
        for _ in 0..dir_pid {
            let page = self.file.read_page(pid)?;
            let next = read_next(&page);
            if next == NO_ROOT {
                return Ok(None);
            }
            pid = next;
        }
        let page = self.file.read_page(pid)?;
        let head = read_dir_slot(&page, slot);
        Ok(if head == NO_ROOT { None } else { Some(head) })
    }

    /// Reads bucket `b`'s head pid, creating the directory and an empty bucket chain if needed.
    fn bucket_head_ensure(&mut self, b: usize) -> Result<u32, GraphError> {
        if let Some(pid) = self.bucket_head(b)? {
            return Ok(pid);
        }
        // Ensure the bucket directory exists and is long enough to hold slot for bucket `b`.
        if self.file.header.property_index_root == NO_ROOT {
            let pid = self.append_empty_dir_page()?;
            self.file.header.property_index_root = pid;
            self.file.write_header()?;
        }
        let (dir_pid, slot) = self.dir_location(b);
        let mut pid = self.file.header.property_index_root;
        for _ in 0..dir_pid {
            let page = self.file.read_page(pid)?;
            let next = read_next(&page);
            if next != NO_ROOT {
                pid = next;
                continue;
            }
            let new_pid = self.append_empty_dir_page()?;
            let mut p = page;
            write_next(&mut p, new_pid);
            self.file.write_page(pid, &p)?;
            pid = new_pid;
        }
        // Allocate an empty bucket chain page and record it in the directory slot.
        let head = self.append_empty_page()?;
        let mut page = self.file.read_page(pid)?;
        write_dir_slot(&mut page, slot, head);
        // Keep the directory page's count field as the highest slot touched + 1 (informational).
        let count = read_count(&page).max(slot + 1);
        write_count(&mut page, count);
        self.file.write_page(pid, &page)?;
        Ok(head)
    }

    /// (directory page index, slot within that page) for bucket `b`.
    fn dir_location(&self, b: usize) -> (usize, usize) {
        (b / DIR_SLOTS_PER_PAGE, b % DIR_SLOTS_PER_PAGE)
    }

    fn append_empty_page(&mut self) -> Result<u32, GraphError> {
        let mut page = [0u8; PAGE_SIZE];
        write_next(&mut page, NO_ROOT);
        write_count(&mut page, 0);
        self.file.append_page(&page)
    }

    fn append_empty_dir_page(&mut self) -> Result<u32, GraphError> {
        let mut page = [0u8; PAGE_SIZE];
        write_next(&mut page, NO_ROOT);
        write_count(&mut page, 0);
        for slot in 0..DIR_SLOTS_PER_PAGE {
            write_dir_slot(&mut page, slot, NO_ROOT);
        }
        self.file.append_page(&page)
    }

    fn link_tail(&mut self, head: u32, new_pid: u32) -> Result<(), GraphError> {
        let limit = self.file.page_count()?;
        let mut steps = 0u32;
        let mut pid = head;
        loop {
            let mut page = self.file.read_page(pid)?;
            let next = read_next(&page);
            if next == NO_ROOT {
                write_next(&mut page, new_pid);
                self.file.write_page(pid, &page)?;
                return Ok(());
            }
            steps += 1;
            if steps > limit {
                return Err(GraphError::StorageCorrupted(pid));
            }
            pid = next;
        }
    }
}

// ---- Byte helpers ----

fn read_next(page: &[u8; PAGE_SIZE]) -> u32 {
    u32::from_le_bytes(page[0..4].try_into().unwrap_or([0xFF; 4]))
}

fn write_next(page: &mut [u8; PAGE_SIZE], pid: u32) {
    page[0..4].copy_from_slice(&pid.to_le_bytes());
}

fn read_count(page: &[u8; PAGE_SIZE]) -> usize {
    let raw = u32::from_le_bytes(page[4..8].try_into().unwrap_or([0; 4])) as usize;
    // Clamp against the max entries a bucket-chain page holds, so a corrupt count can never drive
    // `read_entry` past the page. (Directory pages don't use read_count for iteration.)
    raw.min(ENTRIES_PER_PAGE)
}

fn write_count(page: &mut [u8; PAGE_SIZE], count: usize) {
    page[4..8].copy_from_slice(&(count as u32).to_le_bytes());
}

fn read_dir_slot(page: &[u8; PAGE_SIZE], slot: usize) -> u32 {
    let off = HEADER + slot * 4;
    u32::from_le_bytes(page[off..off + 4].try_into().unwrap_or([0xFF; 4]))
}

fn write_dir_slot(page: &mut [u8; PAGE_SIZE], slot: usize, pid: u32) {
    let off = HEADER + slot * 4;
    page[off..off + 4].copy_from_slice(&pid.to_le_bytes());
}

fn read_entry(page: &[u8; PAGE_SIZE], idx: usize) -> (u16, u64, u64, RecordId) {
    let off = HEADER + idx * ENTRY_SIZE;
    let type_id = u16::from_le_bytes(page[off..off + 2].try_into().unwrap_or([0; 2]));
    let path_hash = u64::from_le_bytes(page[off + 2..off + 10].try_into().unwrap_or([0; 8]));
    let value_hash = u64::from_le_bytes(page[off + 10..off + 18].try_into().unwrap_or([0; 8]));
    let mut rbuf = [0u8; 6];
    rbuf.copy_from_slice(&page[off + 18..off + 24]);
    (type_id, path_hash, value_hash, decode_record_id_6(&rbuf))
}

fn write_entry(page: &mut [u8; PAGE_SIZE], idx: usize, key: IndexKey, rid: RecordId) {
    write_entry_raw(page, idx, (key.type_id, key.path_hash, key.value_hash, rid));
}

fn write_entry_raw(page: &mut [u8; PAGE_SIZE], idx: usize, e: (u16, u64, u64, RecordId)) {
    let off = HEADER + idx * ENTRY_SIZE;
    page[off..off + 2].copy_from_slice(&e.0.to_le_bytes());
    page[off + 2..off + 10].copy_from_slice(&e.1.to_le_bytes());
    page[off + 10..off + 18].copy_from_slice(&e.2.to_le_bytes());
    page[off + 18..off + 24].copy_from_slice(&encode_record_id_6(e.3));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::page::{PageId, SlotId};
    use std::collections::HashSet;

    fn rid(page: u32, slot: u16) -> RecordId {
        RecordId {
            page_id: PageId(page),
            slot_id: SlotId(slot),
        }
    }

    fn key(type_id: u16, path: &str, value: &str) -> IndexKey {
        IndexKey::new(type_id, path, value.as_bytes())
    }

    fn file() -> DatabaseFile {
        DatabaseFile::create_in_memory().unwrap()
    }

    #[test]
    fn add_candidates_roundtrip() {
        let mut f = file();
        let mut idx = PropertyIndex::new(&mut f);
        idx.add(key(1, "email", "a@x"), rid(0, 0)).unwrap();
        idx.add(key(1, "email", "a@x"), rid(0, 1)).unwrap();
        idx.add(key(1, "email", "b@x"), rid(2, 0)).unwrap();

        let mut a: Vec<_> = idx.candidates(key(1, "email", "a@x")).unwrap();
        a.sort_by_key(|r| r.slot_id.0);
        assert_eq!(a, vec![rid(0, 0), rid(0, 1)]);
        assert_eq!(
            idx.candidates(key(1, "email", "b@x")).unwrap(),
            vec![rid(2, 0)]
        );
        // A value never added yields no candidates.
        assert!(idx.candidates(key(1, "email", "none")).unwrap().is_empty());
    }

    #[test]
    fn distinct_type_and_path_do_not_collide() {
        let mut f = file();
        let mut idx = PropertyIndex::new(&mut f);
        idx.add(key(1, "email", "x"), rid(0, 0)).unwrap();
        idx.add(key(2, "email", "x"), rid(1, 0)).unwrap(); // different type
        idx.add(key(1, "name", "x"), rid(2, 0)).unwrap(); // different path
        assert_eq!(
            idx.candidates(key(1, "email", "x")).unwrap(),
            vec![rid(0, 0)]
        );
        assert_eq!(
            idx.candidates(key(2, "email", "x")).unwrap(),
            vec![rid(1, 0)]
        );
        assert_eq!(
            idx.candidates(key(1, "name", "x")).unwrap(),
            vec![rid(2, 0)]
        );
    }

    #[test]
    fn add_is_idempotent_and_remove_works() {
        let mut f = file();
        let mut idx = PropertyIndex::new(&mut f);
        idx.add(key(1, "k", "v"), rid(0, 0)).unwrap();
        idx.add(key(1, "k", "v"), rid(0, 0)).unwrap();
        assert_eq!(idx.candidates(key(1, "k", "v")).unwrap().len(), 1);
        idx.remove(key(1, "k", "v"), rid(0, 0)).unwrap();
        assert!(idx.candidates(key(1, "k", "v")).unwrap().is_empty());
        // Removing absent is a no-op.
        idx.remove(key(1, "k", "v"), rid(9, 9)).unwrap();
    }

    #[test]
    fn many_values_spread_across_buckets_and_persist() {
        // Many distinct values exercise the bucket directory and chain growth; a fresh view
        // (reading the root from the header) must see them all.
        let mut f = file();
        {
            let mut idx = PropertyIndex::new(&mut f);
            for i in 0..1000u32 {
                idx.add(key(1, "n", &i.to_string()), rid(i, 0)).unwrap();
            }
        }
        let mut idx2 = PropertyIndex::new(&mut f);
        for i in 0..1000u32 {
            let got: HashSet<_> = idx2
                .candidates(key(1, "n", &i.to_string()))
                .unwrap()
                .into_iter()
                .collect();
            // The intended rid must be among the candidates (collisions may add extras).
            assert!(got.contains(&rid(i, 0)), "missing value {i}");
        }
    }
}
