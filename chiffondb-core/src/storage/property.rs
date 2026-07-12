use crate::error::GraphError;
use crate::storage::page::{SlotId, PAGE_SIZE};

/// Header size of a slotted page: `slot_count: u16` + `live_count: u16` (A-2).
const HEADER_SIZE: usize = 4;
/// Size of one entry in the slot directory (offset: u16 + length: u16).
const SLOT_ENTRY_SIZE: usize = 4;

/// Directory-entry `offset` sentinel marking a freed (tombstoned) slot (A-2). A live slot's real
/// offset is always `< PAGE_SIZE` (4096), so `0xFFFF` is unambiguous. `is_valid()`/`read_property`
/// treat a slot with this offset as dead: skipped by validity, read as `PropertySlotFreed`.
pub const TOMBSTONE_OFF: u16 = 0xFFFF;

/// `slot_count` sentinel marking a whole page as a member of the property free-page list (A-2).
/// A live slotted page can hold at most `(PAGE_SIZE - HEADER_SIZE) / SLOT_ENTRY_SIZE ≈ 1023` slots,
/// so `0xFFFF` can never be a real live `slot_count` — it unambiguously means "this page is on the
/// free-list; skip it in the ordinary allocator scan and never read it as content" (the
/// disambiguation invariant, design §3.2, review-2026-07-12b).
pub const FREE_PAGE_MARKER: u16 = 0xFFFF;

/// Slotted-page operations.
///
/// Layout:
///   [0..2]   slot_count (u16, LE) — total slots ever allocated (never decreases; bounds the dir)
///   [2..4]   live_count (u16, LE) — non-tombstoned slots; `0` ⇒ page fully empty, eligible to free
///   [4..]    slot directory (4 bytes each: offset u16 + length u16; offset == TOMBSTONE_OFF = dead)
///   data area packed from the end toward the header
///
/// A page whose `slot_count == FREE_PAGE_MARKER` is not a live slotted page at all — it is on the
/// property free-page list (`is_valid()` rejects it).
pub struct SlottedPage {
    data: [u8; PAGE_SIZE],
}

impl Default for SlottedPage {
    fn default() -> Self {
        Self::new()
    }
}

impl SlottedPage {
    pub fn new() -> Self {
        let mut page = Self {
            data: [0u8; PAGE_SIZE],
        };
        page.set_slot_count(0);
        page.set_live_count(0);
        page
    }

