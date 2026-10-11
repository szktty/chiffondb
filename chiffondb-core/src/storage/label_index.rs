//! Tier-1 label (type) index: an always-on, persisted `type_id → {RecordId}` map.
//!
//! Turns `list_nodes(type)` / `count_nodes(type)` / `rids_of_type` from an O(all-nodes) topology
//! scan into O(matches). A node is registered under **every** label it carries (primary
//! `node_type_id` plus additional/dynamic labels), so `MATCH (n:Label)` hits under any of a
//! node's labels (design §11 tier 1, key = all labels).
//!
//! Structure (kept deliberately simple — a set index, not a B-tree; the B-tree is Phase 5):
//!
//! - **Root pages** map `type_id → head pid` of that type's RecordId chain. Root pages form a
//!   chain themselves (types are few — usually one root page suffices):
//!
//!   ```text
//!   [0..4]   next root page (physical pid, 0xFFFF_FFFF = end)
//!   [4..8]   count: number of (type_id, head) slots used on this page (u32)
//!   [8..]    slots: type_id (u16) + head chain pid (u32) = 6 bytes each
//!   ```
//!
//! - **Chain pages** hold a bounded array of `RecordId`s for one type:
//!
//!   ```text
//!   [0..4]   next chain page (physical pid, 0xFFFF_FFFF = end)
//!   [4..8]   count: number of live RecordId entries (u32)
//!   [8..]    entries: RecordId (6 bytes each)
//!   ```
//!
//! Every page is faulted through `DatabaseFile`'s bounded page cache and every mutation rides
//! `read_page`/`write_page` → WAL, so the index has no in-memory state and rolls back with the
//! data (design §11.3). The root of the whole structure (the first root page, or `NO_ROOT` when
//! empty) lives in the file header.

use crate::error::GraphError;
use crate::storage::file::DatabaseFile;
use crate::storage::page::{RecordId, PAGE_SIZE};
use crate::storage::record::{decode_record_id_6, encode_record_id_6};

/// Sentinel for "no next page" / "no root page yet".
pub const NO_ROOT: u32 = 0xFFFF_FFFF;

const HEADER: usize = 8; // next link (4) + count (4)
const ROOT_SLOT_SIZE: usize = 6; // type_id (2) + head pid (4)
const ROOT_SLOTS_PER_PAGE: usize = (PAGE_SIZE - HEADER) / ROOT_SLOT_SIZE;
const ENTRY_SIZE: usize = 6; // RecordId (6 bytes)
const ENTRIES_PER_PAGE: usize = (PAGE_SIZE - HEADER) / ENTRY_SIZE;

/// A persisted `type_id → {RecordId}` index. Holds no state beyond the root pid it reads from
/// (and writes back to) the file header; all data lives on disk.
pub struct LabelIndex<'a> {
    file: &'a mut DatabaseFile,
}

impl<'a> LabelIndex<'a> {
    pub fn new(file: &'a mut DatabaseFile) -> Self {
        Self { file }
    }

