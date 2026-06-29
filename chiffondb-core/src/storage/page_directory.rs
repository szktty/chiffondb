//! Logical→physical page directory.
//!
//! Maps a dense logical page number to the physical file page that holds it, so the
//! topology (and later property) segments no longer need a fixed, contiguous physical
//! range. Removing that fixed range is what lifts the old ~2000-node cap (see
//! `docs/design-variable-topology.md`).
//!
//! On-disk layout: the directory is a chain of *directory pages*. Each directory page is
//!
//! ```text
//! [0..4]   next directory page id (u32, 0xFFFF_FFFF = end of chain)
//! [4..]    entries: physical page id (u32) per logical slot, little-endian
//! ```
//!
//! Logical entry `i` lives in directory page `i / ENTRIES_PER_DIR_PAGE`, at slot
//! `i % ENTRIES_PER_DIR_PAGE`. An unmapped entry holds the sentinel `UNMAPPED`.
//!
//! The directory pages themselves are faulted through `DatabaseFile`'s bounded page cache,
//! so the directory adds no resident memory that grows with the logical space; `PageDirectory`
//! itself keeps only the root page id and the mapped length (O(1) metadata).

use crate::error::GraphError;
use crate::storage::file::DatabaseFile;
use crate::storage::page::PAGE_SIZE;

/// Sentinel for an unmapped logical entry / end-of-chain link.
const UNMAPPED: u32 = 0xFFFF_FFFF;

/// Bytes of a directory page reserved for the next-page link.
const LINK_SIZE: usize = 4;

/// Number of physical-page-id entries that fit in one directory page.
pub const ENTRIES_PER_DIR_PAGE: usize = (PAGE_SIZE - LINK_SIZE) / 4;

/// A persisted logical→physical page mapping, rooted at a directory-page chain.
///
/// Holds only O(1) metadata: the root directory page id and the number of mapped logical
/// entries. Directory pages are read/written on demand via `DatabaseFile`.
#[derive(Clone, Debug)]
pub struct PageDirectory {
    /// Physical page id of the first directory page. `UNMAPPED` until the first mapping is
    /// stored (the directory is created lazily).
    root: u32,
    /// Number of mapped logical entries (the logical space is `0..len`).
    len: usize,
}

impl PageDirectory {
    /// Creates an empty directory (no root page allocated yet).
    pub fn new() -> Self {
        Self {
            root: UNMAPPED,
            len: 0,
        }
    }

    /// Restores a directory from persisted metadata (root page id + mapped length).
    pub fn with_root(root: u32, len: usize) -> Self {
        Self { root, len }
    }

    /// Physical page id of the root directory page (`UNMAPPED` if none allocated).
    pub fn root(&self) -> u32 {
        self.root
    }

    /// Number of mapped logical entries.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Resolves logical page `logical` to its physical page id.
    ///
    /// Returns `None` if `logical` is beyond the mapped length or the slot is unmapped.
    pub fn resolve(
        &self,
        file: &mut DatabaseFile,
        logical: usize,
    ) -> Result<Option<u32>, GraphError> {
        if logical >= self.len {
            return Ok(None);
        }
        let dir_index = logical / ENTRIES_PER_DIR_PAGE;
        let slot = logical % ENTRIES_PER_DIR_PAGE;
        let dir_pid = match self.dir_page_pid(file, dir_index)? {
            Some(pid) => pid,
            None => return Ok(None),
        };
        let page = file.read_page(dir_pid)?;
        let physical = read_entry(&page, slot);
        Ok(if physical == UNMAPPED {
            None
        } else {
            Some(physical)
        })
    }

    /// Appends a new logical→physical mapping and returns the assigned logical page number.
    ///
    /// Extends the directory-page chain as needed.
    pub fn push(&mut self, file: &mut DatabaseFile, physical: u32) -> Result<usize, GraphError> {
        let logical = self.len;
        self.set(file, logical, physical)?;
        self.len = logical + 1;
        Ok(logical)
    }

    /// Sets logical entry `logical` to `physical`, extending the chain as needed.
    ///
    /// `logical` may equal `len` (append) but must not skip past it: the logical space is
    /// kept dense so that `len` alone bounds the mapped range.
    fn set(
        &mut self,
        file: &mut DatabaseFile,
        logical: usize,
        physical: u32,
    ) -> Result<(), GraphError> {
        if logical > self.len {
            return Err(GraphError::StorageCorrupted(logical as u32));
        }
        let dir_index = logical / ENTRIES_PER_DIR_PAGE;
        let slot = logical % ENTRIES_PER_DIR_PAGE;
        let dir_pid = self.ensure_dir_page(file, dir_index)?;
        let mut page = file.read_page(dir_pid)?;
        write_entry(&mut page, slot, physical);
        file.write_page(dir_pid, &page)?;
        Ok(())
    }

    /// Returns the physical page id of directory page `dir_index`, or `None` if the chain is
    /// shorter than `dir_index + 1`.
    fn dir_page_pid(
        &self,
        file: &mut DatabaseFile,
        dir_index: usize,
    ) -> Result<Option<u32>, GraphError> {
        if self.root == UNMAPPED {
            return Ok(None);
        }
        let mut pid = self.root;
        for _ in 0..dir_index {
            let page = file.read_page(pid)?;
            let next = read_link(&page);
            if next == UNMAPPED {
                return Ok(None);
            }
            pid = next;
        }
        Ok(Some(pid))
    }