    pub fn from_bytes(bytes: [u8; PAGE_SIZE]) -> Self {
        Self { data: bytes }
    }

    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.data
    }

    pub fn slot_count(&self) -> u16 {
        u16::from_le_bytes(self.data[0..2].try_into().unwrap())
    }

    /// Number of live (non-tombstoned) slots on this page (A-2). `0` ⇒ the page holds no live
    /// values and is eligible to be returned to the property free-page list. Independent of
    /// `slot_count()` (which only ever grows), so "page fully empty" is an O(1) test that does not
    /// depend on `is_valid()`.
    pub fn live_count(&self) -> u16 {
        u16::from_le_bytes(self.data[2..4].try_into().unwrap())
    }

    /// Performs a basic validity check for this slotted page.
    /// Guards against accidentally reading pages with a different format, such as blob-chain pages.
    pub fn is_valid(&self) -> bool {
        let count = self.slot_count() as usize;
        // A page on the property free-page list carries the marker in `slot_count`; it is not a
        // live slotted page, so the ordinary allocator scan must skip it (design §3.2 invariant).
        if count == FREE_PAGE_MARKER as usize {
            return false;
        }
        let dir_end = HEADER_SIZE + count * SLOT_ENTRY_SIZE;
        if dir_end > PAGE_SIZE {
            return false;
        }
        // Verify that each *live* slot's offset falls within the page bounds. A tombstoned slot
        // (offset == TOMBSTONE_OFF) is dead, not corrupt — skip it so one freed slot doesn't make
        // the whole page fail validity (that was the M-A bug: it hid the page from the allocator).
        for i in 0..count {
            let dir_offset = HEADER_SIZE + i * SLOT_ENTRY_SIZE;
            let offset =
                u16::from_le_bytes(self.data[dir_offset..dir_offset + 2].try_into().unwrap());
            if offset == TOMBSTONE_OFF {
                continue;
            }
            let length = u16::from_le_bytes(
                self.data[dir_offset + 2..dir_offset + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            if offset as usize + length > PAGE_SIZE {
                return false;
            }
        }
        true
    }

    /// Writes data and returns the slot number. Returns Err if there is no space.
    pub fn write_property(&mut self, value: &[u8]) -> Result<SlotId, GraphError> {
        let len = value.len();
        let count = self.slot_count() as usize;
        let dir_end = HEADER_SIZE + (count + 1) * SLOT_ENTRY_SIZE;
        let data_end = self.data_end();
        if dir_end + len > data_end {
            return Err(GraphError::StorageCorrupted(0));
        }
        let new_data_end = data_end - len;
        self.data[new_data_end..new_data_end + len].copy_from_slice(value);
        let slot_id = count as u16;
        let dir_offset = HEADER_SIZE + count * SLOT_ENTRY_SIZE;
        self.data[dir_offset..dir_offset + 2].copy_from_slice(&(new_data_end as u16).to_le_bytes());
        self.data[dir_offset + 2..dir_offset + 4].copy_from_slice(&(len as u16).to_le_bytes());
        self.set_slot_count(count as u16 + 1);
        self.set_live_count(self.live_count() + 1);
        Ok(SlotId(slot_id))
    }

    /// Tombstones slot `slot`, marking it dead so its space is no longer referenced (A-2). The
    /// directory entry's offset is set to `TOMBSTONE_OFF` and `live_count` is decremented; the
    /// slot's bytes are not compacted (that's A-2b), but once `live_count` reaches 0 the whole page
    /// is eligible to return to the property free-page list.
    ///
    /// Idempotent: freeing an already-tombstoned slot is a silent no-op (O-5), so a double-free /
    /// stale RID does not corrupt a live neighbor (only slot `slot`'s own entry is touched).
    pub fn free_slot(&mut self, slot: SlotId) -> Result<(), GraphError> {
        let idx = slot.0 as usize;
        if idx >= self.slot_count() as usize {
            return Err(GraphError::StorageCorrupted(0));
        }
        let dir_offset = HEADER_SIZE + idx * SLOT_ENTRY_SIZE;
        let offset = u16::from_le_bytes(self.data[dir_offset..dir_offset + 2].try_into().unwrap());
        if offset == TOMBSTONE_OFF {
            return Ok(()); // already freed — idempotent no-op (O-5)
        }
        self.data[dir_offset..dir_offset + 2].copy_from_slice(&TOMBSTONE_OFF.to_le_bytes());
        self.data[dir_offset + 2..dir_offset + 4].copy_from_slice(&0u16.to_le_bytes());
        self.set_live_count(self.live_count().saturating_sub(1));
        Ok(())
    }

    /// Bytes available for the *next* value written to this page: the gap between the data area
    /// and the slot directory after reserving one more directory entry. `write_property(v)`
    /// succeeds iff `v.len() <= free_space()`. Used by the property allocator's free-page hint to
    /// decide whether a page is "effectively full" (design §3.3).
    pub fn free_space(&self) -> usize {
        let count = self.slot_count() as usize;
        let dir_end = HEADER_SIZE + (count + 1) * SLOT_ENTRY_SIZE;
        self.data_end().saturating_sub(dir_end)
    }

    /// Reads data from a slot.
    pub fn read_property(&self, slot: SlotId) -> Result<&[u8], GraphError> {
        let idx = slot.0 as usize;
        if idx >= self.slot_count() as usize {
            return Err(GraphError::StorageCorrupted(0));
        }
        let dir_offset = HEADER_SIZE + idx * SLOT_ENTRY_SIZE;
        let offset_raw =
            u16::from_le_bytes(self.data[dir_offset..dir_offset + 2].try_into().unwrap());
        // A tombstoned slot is a freed, expected state (e.g. a stale/cached RID), distinct from
        // genuine corruption — surface it as PropertySlotFreed, not StorageCorrupted (O-5, §3.3).
        if offset_raw == TOMBSTONE_OFF {
            return Err(GraphError::PropertySlotFreed);
        }
        let offset = offset_raw as usize;
        let length = u16::from_le_bytes(
            self.data[dir_offset + 2..dir_offset + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        // The offset/length come from on-disk bytes (a corrupt or hostile page can put anything
        // here), so bound-check before slicing rather than panicking on out-of-range data.
        if offset.checked_add(length).is_none_or(|end| end > PAGE_SIZE) {
            return Err(GraphError::StorageCorrupted(0));
        }
        Ok(&self.data[offset..offset + length])
    }

    /// Returns the current end offset of the data area (the trailing boundary of writable space).
    fn data_end(&self) -> usize {
        let count = self.slot_count() as usize;
        if count == 0 {
            return PAGE_SIZE;
        }
        // Slots are packed from the end, so the minimum offset is the current data_end.
        (0..count)
            .map(|i| {
                let dir_offset = HEADER_SIZE + i * SLOT_ENTRY_SIZE;
                u16::from_le_bytes(self.data[dir_offset..dir_offset + 2].try_into().unwrap())
                    as usize
            })
            .min()
            .unwrap_or(PAGE_SIZE)
    }

    fn set_slot_count(&mut self, count: u16) {
        self.data[0..2].copy_from_slice(&count.to_le_bytes());
    }

    fn set_live_count(&mut self, count: u16) {
        self.data[2..4].copy_from_slice(&count.to_le_bytes());
    }
}

// ---- Page chain (multi-page linked list for blobs larger than 4 KB) ----

/// Header of each page in a page chain (first 8 bytes).
///   [0..4] next_page_id: u32 (0xFFFFFFFF = no next page)
///   [4..8] chunk_length: u32 (number of data bytes stored in this page)
const CHAIN_HEADER_SIZE: usize = 8;
const NO_NEXT_PAGE: u32 = 0xFFFF_FFFF;
pub const CHAIN_DATA_PER_PAGE: usize = PAGE_SIZE - CHAIN_HEADER_SIZE;

/// Calculates the number of pages required to write a page chain.
pub fn pages_needed(data_len: usize) -> usize {
    data_len.div_ceil(CHAIN_DATA_PER_PAGE)
}

/// Writes data into a page chain.
/// `pages` is a slice of empty pages prepared by the caller (must have the required count).
/// The PageId of each page is assumed to be (base_page_id + i).
pub fn write_blob_chain(pages: &mut [[u8; PAGE_SIZE]], data: &[u8]) -> Result<(), GraphError> {
    let needed = pages_needed(data.len());
    if pages.len() < needed {
        return Err(GraphError::StorageCorrupted(0));
    }
    let mut offset = 0;
    for (i, page) in pages.iter_mut().enumerate().take(needed) {
        let chunk_end = (offset + CHAIN_DATA_PER_PAGE).min(data.len());
        let chunk = &data[offset..chunk_end];
        let chunk_len = chunk.len() as u32;
        let next = if i + 1 < needed {
            (i + 1) as u32
        } else {
            NO_NEXT_PAGE
        };
        page[0..4].copy_from_slice(&next.to_le_bytes());
        page[4..8].copy_from_slice(&chunk_len.to_le_bytes());
        page[CHAIN_HEADER_SIZE..CHAIN_HEADER_SIZE + chunk.len()].copy_from_slice(chunk);
        offset = chunk_end;
    }
    Ok(())
}

/// Reads data from a page chain.
pub fn read_blob_chain(pages: &[[u8; PAGE_SIZE]]) -> Result<Vec<u8>, GraphError> {
    let mut result = Vec::new();
    let mut page_idx = 0usize;
    // Bound the walk by the page count so a corrupt `next` (cycle / repeat) cannot loop forever.
    let mut steps = 0usize;
    loop {
        let page = pages
            .get(page_idx)
            .ok_or(GraphError::StorageCorrupted(page_idx as u32))?;
        let next = u32::from_le_bytes(page[0..4].try_into().unwrap());
        let chunk_len = u32::from_le_bytes(page[4..8].try_into().unwrap()) as usize;
        if chunk_len > CHAIN_DATA_PER_PAGE {
            return Err(GraphError::StorageCorrupted(page_idx as u32));
        }
        result.extend_from_slice(&page[CHAIN_HEADER_SIZE..CHAIN_HEADER_SIZE + chunk_len]);
        if next == NO_NEXT_PAGE {
            break;
        }
        steps += 1;
        if steps > pages.len() {
            return Err(GraphError::StorageCorrupted(page_idx as u32));
        }
        page_idx = next as usize;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ---- SlottedPage tests ----

    #[test]
    fn write_and_read_single_property() {
        let mut page = SlottedPage::new();
        let data = b"hello, chiffondb";
        let slot = page.write_property(data).unwrap();
        assert_eq!(page.read_property(slot).unwrap(), data);
    }

    #[test]
    fn write_multiple_properties() {
        let mut page = SlottedPage::new();
        let a = b"alpha";
        let b = b"beta_beta_beta";
        let c = b"gamma_gamma_gamma_gamma";
        let sa = page.write_property(a).unwrap();
        let sb = page.write_property(b).unwrap();
        let sc = page.write_property(c).unwrap();
        assert_eq!(page.read_property(sa).unwrap(), a.as_ref());
        assert_eq!(page.read_property(sb).unwrap(), b.as_ref());
        assert_eq!(page.read_property(sc).unwrap(), c.as_ref());
    }

    #[test]
    fn page_full_returns_error() {
        let mut page = SlottedPage::new();
        // Write data that nearly fills the page.
        let big = vec![0u8; PAGE_SIZE - HEADER_SIZE - SLOT_ENTRY_SIZE * 2 - 10];
        page.write_property(&big).unwrap();
        // Data larger than the remaining capacity must fail.
        let too_big = vec![0u8; 100];
        assert!(page.write_property(&too_big).is_err());
    }

    // ---- A-2: free_slot / live_count / tombstone-tolerant is_valid ----

    /// `free_slot` tombstones a slot, decrements `live_count`, and makes its read a distinct
    /// PropertySlotFreed; live neighbors are untouched, and the page stays valid (M-A).
    #[test]
    fn free_slot_tombstones_and_keeps_page_valid() {
        let mut page = SlottedPage::new();
        let a = page.write_property(b"alpha").unwrap();
        let b = page.write_property(b"beta").unwrap();
        let c = page.write_property(b"gamma").unwrap();
        assert_eq!(page.live_count(), 3);
        page.free_slot(b).unwrap();
        assert_eq!(page.live_count(), 2);
        // The whole page must still be valid (M-A: one tombstone must not fail is_valid()).
        assert!(page.is_valid());
        // The tombstoned slot reads as freed, distinct from corruption.
        assert!(matches!(
            page.read_property(b),
            Err(GraphError::PropertySlotFreed)
        ));
        // Live neighbors are intact.
        assert_eq!(page.read_property(a).unwrap(), b"alpha".as_ref());
        assert_eq!(page.read_property(c).unwrap(), b"gamma".as_ref());
    }

    /// A page with some live + some tombstoned slots still accepts a new write through the ordinary
    /// path (proves the allocator won't skip a partly-freed page — the point of M-A's fix).
    #[test]
    fn mixed_tombstone_page_still_writable() {
        let mut page = SlottedPage::new();
        let _a = page.write_property(b"alpha").unwrap();
        let b = page.write_property(b"beta").unwrap();
        page.free_slot(b).unwrap();
        assert!(page.is_valid());
        // A new value must still write (the page is not "full" or "invalid" just because of a hole).
        let d = page.write_property(b"delta").unwrap();
        assert_eq!(page.read_property(d).unwrap(), b"delta".as_ref());
        assert_eq!(page.live_count(), 2); // alpha + delta live, beta tombstoned
    }

    /// Freeing an already-freed slot is an idempotent no-op (O-5) and doesn't disturb live_count.
    #[test]
    fn double_free_slot_is_noop() {
        let mut page = SlottedPage::new();
        let a = page.write_property(b"alpha").unwrap();
        page.free_slot(a).unwrap();
        assert_eq!(page.live_count(), 0);
        page.free_slot(a).unwrap(); // second free: no-op, no underflow
        assert_eq!(page.live_count(), 0);
    }

    /// A page carrying the FREE_PAGE_MARKER in slot_count is not a live slotted page: is_valid()
    /// rejects it so the ordinary allocator scan skips free-listed pages (design §3.2 invariant).
    #[test]
    fn free_page_marker_fails_is_valid() {
        let mut data = [0u8; PAGE_SIZE];
        data[0..2].copy_from_slice(&FREE_PAGE_MARKER.to_le_bytes());
        let page = SlottedPage::from_bytes(data);
        assert!(!page.is_valid());
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(50))]
        #[test]
        fn random_properties_no_boundary_violation(
            items in proptest::collection::vec(
                proptest::collection::vec(any::<u8>(), 0..200usize),
                1..30usize,
            )
        ) {
            let mut page = SlottedPage::new();
            let mut written: Vec<(SlotId, Vec<u8>)> = Vec::new();

            for item in &items {
                match page.write_property(item) {
                    Ok(slot) => written.push((slot, item.clone())),
                    Err(_) => break, // Stop when the page is full.
                }
            }

            // All written data must be readable back correctly.
            for (slot, expected) in &written {
                let actual = page.read_property(*slot).unwrap();
                prop_assert_eq!(actual, expected.as_slice());
            }

            // The slot directory and data area must not exceed the page boundary.
            let count = page.slot_count() as usize;
            let dir_end = HEADER_SIZE + count * SLOT_ENTRY_SIZE;
            let data_start = page.data_end();
            prop_assert!(dir_end <= data_start, "directory overlaps data area");
        }
    }

    // ---- BlobChain tests ----

    #[test]
    fn small_blob_single_page() {
        let data = b"small blob data";
        let needed = pages_needed(data.len());
        assert_eq!(needed, 1);
        let mut pages = vec![[0u8; PAGE_SIZE]; needed];
        write_blob_chain(&mut pages, data).unwrap();
        let result = read_blob_chain(&pages).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn large_blob_multi_page() {
        let data: Vec<u8> = (0..u8::MAX).cycle().take(PAGE_SIZE * 3).collect();
        let needed = pages_needed(data.len());
        assert!(needed > 1);
        let mut pages = vec![[0u8; PAGE_SIZE]; needed];
        write_blob_chain(&mut pages, &data).unwrap();
        let result = read_blob_chain(&pages).unwrap();
        assert_eq!(result, data);
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(50))]
        #[test]
        fn random_blob_roundtrip(
            data in proptest::collection::vec(any::<u8>(), 0..PAGE_SIZE * 4)
        ) {
            let needed = pages_needed(data.len()).max(1);
            let mut pages = vec![[0u8; PAGE_SIZE]; needed];
            write_blob_chain(&mut pages, &data).unwrap();
            let result = read_blob_chain(&pages).unwrap();
            prop_assert_eq!(result, data);
        }
    }
}
