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
//!   A type's chain is ordered `head (partly filled) → empty spares → full pages`. Adds fill the
//!   head; removals fill their hole from the head. Pages emptied by removals stay in the chain as
//!   spares and are taken again before the file is extended, so a type keeps reusing every page
//!   it ever held. The first spare of the run records where the run ends, in bytes a reader of
//!   entries never looks at (`count` is 0):
//!
//!   ```text
//!   [8..14]  SPARE_MARKER (0xFF × 6 — not a valid RecordId)
//!   [14..18] physical pid of the last spare in the run (u32)
//!   ```
//!
//!   That lets both a full head and an empty head be replaced in O(1) however long the run is.
//!   The record is a hint: writing entry 0 destroys it, it is checked before use, and without it
//!   the code walks (files written before spares existed have none).
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
/// Entry-0 bytes that mark an empty page as carrying an end-of-run record. No RecordId encodes
/// to this (its page id would be the `0xFFFF_FFFF` sentinel), so stale entry bytes cannot match.
const SPARE_MARKER: [u8; ENTRY_SIZE] = [0xFF; ENTRY_SIZE];

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
    /// on the head page, and a full head is replaced by a spare (or, with none left, a fresh
    /// page) linked in front. Walking to a free slot instead made every insert O(set size),
    /// i.e. bulk loads quadratic.
    pub fn add_new(&mut self, type_id: u16, rid: RecordId) -> Result<(), GraphError> {
        let mut new_page = [0u8; PAGE_SIZE];
        write_entry(&mut new_page, 0, rid);
        write_count(&mut new_page, 1);
        let head = match self.head_of(type_id)? {
            Some(head) => head,
            None => {
                write_next(&mut new_page, NO_ROOT);
                let new_pid = self.file.append_page(&new_page)?;
                return self.set_head(type_id, new_pid);
            }
        };
        let mut page = self.file.read_page(head)?;
        let count = read_count(&page);
        if count < ENTRIES_PER_PAGE {
            write_entry(&mut page, count, rid);
            write_count(&mut page, count + 1);
            return self.file.write_page(head, &page);
        }
        if let Some(spare) = self.promote_spare(head)? {
            let mut spare_page = self.file.read_page(spare)?;
            write_entry(&mut spare_page, 0, rid);
            write_count(&mut spare_page, 1);
            self.file.write_page(spare, &spare_page)?;
            return self.set_head(type_id, spare);
        }
        write_next(&mut new_page, head);
        let new_pid = self.file.append_page(&new_page)?;
        self.set_head(type_id, new_pid)
    }

    /// Relinks the chain so the first spare behind the full page `head` comes first and `head`
    /// follows the rest of the spare run: `head → s1 → … → sk → rest` becomes
    /// `s1 → … → sk → head → rest`. Returns `s1`, or `None` when no spare follows `head`.
    ///
    /// Putting `head` directly behind `s1` would be simpler but would strand `s2..sk` behind a
    /// full page, where the next full head could not find them.
    fn promote_spare(&mut self, head: u32) -> Result<Option<u32>, GraphError> {
        let mut head_page = self.file.read_page(head)?;
        let s1 = read_next(&head_page);
        if s1 == NO_ROOT {
            return Ok(None);
        }
        let mut s1_page = self.file.read_page(s1)?;
        if read_count(&s1_page) != 0 {
            return Ok(None);
        }
        let s2 = read_next(&s1_page);
        // Without a usable end-of-run record, treat `s1` as the whole run. Any spare behind it is
        // then stranded until a removal regroups the run, which costs space but loses nothing.
        let tail = match read_spare_tail(&s1_page) {
            Some(t) if t != s1 && t != head && read_count(&self.file.read_page(t)?) == 0 => t,
            _ => s1,
        };
        if tail == s1 {
            write_next(&mut head_page, s2);
            self.file.write_page(head, &head_page)?;
            write_next(&mut s1_page, head);
            self.file.write_page(s1, &s1_page)?;
            return Ok(Some(s1));
        }
        let mut tail_page = self.file.read_page(tail)?;
        write_next(&mut head_page, read_next(&tail_page));
        self.file.write_page(head, &head_page)?;
        write_next(&mut tail_page, head);
        self.file.write_page(tail, &tail_page)?;
        // `s2` now leads the run, so it takes over the end-of-run record (`s2` may be `tail`).
        let mut s2_page = self.file.read_page(s2)?;
        if read_count(&s2_page) == 0 {
            write_spare_tail(&mut s2_page, tail);
            self.file.write_page(s2, &s2_page)?;
        }
        Ok(Some(s1))
    }

    /// Removes `rid` from `type_id`'s set (no-op if absent).
    ///
    /// The hole is filled with the head page's last entry rather than the last entry of the
    /// page it was found on. `add_new` only ever fills the head, so this keeps every page that
    /// holds entries behind the head full; otherwise holes left by deletes would never be reused.
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
        // An empty head has nothing to donate. It stays the head while it is merely empty, so
        // adds and removes alternating across a page boundary touch no links; only now, when a
        // further removal needs an entry from beyond it, does a page with entries take its place.
        if read_count(&donor) == 0 {
            donor_pid = self.promote_first_non_empty(type_id, head)?;
            donor = self.file.read_page(donor_pid)?;
        }
        let donor_count = read_count(&donor);
        if donor_count == 0 {
            return Err(GraphError::StorageCorrupted(donor_pid));
        }
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

    /// Makes the first page with entries behind the empty page `head` the new head, and returns
    /// it: `head → e1 → … → ek → d → rest` becomes `d → head → e1 → … → ek → rest`. The empty
    /// pages (`head` included) stay in the chain as the spare run behind the new head.
    fn promote_first_non_empty(&mut self, type_id: u16, head: u32) -> Result<u32, GraphError> {
        let head_page = self.file.read_page(head)?;
        let first = read_next(&head_page);
        if first == NO_ROOT {
            return Err(GraphError::StorageCorrupted(head));
        }
        let first_page = self.file.read_page(first)?;
        // `last_empty` is the empty page directly in front of `d`.
        let (last_empty, d) = if read_count(&first_page) != 0 {
            (head, first)
        } else {
            match self.end_of_spare_run(first, &first_page)? {
                Some(found) => found,
                None => {
                    // No usable end-of-run record (a chain written before spares existed can
                    // hold empty pages anywhere): walk to the first page with entries.
                    let limit = self.file.page_count()?;
                    let mut steps = 0u32;
                    let mut last_empty = first;
                    let mut d = read_next(&first_page);
                    loop {
                        steps += 1;
                        if d == NO_ROOT || steps > limit {
                            return Err(GraphError::StorageCorrupted(last_empty));
                        }
                        let page = self.file.read_page(d)?;
                        if read_count(&page) != 0 {
                            break;
                        }
                        last_empty = d;
                        d = read_next(&page);
                    }
                    (last_empty, d)
                }
            }
        };

        // Repoint the root at `d` as soon as `d` links back to `head`, before `rest` is hung
        // behind the empty pages. If a later write fails, the chain is then a cycle that the
        // walks report as `StorageCorrupted`; rooting last would instead leave `d` and its
        // entries unreachable with no error.
        let mut d_page = self.file.read_page(d)?;
        let rest = read_next(&d_page);
        write_next(&mut d_page, head);
        self.file.write_page(d, &d_page)?;
        self.set_head(type_id, d)?;
        if last_empty != head {
            let mut last_page = self.file.read_page(last_empty)?;
            write_next(&mut last_page, rest);
            self.file.write_page(last_empty, &last_page)?;
        }
        // `head` now leads the spare run and records where it ends.
        let mut head_page = self.file.read_page(head)?;
        if last_empty == head {
            write_next(&mut head_page, rest);
        }
        write_spare_tail(&mut head_page, last_empty);
        self.file.write_page(head, &head_page)?;
        Ok(d)
    }

    /// Follows the end-of-run record of the spare `first` to `(last spare, page behind it)`.
    /// Returns `None` unless the record checks out: the named page is empty and the page behind
    /// it holds entries.
    fn end_of_spare_run(
        &mut self,
        first: u32,
        first_page: &[u8; PAGE_SIZE],
    ) -> Result<Option<(u32, u32)>, GraphError> {
        let tail = match read_spare_tail(first_page) {
            Some(tail) => tail,
            None => return Ok(None),
        };
        let tail_page = if tail == first {
            *first_page
        } else {
            self.file.read_page(tail)?
        };
        let d = read_next(&tail_page);
        if read_count(&tail_page) != 0 || d == NO_ROOT {
            return Ok(None);
        }
        if read_count(&self.file.read_page(d)?) == 0 {
            return Ok(None);
        }
        Ok(Some((tail, d)))
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

/// Reads the end-of-run record of an empty page, if it carries one.
fn read_spare_tail(page: &[u8; PAGE_SIZE]) -> Option<u32> {
    if read_count(page) != 0 || page[HEADER..HEADER + ENTRY_SIZE] != SPARE_MARKER {
        return None;
    }
    let off = HEADER + ENTRY_SIZE;
    let tail = u32::from_le_bytes(page[off..off + 4].try_into().unwrap_or([0xFF; 4]));
    (tail != NO_ROOT).then_some(tail)
}

/// Records `tail` as the last spare of the run that the empty page `page` leads.
fn write_spare_tail(page: &mut [u8; PAGE_SIZE], tail: u32) {
    let off = HEADER + ENTRY_SIZE;
    page[HEADER..off].copy_from_slice(&SPARE_MARKER);
    page[off..off + 4].copy_from_slice(&tail.to_le_bytes());
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
        // Remove the oldest entry (its hole is filled from the head page) and re-check.
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
    fn empty_head_is_reused_then_kept_as_a_spare_when_reached_past() {
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

        // With the head empty, one more removal reaches past it: the full page becomes the head
        // and the empty one stays behind it as a spare.
        LabelIndex::new(&mut f)
            .remove(1, rid(ENTRIES_PER_PAGE as u32, 0))
            .unwrap();
        LabelIndex::new(&mut f).remove(1, rid(0, 0)).unwrap();
        assert_eq!(chain_counts(&mut f, 1), vec![ENTRIES_PER_PAGE - 1, 0]);
        assert_eq!(
            LabelIndex::new(&mut f).count(1).unwrap(),
            (ENTRIES_PER_PAGE - 1) as u64
        );

        // Growing back refills the head, then takes the spare instead of extending the file.
        LabelIndex::new(&mut f).add_new(1, rid(0, 0)).unwrap();
        LabelIndex::new(&mut f)
            .add_new(1, rid(ENTRIES_PER_PAGE as u32, 0))
            .unwrap();
        assert_eq!(chain_counts(&mut f, 1), vec![1, ENTRIES_PER_PAGE]);
        assert_eq!(f.page_count().unwrap(), pages);
    }

    #[test]
    fn boundary_churn_does_not_grow_the_file() {
        let mut f = file();
        for i in 0..=ENTRIES_PER_PAGE {
            LabelIndex::new(&mut f)
                .add_new(1, rid(i as u32, 0))
                .unwrap();
        }
        let pages = f.page_count().unwrap();
        let a = rid(ENTRIES_PER_PAGE as u32, 0);
        let b = rid(ENTRIES_PER_PAGE as u32 - 1, 0);
        for _ in 0..100 {
            LabelIndex::new(&mut f).remove(1, a).unwrap();
            LabelIndex::new(&mut f).remove(1, b).unwrap();
            LabelIndex::new(&mut f).add_new(1, b).unwrap();
            LabelIndex::new(&mut f).add_new(1, a).unwrap();
        }
        assert_eq!(f.page_count().unwrap(), pages);
    }

    #[test]
    fn multi_page_churn_does_not_grow_the_file() {
        // Shrink by more than two pages' worth and grow back, repeatedly: every page the type
        // ever held must be reused, including spares that sit behind other spares.
        let mut f = file();
        let n = ENTRIES_PER_PAGE * 4 + 5;
        let swing = ENTRIES_PER_PAGE * 2 + 9;
        for i in 0..n {
            LabelIndex::new(&mut f)
                .add_new(1, rid(i as u32, 0))
                .unwrap();
        }
        let pages = f.page_count().unwrap();
        for round in 0..6 {
            // Alternate which end of the id range is churned so holes land on old and new pages.
            let base = if round % 2 == 0 { 0 } else { n - swing };
            for i in base..base + swing {
                LabelIndex::new(&mut f).remove(1, rid(i as u32, 0)).unwrap();
            }
            assert_eq!(
                LabelIndex::new(&mut f).count(1).unwrap(),
                (n - swing) as u64
            );
            for i in base..base + swing {
                LabelIndex::new(&mut f)
                    .add_new(1, rid(i as u32, 0))
                    .unwrap();
            }
            assert_eq!(f.page_count().unwrap(), pages, "round {round}");
        }
        let got: HashSet<_> = LabelIndex::new(&mut f)
            .get(1)
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(got.len(), n);
    }

    #[test]
    fn chain_left_mid_promotion_is_reported_as_corrupt() {
        // Reproduces by hand the state `promote_first_non_empty` is in after its first two
        // writes (`d.next = head`, root → `d`), as if the rest of the relink had failed. The
        // chain is then a cycle, and every walk must report it rather than return a short set.
        let mut f = file();
        for i in 0..=ENTRIES_PER_PAGE * 2 {
            LabelIndex::new(&mut f)
                .add_new(1, rid(i as u32, 0))
                .unwrap();
        }
        LabelIndex::new(&mut f)
            .remove(1, rid(ENTRIES_PER_PAGE as u32 * 2, 0))
            .unwrap();
        assert_eq!(
            chain_counts(&mut f, 1),
            vec![0, ENTRIES_PER_PAGE, ENTRIES_PER_PAGE]
        );

        let head = LabelIndex::new(&mut f).head_of(1).unwrap().unwrap();
        let d = read_next(&f.read_page(head).unwrap());
        let mut d_page = f.read_page(d).unwrap();
        write_next(&mut d_page, head);
        f.write_page(d, &d_page).unwrap();
        LabelIndex::new(&mut f).set_head(1, d).unwrap();

        let mut idx = LabelIndex::new(&mut f);
        assert!(matches!(idx.get(1), Err(GraphError::StorageCorrupted(_))));
        assert!(matches!(idx.count(1), Err(GraphError::StorageCorrupted(_))));
        assert!(matches!(
            idx.remove(1, rid(u32::MAX - 1, 0)),
            Err(GraphError::StorageCorrupted(_))
        ));
        assert!(matches!(
            idx.add(1, rid(u32::MAX - 1, 0)),
            Err(GraphError::StorageCorrupted(_))
        ));
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
    fn remove_keeps_empty_pages_left_by_the_old_tail_append_layout_as_spares() {
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

        let pages = f.page_count().unwrap();
        LabelIndex::new(&mut f).remove(1, rid(7, 0)).unwrap();
        assert_eq!(LabelIndex::new(&mut f).count(1).unwrap(), 0);
        // The page that held the entry moved to the front; no page left the chain.
        assert_eq!(chain_counts(&mut f, 1), vec![0, 0, 0]);
        assert_eq!(LabelIndex::new(&mut f).head_of(1).unwrap(), Some(last_pid));

        // All three pages are filled before the file is extended.
        for i in 0..ENTRIES_PER_PAGE * 3 {
            LabelIndex::new(&mut f)
                .add_new(1, rid(i as u32, 0))
                .unwrap();
        }
        assert_eq!(chain_counts(&mut f, 1), vec![ENTRIES_PER_PAGE; 3]);
        assert_eq!(f.page_count().unwrap(), pages);
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

        /// add_new / add / remove on a set spanning several chain pages. The set starts just past
        /// a page boundary so the ops keep crossing it (empty head, spare kept, spare retaken).
        /// Checks no entry is lost or duplicated and that the chain keeps the shape
        /// `head → empty spares → full pages`.
        #[test]
        fn matches_model_across_chain_pages(
            extra in 0usize..4,
            ops in proptest::collection::vec(
                (0u32..(ENTRIES_PER_PAGE as u32 * 3), 0u8..3),
                1..400,
            ),
        ) {
            let mut f = file();
            let mut model: HashSet<RecordId> = HashSet::new();
            for i in 0..ENTRIES_PER_PAGE * 2 + extra {
                let rec = rid(i as u32, 0);
                LabelIndex::new(&mut f).add_new(1, rec).unwrap();
                model.insert(rec);
            }
            let mut peak = model.len();
            for (r, op) in ops {
                let rec = rid(r, 0);
                match op {
                    0 => {
                        LabelIndex::new(&mut f).remove(1, rec).unwrap();
                        model.remove(&rec);
                    }
                    1 => {
                        LabelIndex::new(&mut f).add(1, rec).unwrap();
                        model.insert(rec);
                    }
                    _ => {
                        // add_new requires the rid to be absent.
                        if model.insert(rec) {
                            LabelIndex::new(&mut f).add_new(1, rec).unwrap();
                        }
                    }
                }
                prop_assert_eq!(
                    LabelIndex::new(&mut f).count(1).unwrap(),
                    model.len() as u64
                );
                let counts = chain_counts(&mut f, 1);
                prop_assert!(
                    counts
                        .iter()
                        .skip(1)
                        .skip_while(|&&c| c == 0)
                        .all(|&c| c == ENTRIES_PER_PAGE),
                    "not head -> empty spares -> full pages: {:?}",
                    counts
                );
                // Spares are retaken before the file is extended, so the chain is exactly as
                // long as the largest set it has held requires.
                peak = peak.max(model.len());
                prop_assert_eq!(counts.len(), peak.div_ceil(ENTRIES_PER_PAGE));
            }
            let got = LabelIndex::new(&mut f).get(1).unwrap();
            prop_assert_eq!(got.len(), model.len());
            prop_assert_eq!(got.into_iter().collect::<HashSet<_>>(), model);
        }

        /// Chains written by the old tail-append layout can have any fill per page (a full head
        /// with partial or empty pages behind it). add_new / add / remove must stay correct on
        /// them even though the "pages behind the head are full" shape does not hold.
        #[test]
        fn matches_model_on_legacy_chain_shapes(
            fills in proptest::collection::vec(
                prop_oneof![
                    Just(0usize),
                    Just(1usize),
                    Just(ENTRIES_PER_PAGE),
                    0usize..=ENTRIES_PER_PAGE,
                ],
                1..5,
            ),
            ops in proptest::collection::vec((0u32..4000, 0u8..3), 1..300),
        ) {
            let mut f = file();
            let mut model: HashSet<RecordId> = HashSet::new();
            // Build tail first so each page can point at the one behind it; ids are dense so the
            // ops below hit every page.
            let mut next_pid = NO_ROOT;
            let mut next_id = 0u32;
            for &fill in fills.iter().rev() {
                let mut page = [0u8; PAGE_SIZE];
                write_next(&mut page, next_pid);
                for i in 0..fill {
                    let rec = rid(next_id, 0);
                    next_id += 1;
                    write_entry(&mut page, i, rec);
                    model.insert(rec);
                }
                write_count(&mut page, fill);
                next_pid = f.append_page(&page).unwrap();
            }
            LabelIndex::new(&mut f).set_head(1, next_pid).unwrap();

            let span = next_id + 50;
            for (r, op) in ops {
                let rec = rid(r % span, 0);
                match op {
                    0 => {
                        LabelIndex::new(&mut f).remove(1, rec).unwrap();
                        model.remove(&rec);
                    }
                    1 => {
                        LabelIndex::new(&mut f).add(1, rec).unwrap();
                        model.insert(rec);
                    }
                    _ => {
                        if model.insert(rec) {
                            LabelIndex::new(&mut f).add_new(1, rec).unwrap();
                        }
                    }
                }
                prop_assert_eq!(
                    LabelIndex::new(&mut f).count(1).unwrap(),
                    model.len() as u64
                );
            }
            let got = LabelIndex::new(&mut f).get(1).unwrap();
            prop_assert_eq!(got.len(), model.len());
            prop_assert_eq!(got.into_iter().collect::<HashSet<_>>(), model);
        }

        /// The end-of-run record is a hint, not a fact. Plant records naming arbitrary pages of
        /// the same chain on its empty pages (an older binary, or the walking fallback, can leave
        /// such records behind): the set must stay exact and no page may leave the chain.
        #[test]
        fn wrong_spare_hints_never_lose_entries_or_pages(
            fills in proptest::collection::vec(
                prop_oneof![
                    3 => Just(0usize),
                    1 => Just(ENTRIES_PER_PAGE),
                    1 => Just(ENTRIES_PER_PAGE - 1),
                    1 => 1usize..ENTRIES_PER_PAGE,
                ],
                2..7,
            ),
            hints in proptest::collection::vec(proptest::option::of(0usize..7), 7),
            ops in proptest::collection::vec((0u32..6000, 0u8..3), 1..400),
        ) {
            let mut f = file();
            let mut model: HashSet<RecordId> = HashSet::new();
            let mut next_pid = NO_ROOT;
            let mut next_id = 0u32;
            // Chain order, head first.
            let mut pids: Vec<u32> = Vec::new();
            for &fill in fills.iter().rev() {
                let mut page = [0u8; PAGE_SIZE];
                write_next(&mut page, next_pid);
                for i in 0..fill {
                    let rec = rid(next_id, 0);
                    next_id += 1;
                    write_entry(&mut page, i, rec);
                    model.insert(rec);
                }
                write_count(&mut page, fill);
                next_pid = f.append_page(&page).unwrap();
                pids.insert(0, next_pid);
            }
            for (i, &pid) in pids.iter().enumerate() {
                if let (0, Some(target)) = (fills[i], hints[i]) {
                    let mut page = f.read_page(pid).unwrap();
                    write_spare_tail(&mut page, pids[target % pids.len()]);
                    f.write_page(pid, &page).unwrap();
                }
            }
            LabelIndex::new(&mut f).set_head(1, next_pid).unwrap();

            let span = next_id + 400;
            for (r, op) in ops {
                let rec = rid(r % span, 0);
                match op {
                    0 => {
                        LabelIndex::new(&mut f).remove(1, rec).unwrap();
                        model.remove(&rec);
                    }
                    1 => {
                        LabelIndex::new(&mut f).add(1, rec).unwrap();
                        model.insert(rec);
                    }
                    _ => {
                        if model.insert(rec) {
                            LabelIndex::new(&mut f).add_new(1, rec).unwrap();
                        }
                    }
                }
                prop_assert_eq!(
                    LabelIndex::new(&mut f).count(1).unwrap(),
                    model.len() as u64
                );
            }
            let got = LabelIndex::new(&mut f).get(1).unwrap();
            prop_assert_eq!(got.len(), model.len());
            prop_assert_eq!(got.into_iter().collect::<HashSet<_>>(), model);
            let chain: HashSet<u32> = chain_pids(&mut f, 1).into_iter().collect();
            for pid in pids {
                prop_assert!(chain.contains(&pid), "page {} left the chain", pid);
            }
        }
    }

    /// Swings of several pages' worth: shrink far enough to leave a run of spares, grow back,
    /// repeat. Removals are scattered over the whole set so holes land on every page. Checks the
    /// set, the chain shape, and that the file is never extended while a spare exists.
    ///
    /// A fixed set of pseudo-random scenarios rather than a proptest: one scenario is thousands of
    /// chain walks, so `PROPTEST_CASES=1000` would dominate the whole suite's run time.
    #[test]
    fn matches_model_through_multi_page_swings() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as usize
        };
        for _ in 0..12 {
            let mut f = file();
            let mut live: Vec<RecordId> = Vec::new();
            let mut next_id = 0u32;
            for _ in 0..ENTRIES_PER_PAGE * 3 + next(4) {
                let rec = rid(next_id, 0);
                next_id += 1;
                LabelIndex::new(&mut f).add_new(1, rec).unwrap();
                live.push(rec);
            }
            let mut peak = live.len();
            for _ in 0..1 + next(3) {
                let shrink = 1 + next(ENTRIES_PER_PAGE * 3);
                let grow = next(ENTRIES_PER_PAGE * 3);
                let stride = 1 + next(1000);
                let mut at = 0usize;
                for _ in 0..shrink.min(live.len()) {
                    at = (at + stride) % live.len();
                    let rec = live.swap_remove(at);
                    LabelIndex::new(&mut f).remove(1, rec).unwrap();
                }
                assert_spare_chain(&mut f, live.len(), peak).unwrap();
                for _ in 0..grow {
                    let rec = rid(next_id, 0);
                    next_id += 1;
                    LabelIndex::new(&mut f).add_new(1, rec).unwrap();
                    live.push(rec);
                }
                peak = peak.max(live.len());
                assert_spare_chain(&mut f, live.len(), peak).unwrap();
            }
            let got = LabelIndex::new(&mut f).get(1).unwrap();
            assert_eq!(got.len(), live.len());
            assert_eq!(
                got.into_iter().collect::<HashSet<_>>(),
                live.into_iter().collect::<HashSet<_>>()
            );
        }
    }

    /// Page ids of `type_id`'s chain, head first.
    fn chain_pids(f: &mut DatabaseFile, type_id: u16) -> Vec<u32> {
        let mut out = Vec::new();
        let mut pid = match LabelIndex::new(f).head_of(type_id).unwrap() {
            Some(pid) => pid,
            None => return out,
        };
        loop {
            out.push(pid);
            pid = read_next(&f.read_page(pid).unwrap());
            if pid == NO_ROOT {
                return out;
            }
        }
    }

    /// Asserts the chain holds `len` entries as `head → empty spares → full pages` and is exactly
    /// as long as the largest set it has held (`peak`) requires.
    fn assert_spare_chain(
        f: &mut DatabaseFile,
        len: usize,
        peak: usize,
    ) -> Result<(), TestCaseError> {
        prop_assert_eq!(LabelIndex::new(f).count(1).unwrap(), len as u64);
        let counts = chain_counts(f, 1);
        prop_assert!(
            counts
                .iter()
                .skip(1)
                .skip_while(|&&c| c == 0)
                .all(|&c| c == ENTRIES_PER_PAGE),
            "not head -> empty spares -> full pages: {:?}",
            counts
        );
        prop_assert_eq!(counts.len(), peak.div_ceil(ENTRIES_PER_PAGE));
        Ok(())
    }
}
