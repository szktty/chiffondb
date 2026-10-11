use crate::storage::page::PAGE_SIZE;
use std::collections::HashMap;

/// Sentinel for "no neighbour" in the intrusive LRU list.
const NIL: usize = usize::MAX;

/// A single cached page frame, linked into the LRU list by slab index.
struct Frame {
    page_id: u32,
    data: [u8; PAGE_SIZE],
    dirty: bool,
    /// Neighbour towards the least-recently-used end (`NIL` at the LRU end).
    prev: usize,
    /// Neighbour towards the most-recently-used end (`NIL` at the MRU end).
    next: usize,
}

/// A dirty page evicted from the cache that the caller must persist before dropping.
pub struct DirtyVictim {
    pub page_id: u32,
    pub data: [u8; PAGE_SIZE],
}

/// Bounded LRU page cache (modeled on SQLite's pcache).
///
/// Holds at most `capacity` pages of data in memory, both clean and dirty.
/// Eviction policy:
/// - A clean victim is simply dropped (it can be re-read from the WAL or main file).
/// - A dirty victim is returned to the caller via [`DirtyVictim`] so it can be
///   written to the WAL *before* its bytes leave memory. This preserves the
///   durability invariant: a dirty page is never lost on eviction.
///
/// The cache itself performs no I/O; coordination with the WAL/main file is the
/// caller's responsibility. This keeps the cache decoupled from the storage backend.
///
/// Current usage note: the file backend is write-through (every write is appended to
/// the WAL before caching), so it always inserts pages with `dirty = false`. As a
/// result the dirty-victim path below never fires today. The dirty/`DirtyVictim`
/// machinery is retained for a future write-back mode (e.g. when topology/index pages
/// are routed through this cache — see `docs/plan-page-cache.md` Phase 2).
pub struct PageCache {
    capacity: usize,
    /// Frame storage; a slot is reused after eviction/removal via `free`.
    slab: Vec<Frame>,
    /// Slab indices of vacated frames.
    free: Vec<usize>,
    /// page id → slab index of its frame.
    index: HashMap<u32, usize>,
    /// Least-recently-used frame (`NIL` when empty).
    lru_head: usize,
    /// Most-recently-used frame (`NIL` when empty).
    mru_tail: usize,
}