    /// Adds `rid` to `type_id`'s set (idempotent).
    ///
    /// Walks the whole chain to rule out a duplicate, so it costs O(set size). Callers that
    /// already know `rid` is absent should use [`add_new`](Self::add_new).
    pub fn add(&mut self, type_id: u16, rid: RecordId) -> Result<(), GraphError> {
        let mut pid = match self.head_of(type_id)? {
            Some(pid) => pid,
            None => return self.add_new(type_id, rid),
        };
        let limit = self.file.page_count()?;
        let mut steps = 0u32;
        loop {
            let page = self.file.read_page(pid)?;
            let count = read_count(&page);
            for i in 0..count {
                if read_entry(&page, i) == rid {
                    return Ok(());
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
        self.add_new(type_id, rid)
    }

    /// Adds `rid` to `type_id`'s set without checking for a duplicate.
    ///
    /// The caller must guarantee `rid` is not already registered under `type_id`; a violation
    /// leaves a duplicate entry. In exchange the cost is O(1) in the set size: the entry goes
    /// on the head page, and a full head gets a fresh page linked in front of it. Walking to a
    /// free slot instead made every insert O(set size), i.e. bulk loads quadratic.
    pub fn add_new(&mut self, type_id: u16, rid: RecordId) -> Result<(), GraphError> {
        if let Some(head) = self.head_of(type_id)? {
            let mut page = self.file.read_page(head)?;
            let count = read_count(&page);
            if count < ENTRIES_PER_PAGE {
                write_entry(&mut page, count, rid);
                write_count(&mut page, count + 1);
                return self.file.write_page(head, &page);
            }
            let mut new_page = [0u8; PAGE_SIZE];
            write_next(&mut new_page, head);
            write_entry(&mut new_page, 0, rid);
            write_count(&mut new_page, 1);
            let new_pid = self.file.append_page(&new_page)?;
            return self.set_head(type_id, new_pid);
        }
        let mut new_page = [0u8; PAGE_SIZE];
        write_next(&mut new_page, NO_ROOT);
        write_entry(&mut new_page, 0, rid);
        write_count(&mut new_page, 1);
        let new_pid = self.file.append_page(&new_page)?;
        self.set_head(type_id, new_pid)
    }

    /// Removes `rid` from `type_id`'s set (no-op if absent).
    ///
    /// The hole is filled with the head page's last entry rather than the last entry of the
    /// page it was found on. `add_new` only ever fills the head, so this keeps every page
    /// behind the head full; otherwise holes left by deletes would never be reused.
    pub fn remove(&mut self, type_id: u16, rid: RecordId) -> Result<(), GraphError> {
        let head = match self.head_of(type_id)? {
            Some(pid) => pid,
            None => return Ok(()),
        };
        let limit = self.file.page_count()?;
        let mut steps = 0u32;
        let mut pid = head;
        loop {
            let page = self.file.read_page(pid)?;
            let count = read_count(&page);
            for i in 0..count {
                if read_entry(&page, i) == rid {
                    return self.fill_hole_from_head(type_id, head, pid, i);
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

    /// Overwrites entry `idx` of page `pid` with the head page's last entry and shrinks the head.
    fn fill_hole_from_head(
        &mut self,
        type_id: u16,
        head: u32,
        pid: u32,
        idx: usize,
    ) -> Result<(), GraphError> {
        let mut donor_pid = head;
        let mut donor = self.file.read_page(donor_pid)?;
        // An empty head has nothing to donate. It is kept while it is merely empty (so adds and
        // removes alternating across a page boundary reuse it) and unlinked only now, when a
        // further removal has to reach past it. The unlinked page is not reclaimed. This loops
        // because files written before head insertion can hold empty pages mid-chain; it ends at
        // `pid` at the latest, which holds the entry being removed.
        let limit = self.file.page_count()?;
        let mut steps = 0u32;
        while read_count(&donor) == 0 {
            let next = read_next(&donor);
            steps += 1;
            if next == NO_ROOT || steps > limit {
                return Err(GraphError::StorageCorrupted(donor_pid));
            }
            self.set_head(type_id, next)?;
            donor_pid = next;
            donor = self.file.read_page(donor_pid)?;
        }
        let donor_count = read_count(&donor);
        let last = read_entry(&donor, donor_count - 1);
        write_count(&mut donor, donor_count - 1);
        if pid == donor_pid {
            // `idx == donor_count - 1` rewrites a slot past the new count, which is harmless.
            write_entry(&mut donor, idx, last);
            return self.file.write_page(donor_pid, &donor);
        }
        self.file.write_page(donor_pid, &donor)?;
        let mut page = self.file.read_page(pid)?;
        write_entry(&mut page, idx, last);
        self.file.write_page(pid, &page)
    }

    /// Every RecordId registered under `type_id`.
    pub fn get(&mut self, type_id: u16) -> Result<Vec<RecordId>, GraphError> {
        let mut out = Vec::new();
        let mut pid = match self.head_of(type_id)? {
            Some(pid) => pid,
            None => return Ok(out),
        };
        let limit = self.file.page_count()?;
        let mut steps = 0u32;
        loop {
            let page = self.file.read_page(pid)?;
            let count = read_count(&page);
            for i in 0..count {
                out.push(read_entry(&page, i));
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

    /// Number of RecordIds registered under `type_id`.
    pub fn count(&mut self, type_id: u16) -> Result<u64, GraphError> {
        let mut total = 0u64;
        let mut pid = match self.head_of(type_id)? {
            Some(pid) => pid,
            None => return Ok(0),
        };
        let limit = self.file.page_count()?;
        let mut steps = 0u32;
        loop {
            let page = self.file.read_page(pid)?;
            total += read_count(&page) as u64;
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
        Ok(total)
    }

    // ---- Root map (type_id → head pid) ----

    /// Looks up the head chain page for `type_id`, walking the root-page chain.
    fn head_of(&mut self, type_id: u16) -> Result<Option<u32>, GraphError> {
        let mut pid = self.file.header.label_index_root;
        if pid == NO_ROOT {
            return Ok(None);
        }
        let limit = self.file.page_count()?;
        let mut steps = 0u32;
        loop {
            let page = self.file.read_page(pid)?;
            let count = read_count(&page);
            for i in 0..count {
                let (t, head) = read_root_slot(&page, i);
                if t == type_id {
                    return Ok(Some(head));
                }
            }
            let next = read_next(&page);
            if next == NO_ROOT {
                return Ok(None);
            }
            steps += 1;
            if steps > limit {
                return Err(GraphError::StorageCorrupted(pid));
            }
            pid = next;
        }
    }

    /// Records `type_id → head` in the root map, appending a root page/slot as needed.
    fn set_head(&mut self, type_id: u16, head: u32) -> Result<(), GraphError> {
        // Ensure a root page exists.
        if self.file.header.label_index_root == NO_ROOT {
            let pid = self.append_empty_page()?;
            self.file.header.label_index_root = pid;
            self.file.write_header()?;
        }
        let root = self.file.header.label_index_root;

        // Walk root pages: update an existing slot for type_id, else find room for a new slot.
        let limit = self.file.page_count()?;
        let mut steps = 0u32;
        let mut pid = root;
        loop {
            let mut page = self.file.read_page(pid)?;
            let count = read_count(&page);
            for i in 0..count {
                let (t, _) = read_root_slot(&page, i);
                if t == type_id {
                    write_root_slot(&mut page, i, type_id, head);
                    self.file.write_page(pid, &page)?;
                    return Ok(());
                }
            }
            if count < ROOT_SLOTS_PER_PAGE {
                write_root_slot(&mut page, count, type_id, head);
                write_count(&mut page, count + 1);
                self.file.write_page(pid, &page)?;
                return Ok(());
            }
            let next = read_next(&page);
            if next == NO_ROOT {
                // Append and link a new root page, then place the slot there.
                let new_pid = self.append_empty_page()?;
                let mut new_page = self.file.read_page(new_pid)?;
                write_root_slot(&mut new_page, 0, type_id, head);
                write_count(&mut new_page, 1);
                self.file.write_page(new_pid, &new_page)?;
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

    fn append_empty_page(&mut self) -> Result<u32, GraphError> {
        let mut page = [0u8; PAGE_SIZE];
        write_next(&mut page, NO_ROOT);
        write_count(&mut page, 0);
        self.file.append_page(&page)
    }
}

// ---- Byte helpers (shared header layout: [0..4] next, [4..8] count) ----

fn read_next(page: &[u8; PAGE_SIZE]) -> u32 {
    u32::from_le_bytes(page[0..4].try_into().unwrap_or([0xFF; 4]))
}

fn write_next(page: &mut [u8; PAGE_SIZE], pid: u32) {
    page[0..4].copy_from_slice(&pid.to_le_bytes());
}

fn read_count(page: &[u8; PAGE_SIZE]) -> usize {
    let raw = u32::from_le_bytes(page[4..8].try_into().unwrap_or([0; 4])) as usize;
    // Clamp against the max entries/slots a page can hold so a corrupt count can never drive a
    // read past the page (entry and root-slot are both 6 bytes → same bound).
    raw.min(ENTRIES_PER_PAGE.max(ROOT_SLOTS_PER_PAGE))
}

fn write_count(page: &mut [u8; PAGE_SIZE], count: usize) {
    page[4..8].copy_from_slice(&(count as u32).to_le_bytes());
}

fn read_entry(page: &[u8; PAGE_SIZE], idx: usize) -> RecordId {
    let off = HEADER + idx * ENTRY_SIZE;
    let mut buf = [0u8; 6];
    buf.copy_from_slice(&page[off..off + ENTRY_SIZE]);
    decode_record_id_6(&buf)
}

fn write_entry(page: &mut [u8; PAGE_SIZE], idx: usize, rid: RecordId) {
    let off = HEADER + idx * ENTRY_SIZE;
    page[off..off + ENTRY_SIZE].copy_from_slice(&encode_record_id_6(rid));
}

fn read_root_slot(page: &[u8; PAGE_SIZE], idx: usize) -> (u16, u32) {
    let off = HEADER + idx * ROOT_SLOT_SIZE;
    let t = u16::from_le_bytes(page[off..off + 2].try_into().unwrap_or([0; 2]));
    let head = u32::from_le_bytes(page[off + 2..off + 6].try_into().unwrap_or([0xFF; 4]));
    (t, head)
}

fn write_root_slot(page: &mut [u8; PAGE_SIZE], idx: usize, type_id: u16, head: u32) {
    let off = HEADER + idx * ROOT_SLOT_SIZE;
    page[off..off + 2].copy_from_slice(&type_id.to_le_bytes());
    page[off + 2..off + 6].copy_from_slice(&head.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::page::{PageId, SlotId};
    use proptest::prelude::*;
    use std::collections::{HashMap, HashSet};

    fn rid(page: u32, slot: u16) -> RecordId {
        RecordId {
            page_id: PageId(page),
            slot_id: SlotId(slot),
        }
    }

    fn file() -> DatabaseFile {
        DatabaseFile::create_in_memory().unwrap()
    }

    #[test]
    fn add_get_count_roundtrip() {
        let mut f = file();
        let mut idx = LabelIndex::new(&mut f);
        idx.add(1, rid(0, 0)).unwrap();
        idx.add(1, rid(0, 1)).unwrap();
        idx.add(2, rid(5, 0)).unwrap();

        let mut ones: Vec<_> = idx.get(1).unwrap();
        ones.sort_by_key(|r| r.slot_id.0);
        assert_eq!(ones, vec![rid(0, 0), rid(0, 1)]);
        assert_eq!(idx.count(1).unwrap(), 2);
        assert_eq!(idx.get(2).unwrap(), vec![rid(5, 0)]);
        assert_eq!(idx.count(2).unwrap(), 1);
        // An unseen type has an empty set.
        assert_eq!(idx.get(9).unwrap(), Vec::<RecordId>::new());
        assert_eq!(idx.count(9).unwrap(), 0);
    }

    #[test]
    fn add_is_idempotent() {
        let mut f = file();
        let mut idx = LabelIndex::new(&mut f);
        idx.add(1, rid(0, 0)).unwrap();
        idx.add(1, rid(0, 0)).unwrap();
        assert_eq!(idx.count(1).unwrap(), 1);
    }

    #[test]
    fn remove_deletes_and_is_noop_when_absent() {
        let mut f = file();
        let mut idx = LabelIndex::new(&mut f);
        idx.add(1, rid(0, 0)).unwrap();
        idx.add(1, rid(0, 1)).unwrap();
        idx.remove(1, rid(0, 0)).unwrap();
        assert_eq!(idx.get(1).unwrap(), vec![rid(0, 1)]);
        // Removing something not present is a no-op.
        idx.remove(1, rid(9, 9)).unwrap();
        idx.remove(7, rid(0, 1)).unwrap();
        assert_eq!(idx.count(1).unwrap(), 1);
    }

    #[test]
    fn chain_spans_multiple_pages() {
        // Force several chain pages for one type.
        let mut f = file();
        let mut idx = LabelIndex::new(&mut f);
        let n = ENTRIES_PER_PAGE * 2 + 3;
        for i in 0..n {
            idx.add(1, rid(i as u32, 0)).unwrap();
        }
        assert_eq!(idx.count(1).unwrap(), n as u64);
        let got: HashSet<_> = idx.get(1).unwrap().into_iter().collect();
        assert_eq!(got.len(), n);
        // Remove one from the first page (swap-remove pulls from the tail) and re-check.
        idx.remove(1, rid(0, 0)).unwrap();
        assert_eq!(idx.count(1).unwrap(), (n - 1) as u64);
        assert!(!idx.get(1).unwrap().contains(&rid(0, 0)));
    }

    #[test]
    fn persists_across_reopen_via_header_root() {
        // The root pid lives in the header; a fresh LabelIndex view sees prior writes.
        let mut f = file();
        {
            let mut idx = LabelIndex::new(&mut f);
            idx.add(3, rid(1, 0)).unwrap();
            idx.add(3, rid(2, 0)).unwrap();
        }
        let mut idx2 = LabelIndex::new(&mut f);
        assert_eq!(idx2.count(3).unwrap(), 2);
    }

    /// Per-page entry counts of `type_id`'s chain, head first.
    fn chain_counts(f: &mut DatabaseFile, type_id: u16) -> Vec<usize> {
        let mut out = Vec::new();
        let mut pid = match LabelIndex::new(f).head_of(type_id).unwrap() {
            Some(pid) => pid,
            None => return out,
        };
        loop {
            let page = f.read_page(pid).unwrap();
            out.push(read_count(&page));
            pid = read_next(&page);
            if pid == NO_ROOT {
                return out;
            }
        }
    }

    #[test]
    fn add_new_fills_the_head_and_links_new_pages_in_front() {
        let mut f = file();
        let n = ENTRIES_PER_PAGE * 2 + 3;
        for i in 0..n {
            LabelIndex::new(&mut f)
                .add_new(1, rid(i as u32, 0))
                .unwrap();
        }
        assert_eq!(
            chain_counts(&mut f, 1),
            vec![3, ENTRIES_PER_PAGE, ENTRIES_PER_PAGE]
        );
        let got: HashSet<_> = LabelIndex::new(&mut f)
            .get(1)
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(got.len(), n);
    }

    #[test]
    fn remove_keeps_pages_behind_the_head_full() {
        let mut f = file();
        let n = ENTRIES_PER_PAGE * 2 + 3;
        for i in 0..n {
            LabelIndex::new(&mut f)
                .add_new(1, rid(i as u32, 0))
                .unwrap();
        }
        // rid 0 sits on the last (oldest) page; its hole is filled from the head.
        LabelIndex::new(&mut f).remove(1, rid(0, 0)).unwrap();
        assert_eq!(
            chain_counts(&mut f, 1),
            vec![2, ENTRIES_PER_PAGE, ENTRIES_PER_PAGE]
        );
        let got: HashSet<_> = LabelIndex::new(&mut f)
            .get(1)
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(got.len(), n - 1);
        assert!(!got.contains(&rid(0, 0)));
    }

    #[test]
    fn empty_head_is_reused_then_unlinked_only_when_reached_past() {
        let mut f = file();
        for i in 0..=ENTRIES_PER_PAGE {
            LabelIndex::new(&mut f)
                .add_new(1, rid(i as u32, 0))
                .unwrap();
        }
        assert_eq!(chain_counts(&mut f, 1), vec![1, ENTRIES_PER_PAGE]);

        // Alternating across the page boundary must not grow the file.
        let pages = f.page_count().unwrap();
        for _ in 0..5 {
            LabelIndex::new(&mut f)
                .remove(1, rid(ENTRIES_PER_PAGE as u32, 0))
                .unwrap();
            assert_eq!(chain_counts(&mut f, 1), vec![0, ENTRIES_PER_PAGE]);
            LabelIndex::new(&mut f)
                .add_new(1, rid(ENTRIES_PER_PAGE as u32, 0))
                .unwrap();
        }
        assert_eq!(f.page_count().unwrap(), pages);

        // With the head empty, one more removal reaches past it and drops it from the chain.
        LabelIndex::new(&mut f)
            .remove(1, rid(ENTRIES_PER_PAGE as u32, 0))
            .unwrap();
        LabelIndex::new(&mut f).remove(1, rid(0, 0)).unwrap();
        assert_eq!(chain_counts(&mut f, 1), vec![ENTRIES_PER_PAGE - 1]);
        assert_eq!(
            LabelIndex::new(&mut f).count(1).unwrap(),
            (ENTRIES_PER_PAGE - 1) as u64
        );
    }

    #[test]
    fn remove_last_entry_leaves_an_empty_set() {
        let mut f = file();
        let mut idx = LabelIndex::new(&mut f);
        idx.add_new(1, rid(0, 0)).unwrap();
        idx.remove(1, rid(0, 0)).unwrap();
        assert_eq!(idx.count(1).unwrap(), 0);
        idx.remove(1, rid(0, 0)).unwrap();
        idx.add_new(1, rid(1, 0)).unwrap();
        assert_eq!(idx.get(1).unwrap(), vec![rid(1, 0)]);
    }

    #[test]
    fn remove_skips_empty_pages_left_by_the_old_tail_append_layout() {
        // Files written before head insertion appended at the tail and swap-removed within a
        // page, so an empty page can sit anywhere. Build: empty head -> empty -> one entry.
        let mut f = file();
        let mut last = [0u8; PAGE_SIZE];
        write_next(&mut last, NO_ROOT);
        write_entry(&mut last, 0, rid(7, 0));
        write_count(&mut last, 1);
        let last_pid = f.append_page(&last).unwrap();
        let mut mid = [0u8; PAGE_SIZE];
        write_next(&mut mid, last_pid);
        let mid_pid = f.append_page(&mid).unwrap();
        let mut head = [0u8; PAGE_SIZE];
        write_next(&mut head, mid_pid);
        let head_pid = f.append_page(&head).unwrap();
        LabelIndex::new(&mut f).set_head(1, head_pid).unwrap();

        LabelIndex::new(&mut f).remove(1, rid(7, 0)).unwrap();
        assert_eq!(LabelIndex::new(&mut f).count(1).unwrap(), 0);
        assert_eq!(chain_counts(&mut f, 1), vec![0]);
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(50))]
        /// A random sequence of add/remove ops leaves each type's set equal to a model HashSet.
        #[test]
        fn matches_model_under_random_ops(
            ops in proptest::collection::vec(
                (0u16..4, 0u32..20, any::<bool>()),
                1..300,
            )
        ) {
            let mut f = file();
            let mut idx = LabelIndex::new(&mut f);
            let mut model: HashMap<u16, HashSet<RecordId>> = HashMap::new();
            for (t, r, is_add) in ops {
                let rec = rid(r, 0);
                if is_add {
                    idx.add(t, rec).unwrap();
                    model.entry(t).or_default().insert(rec);
                } else {
                    idx.remove(t, rec).unwrap();
                    model.entry(t).or_default().remove(&rec);
                }
            }
            for t in 0u16..4 {
                let got: HashSet<_> = idx.get(t).unwrap().into_iter().collect();
                let want = model.get(&t).cloned().unwrap_or_default();
                prop_assert_eq!(got, want);
            }
        }
    }
}