    /// Ensures directory page `dir_index` exists (appending blank pages and linking them),
    /// and returns its physical page id.
    fn ensure_dir_page(
        &mut self,
        file: &mut DatabaseFile,
        dir_index: usize,
    ) -> Result<u32, GraphError> {
        if self.root == UNMAPPED {
            self.root = append_blank_dir_page(file)?;
        }
        let mut pid = self.root;
        for _ in 0..dir_index {
            let mut page = file.read_page(pid)?;
            let next = read_link(&page);
            if next != UNMAPPED {
                pid = next;
                continue;
            }
            let new_pid = append_blank_dir_page(file)?;
            write_link(&mut page, new_pid);
            file.write_page(pid, &page)?;
            pid = new_pid;
        }
        Ok(pid)
    }
}

impl Default for PageDirectory {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds a blank directory page (all entries unmapped, no next link) and appends it,
/// returning its physical page id.
fn append_blank_dir_page(file: &mut DatabaseFile) -> Result<u32, GraphError> {
    let mut page = [0u8; PAGE_SIZE];
    write_link(&mut page, UNMAPPED);
    for slot in 0..ENTRIES_PER_DIR_PAGE {
        write_entry(&mut page, slot, UNMAPPED);
    }
    file.append_page(&page)
}

fn read_link(page: &[u8; PAGE_SIZE]) -> u32 {
    u32::from_le_bytes(page[0..4].try_into().unwrap_or([0xFF; 4]))
}

fn write_link(page: &mut [u8; PAGE_SIZE], pid: u32) {
    page[0..4].copy_from_slice(&pid.to_le_bytes());
}

fn entry_offset(slot: usize) -> usize {
    LINK_SIZE + slot * 4
}

fn read_entry(page: &[u8; PAGE_SIZE], slot: usize) -> u32 {
    let off = entry_offset(slot);
    u32::from_le_bytes(page[off..off + 4].try_into().unwrap_or([0xFF; 4]))
}

fn write_entry(page: &mut [u8; PAGE_SIZE], slot: usize, physical: u32) {
    let off = entry_offset(slot);
    page[off..off + 4].copy_from_slice(&physical.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> DatabaseFile {
        // Page 0 stands in for the header so appended directory pages start at pid 1,
        // matching how a real database reserves page 0.
        let mut f = DatabaseFile::create_in_memory().unwrap();
        f.append_page(&[0u8; PAGE_SIZE]).unwrap();
        f
    }

    #[test]
    fn empty_directory_resolves_nothing() {
        let mut f = file();
        let dir = PageDirectory::new();
        assert!(dir.is_empty());
        assert_eq!(dir.resolve(&mut f, 0).unwrap(), None);
    }

    #[test]
    fn push_then_resolve_roundtrips() {
        let mut f = file();
        let mut dir = PageDirectory::new();
        let l0 = dir.push(&mut f, 42).unwrap();
        let l1 = dir.push(&mut f, 99).unwrap();
        assert_eq!(l0, 0);
        assert_eq!(l1, 1);
        assert_eq!(dir.len(), 2);
        assert_eq!(dir.resolve(&mut f, 0).unwrap(), Some(42));
        assert_eq!(dir.resolve(&mut f, 1).unwrap(), Some(99));
        assert_eq!(dir.resolve(&mut f, 2).unwrap(), None);
    }

    #[test]
    fn resolve_beyond_len_is_none() {
        let mut f = file();
        let mut dir = PageDirectory::new();
        dir.push(&mut f, 7).unwrap();
        assert_eq!(dir.resolve(&mut f, 1).unwrap(), None);
        assert_eq!(dir.resolve(&mut f, 1000).unwrap(), None);
    }

    #[test]
    fn chain_extends_across_directory_pages() {
        // Push past one directory page so the chain must grow to a second page.
        let mut f = file();
        let mut dir = PageDirectory::new();
        let n = ENTRIES_PER_DIR_PAGE + 5;
        for i in 0..n {
            // Use a distinct, recognizable physical id per logical entry.
            let logical = dir.push(&mut f, 1000 + i as u32).unwrap();
            assert_eq!(logical, i);
        }
        assert_eq!(dir.len(), n);
        // Boundary entries around the directory-page split must all resolve correctly.
        for i in [0, ENTRIES_PER_DIR_PAGE - 1, ENTRIES_PER_DIR_PAGE, n - 1] {
            assert_eq!(dir.resolve(&mut f, i).unwrap(), Some(1000 + i as u32));
        }
    }

    #[test]
    fn with_root_restores_mapping() {
        let mut f = file();
        let (root, len) = {
            let mut dir = PageDirectory::new();
            dir.push(&mut f, 11).unwrap();
            dir.push(&mut f, 22).unwrap();
            (dir.root(), dir.len())
        };
        // Reopen from persisted metadata only.
        let restored = PageDirectory::with_root(root, len);
        assert_eq!(restored.resolve(&mut f, 0).unwrap(), Some(11));
        assert_eq!(restored.resolve(&mut f, 1).unwrap(), Some(22));
    }
}