impl PageCache {
    pub fn new(capacity: usize) -> Self {
        // A capacity of zero would make every insert evict itself; clamp to 1.
        let capacity = capacity.max(1);
        Self {
            capacity,
            slab: Vec::new(),
            free: Vec::new(),
            index: HashMap::new(),
            lru_head: NIL,
            mru_tail: NIL,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Returns a copy of the cached page if present, marking it most-recently-used.
    pub fn get(&mut self, page_id: u32) -> Option<[u8; PAGE_SIZE]> {
        let slot = *self.index.get(&page_id)?;
        self.touch(slot);
        Some(self.slab[slot].data)
    }

    /// Inserts or updates a page. `dirty` marks whether the page differs from the
    /// persisted (WAL/main file) copy.
    ///
    /// If the insertion exceeds capacity, the least-recently-used page is evicted.
    /// When the evicted page is dirty, it is returned so the caller can persist it
    /// to the WAL before the data is discarded. A clean eviction returns `None`.
    #[must_use = "a returned DirtyVictim must be written to the WAL to avoid data loss"]
    pub fn put(&mut self, page_id: u32, data: [u8; PAGE_SIZE], dirty: bool) -> Option<DirtyVictim> {
        if let Some(&slot) = self.index.get(&page_id) {
            let frame = &mut self.slab[slot];
            frame.data = data;
            // Once dirty, a page stays dirty until checkpoint/eviction clears it.
            frame.dirty = frame.dirty || dirty;
            self.touch(slot);
            return None;
        }

        let victim = if self.index.len() >= self.capacity {
            self.evict_lru()
        } else {
            None
        };

        let frame = Frame {
            page_id,
            data,
            dirty,
            prev: NIL,
            next: NIL,
        };
        let slot = match self.free.pop() {
            Some(slot) => {
                self.slab[slot] = frame;
                slot
            }
            None => {
                self.slab.push(frame);
                self.slab.len() - 1
            }
        };
        self.index.insert(page_id, slot);
        self.push_mru(slot);
        victim
    }

    /// Drops the cached frame for a page (e.g. after checkpoint persisted it).
    /// Does not return a victim; the caller asserts the page is already durable.
    pub fn remove(&mut self, page_id: u32) {
        if let Some(slot) = self.index.remove(&page_id) {
            self.unlink(slot);
            self.free.push(slot);
        }
    }

    /// Clears all cached frames (e.g. after checkpoint or rollback).
    pub fn clear(&mut self) {
        self.slab.clear();
        self.free.clear();
        self.index.clear();
        self.lru_head = NIL;
        self.mru_tail = NIL;
    }

    /// Evicts the least-recently-used page. Returns it as a [`DirtyVictim`] when dirty.
    fn evict_lru(&mut self) -> Option<DirtyVictim> {
        let slot = self.lru_head;
        if slot == NIL {
            return None;
        }
        self.unlink(slot);
        self.free.push(slot);
        let frame = &self.slab[slot];
        self.index.remove(&frame.page_id);
        if frame.dirty {
            Some(DirtyVictim {
                page_id: frame.page_id,
                data: frame.data,
            })
        } else {
            None
        }
    }

    /// Moves the accessed frame to the most-recently-used position.
    fn touch(&mut self, slot: usize) {
        if self.mru_tail != slot {
            self.unlink(slot);
            self.push_mru(slot);
        }
    }

    fn unlink(&mut self, slot: usize) {
        let (prev, next) = (self.slab[slot].prev, self.slab[slot].next);
        if prev == NIL {
            self.lru_head = next;
        } else {
            self.slab[prev].next = next;
        }
        if next == NIL {
            self.mru_tail = prev;
        } else {
            self.slab[next].prev = prev;
        }
        self.slab[slot].prev = NIL;
        self.slab[slot].next = NIL;
    }

    fn push_mru(&mut self, slot: usize) {
        self.slab[slot].prev = self.mru_tail;
        self.slab[slot].next = NIL;
        if self.mru_tail == NIL {
            self.lru_head = slot;
        } else {
            let tail = self.mru_tail;
            self.slab[tail].next = slot;
        }
        self.mru_tail = slot;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(byte: u8) -> [u8; PAGE_SIZE] {
        let mut p = [0u8; PAGE_SIZE];
        p[0] = byte;
        p
    }

    #[test]
    fn get_returns_inserted_page() {
        let mut cache = PageCache::new(4);
        assert!(cache.put(1, page(42), false).is_none());
        assert_eq!(cache.get(1).unwrap()[0], 42);
        assert!(cache.get(99).is_none());
    }

    #[test]
    fn clean_eviction_returns_no_victim() {
        let mut cache = PageCache::new(2);
        assert!(cache.put(0, page(0), false).is_none());
        assert!(cache.put(1, page(1), false).is_none());
        // capacity=2, inserting a third clean page evicts the LRU (page 0) with no victim.
        let victim = cache.put(2, page(2), false);
        assert!(victim.is_none());
        assert!(cache.get(0).is_none());
        assert!(cache.get(1).is_some());
        assert!(cache.get(2).is_some());
    }

    #[test]
    fn dirty_eviction_returns_victim() {
        let mut cache = PageCache::new(2);
        assert!(cache.put(0, page(0xAA), true).is_none());
        assert!(cache.put(1, page(0xBB), false).is_none());
        // Inserting page 2 evicts dirty page 0; it must come back as a victim.
        let victim = cache.put(2, page(0xCC), false).expect("dirty victim");
        assert_eq!(victim.page_id, 0);
        assert_eq!(victim.data[0], 0xAA);
    }

    #[test]
    fn touch_on_get_updates_lru() {
        let mut cache = PageCache::new(2);
        let _ = cache.put(0, page(0), false);
        let _ = cache.put(1, page(1), false);
        // Access page 0 so page 1 becomes the LRU.
        let _ = cache.get(0);
        let _ = cache.put(2, page(2), false);
        assert!(
            cache.get(0).is_some(),
            "page 0 was recently used, should stay"
        );
        assert!(cache.get(1).is_none(), "page 1 was LRU, should be evicted");
    }

    #[test]
    fn put_existing_keeps_dirty_flag() {
        let mut cache = PageCache::new(2);
        let _ = cache.put(0, page(1), true); // dirty
        let _ = cache.put(1, page(2), false);
        // Re-put page 0 as clean; it must remain dirty (sticky until persisted).
        assert!(cache.put(0, page(3), false).is_none());
        // Force eviction of page 0 by exceeding capacity after touching page 1.
        let _ = cache.get(1);
        let victim = cache.put(2, page(4), false).expect("page 0 still dirty");
        assert_eq!(victim.page_id, 0);
        assert_eq!(victim.data[0], 3);
    }

    #[test]
    fn remove_frees_a_slot_and_keeps_lru_order() {
        let mut cache = PageCache::new(3);
        let _ = cache.put(0, page(0), false);
        let _ = cache.put(1, page(1), false);
        let _ = cache.put(2, page(2), false);
        cache.remove(1);
        cache.remove(1);
        assert_eq!(cache.len(), 2);
        assert!(cache.get(1).is_none());
        // Room for one more without eviction; then page 0 is the LRU.
        let _ = cache.put(3, page(3), false);
        assert_eq!(cache.len(), 3);
        let _ = cache.put(4, page(4), false);
        assert!(cache.get(0).is_none());
        assert_eq!(cache.get(2).unwrap()[0], 2);
        assert_eq!(cache.get(3).unwrap()[0], 3);
        assert_eq!(cache.get(4).unwrap()[0], 4);
    }

    #[test]
    fn clear_empties_the_cache_and_it_stays_usable() {
        let mut cache = PageCache::new(2);
        let _ = cache.put(0, page(0), false);
        let _ = cache.put(1, page(1), false);
        cache.clear();
        assert!(cache.is_empty());
        assert!(cache.get(0).is_none());
        let _ = cache.put(5, page(5), false);
        let _ = cache.put(6, page(6), false);
        let _ = cache.put(7, page(7), false);
        assert_eq!(cache.len(), 2);
        assert!(cache.get(5).is_none());
        assert_eq!(cache.get(7).unwrap()[0], 7);
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(50))]
        /// Random get/put/remove ops evict exactly what a naive recency list would.
        #[test]
        fn matches_naive_lru_model(
            capacity in 1usize..6,
            ops in proptest::collection::vec((0u8..3, 0u32..10), 1..300),
        ) {
            let mut cache = PageCache::new(capacity);
            // Front = least-recently-used.
            let mut model: Vec<u32> = Vec::new();
            for (op, id) in ops {
                match op {
                    0 => {
                        let hit = cache.get(id).is_some();
                        proptest::prop_assert_eq!(hit, model.contains(&id));
                        if hit {
                            model.retain(|&m| m != id);
                            model.push(id);
                        }
                    }
                    1 => {
                        let _ = cache.put(id, page(id as u8), false);
                        if model.contains(&id) {
                            model.retain(|&m| m != id);
                        } else if model.len() >= capacity {
                            model.remove(0);
                        }
                        model.push(id);
                    }
                    _ => {
                        cache.remove(id);
                        model.retain(|&m| m != id);
                    }
                }
                proptest::prop_assert_eq!(cache.len(), model.len());
            }
            // Drain by eviction order: filling with fresh ids must push out the model's order.
            for (i, expected) in model.clone().into_iter().enumerate() {
                if model.len() < capacity {
                    break;
                }
                let _ = cache.put(1000 + i as u32, page(0), false);
                proptest::prop_assert!(cache.get(expected).is_none());
            }
        }
    }

    #[test]
    fn capacity_is_respected() {
        let mut cache = PageCache::new(3);
        for i in 0..10u32 {
            let _ = cache.put(i, page(i as u8), false);
        }
        assert!(cache.len() <= 3);
    }
}
