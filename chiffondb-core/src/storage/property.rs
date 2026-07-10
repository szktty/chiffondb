use crate::error::GraphError;
use crate::storage::page::{SlotId, PAGE_SIZE};

/// Header size of a slotted page (slot_count: u16).
const HEADER_SIZE: usize = 2;
/// Size of one entry in the slot directory (offset: u16 + length: u16).
const SLOT_ENTRY_SIZE: usize = 4;

/// Slotted-page operations.
///
/// Layout:
///   [0..2]   slot_count (u16, little-endian)
///   [2..]    slot directory (4 bytes each: offset u16 + length u16)
///   data area packed from the end toward the header
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

    /// Performs a basic validity check for this slotted page.
    /// Guards against accidentally reading pages with a different format, such as blob-chain pages.
    pub fn is_valid(&self) -> bool {
        let count = self.slot_count() as usize;
        let dir_end = HEADER_SIZE + count * SLOT_ENTRY_SIZE;
        if dir_end > PAGE_SIZE {
            return false;
        }
        // Verify that each slot's offset falls within the page bounds.
        for i in 0..count {
            let dir_offset = HEADER_SIZE + i * SLOT_ENTRY_SIZE;
            let offset =
                u16::from_le_bytes(self.data[dir_offset..dir_offset + 2].try_into().unwrap())
                    as usize;
            let length = u16::from_le_bytes(
                self.data[dir_offset + 2..dir_offset + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            if offset + length > PAGE_SIZE {
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
        Ok(SlotId(slot_id))
    }

    /// Reads data from a slot.
    pub fn read_property(&self, slot: SlotId) -> Result<&[u8], GraphError> {
        let idx = slot.0 as usize;
        if idx >= self.slot_count() as usize {
            return Err(GraphError::StorageCorrupted(0));
        }
        let dir_offset = HEADER_SIZE + idx * SLOT_ENTRY_SIZE;
        let offset =
            u16::from_le_bytes(self.data[dir_offset..dir_offset + 2].try_into().unwrap()) as usize;
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
