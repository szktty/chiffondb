use crate::error::GraphError;

pub const PAGE_SIZE: usize = 4096;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PageId(pub u32);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SlotId(pub u16);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct RecordId {
    pub page_id: PageId,
    pub slot_id: SlotId,
}

impl RecordId {
    pub fn new(page_id: u32, slot_id: u16) -> Self {
        Self {
            page_id: PageId(page_id),
            slot_id: SlotId(slot_id),
        }
    }
}

/// Typed wrapper for a node record ID. Prevents accidental use of an edge RID as a node RID.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeRid(pub(crate) RecordId);

impl NodeRid {
    pub fn new(page_id: u32, slot_id: u16) -> Self {
        Self(RecordId::new(page_id, slot_id))
    }

    pub fn page_id(self) -> PageId {
        self.0.page_id
    }

    pub fn slot_id(self) -> SlotId {
        self.0.slot_id
    }
}

impl From<NodeRid> for RecordId {
    fn from(n: NodeRid) -> Self {
        n.0
    }
}

/// Typed wrapper for an edge record ID. Prevents accidental use of a node RID as an edge RID.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct EdgeRid(pub(crate) RecordId);

impl EdgeRid {
    pub fn new(page_id: u32, slot_id: u16) -> Self {
        Self(RecordId::new(page_id, slot_id))
    }

    pub fn page_id(self) -> PageId {
        self.0.page_id
    }

    pub fn slot_id(self) -> SlotId {
        self.0.slot_id
    }
}

impl From<EdgeRid> for RecordId {
    fn from(e: EdgeRid) -> Self {
        e.0
    }
}

/// Number of bytes needed for the bitmap (slot_count / 8, rounded up).
const fn bitmap_bytes(slot_count: usize) -> usize {
    slot_count.div_ceil(8)
}

/// A fixed-size 4 KB page. The bitmap occupies the header, followed by slot data.
#[derive(Clone)]
pub struct Page {
    data: [u8; PAGE_SIZE],
    slot_size: usize,
    slot_count: usize,
}

impl Page {
    /// Creates a page with slots of `slot_size` bytes each.
    pub fn new(slot_size: usize) -> Self {
        assert!(slot_size > 0, "slot_size must be positive");
        let slot_count = Self::compute_slot_count(slot_size);
        Self {
            data: [0u8; PAGE_SIZE],
            slot_size,
            slot_count,
        }
    }

    /// Restores a page from an existing byte array.
    pub fn from_bytes(bytes: [u8; PAGE_SIZE], slot_size: usize) -> Self {
        assert!(slot_size > 0);
        let slot_count = Self::compute_slot_count(slot_size);
        Self {
            data: bytes,
            slot_size,
            slot_count,
        }
    }

    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.data
    }

    pub fn slot_count(&self) -> usize {
        self.slot_count
    }

    /// Allocates one free slot and returns its slot number.
    pub fn alloc_slot(&mut self) -> Option<SlotId> {
        for i in 0..self.slot_count {
            if !self.is_used(SlotId(i as u16)) {
                self.set_bitmap(i, true);
                return Some(SlotId(i as u16));
            }
        }
        None
    }

    /// Frees a slot.
    pub fn free_slot(&mut self, slot: SlotId) -> Result<(), GraphError> {
        let idx = slot.0 as usize;
        if idx >= self.slot_count {
            return Err(GraphError::StorageCorrupted(0));
        }
        self.set_bitmap(idx, false);
        Ok(())
    }

    /// Returns whether the given slot is currently in use.
    pub fn is_used(&self, slot: SlotId) -> bool {
        let idx = slot.0 as usize;
        if idx >= self.slot_count {
            return false;
        }
        let byte = idx / 8;
        let bit = idx % 8;
        (self.data[byte] >> bit) & 1 == 1
    }

    /// Writes data to a slot. `data` must be exactly `slot_size` bytes.
    pub fn write_slot(&mut self, slot: SlotId, data: &[u8]) -> Result<(), GraphError> {
        if data.len() != self.slot_size {
            return Err(GraphError::StorageCorrupted(0));
        }
        let idx = slot.0 as usize;
        if idx >= self.slot_count {
            return Err(GraphError::StorageCorrupted(0));
        }
        let offset = self.slot_offset(idx);
        self.data[offset..offset + self.slot_size].copy_from_slice(data);
        Ok(())
    }

    /// Reads data from a slot.
    pub fn read_slot(&self, slot: SlotId) -> Result<&[u8], GraphError> {
        let idx = slot.0 as usize;
        if idx >= self.slot_count {
            return Err(GraphError::StorageCorrupted(0));
        }
        let offset = self.slot_offset(idx);
        Ok(&self.data[offset..offset + self.slot_size])
    }

    fn compute_slot_count(slot_size: usize) -> usize {
        // Find the maximum number of slots such that both the bitmap and slot data
        // fit within the page: bitmap_bytes(n) + n * slot_size <= PAGE_SIZE.
        let mut n = PAGE_SIZE / (slot_size + 1);
        while bitmap_bytes(n) + n * slot_size > PAGE_SIZE {
            n -= 1;
        }
        n
    }

    fn slot_offset(&self, idx: usize) -> usize {
        bitmap_bytes(self.slot_count) + idx * self.slot_size
    }

    fn set_bitmap(&mut self, idx: usize, used: bool) {
        let byte = idx / 8;
        let bit = idx % 8;
        if used {
            self.data[byte] |= 1 << bit;
        } else {
            self.data[byte] &= !(1 << bit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(50))]
        #[test]
        fn bitmap_consistent_with_alloc_free(
            ops in proptest::collection::vec(any::<bool>(), 1..200usize)
        ) {
            let slot_size = 64usize;
            let mut page = Page::new(slot_size);
            let mut allocated: std::collections::HashSet<u16> = std::collections::HashSet::new();

            for alloc in ops {
                if alloc || allocated.is_empty() {
                    if let Some(slot) = page.alloc_slot() {
                        allocated.insert(slot.0);
                    }
                } else {
                    // Free one of the already-allocated slots.
                    let &target = allocated.iter().next().unwrap();
                    page.free_slot(SlotId(target)).unwrap();
                    allocated.remove(&target);
                }
            }

            // Verify that the bitmap matches the allocated set.
            for i in 0..page.slot_count() {
                let slot = SlotId(i as u16);
                assert_eq!(
                    page.is_used(slot),
                    allocated.contains(&(i as u16)),
                    "slot {} mismatch",
                    i
                );
            }
        }
    }

    #[test]
    fn write_and_read_slot() {
        let mut page = Page::new(64);
        let slot = page.alloc_slot().unwrap();
        let data = [0xABu8; 64];
        page.write_slot(slot, &data).unwrap();
        assert_eq!(page.read_slot(slot).unwrap(), &data);
    }

    #[test]
    fn alloc_fills_page_then_returns_none() {
        let mut page = Page::new(64);
        let count = page.slot_count();
        for _ in 0..count {
            assert!(page.alloc_slot().is_some());
        }
        assert!(page.alloc_slot().is_none());
    }

    #[test]
    fn free_slot_makes_it_reusable() {
        let mut page = Page::new(64);
        let slot = page.alloc_slot().unwrap();
        page.free_slot(slot).unwrap();
        assert!(!page.is_used(slot));
        let slot2 = page.alloc_slot().unwrap();
        assert_eq!(slot, slot2);
    }
}
