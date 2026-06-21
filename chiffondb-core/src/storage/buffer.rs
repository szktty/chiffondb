use crate::storage::page::PAGE_SIZE;
use std::collections::HashMap;
use std::collections::VecDeque;

/// A single cached page frame.
struct Frame {
    data: [u8; PAGE_SIZE],
    dirty: bool,
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
    frames: HashMap<u32, Frame>,
    /// LRU order; front = least-recently-used, back = most-recently-used.
    lru: VecDeque<u32>,
}

impl PageCache {
    pub fn new(capacity: usize) -> Self {
        // A capacity of zero would make every insert evict itself; clamp to 1.
        let capacity = capacity.max(1);
        Self {
            capacity,
            frames: HashMap::new(),
            lru: VecDeque::new(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Returns a copy of the cached page if present, marking it most-recently-used.
    pub fn get(&mut self, page_id: u32) -> Option<[u8; PAGE_SIZE]> {
        if let Some(frame) = self.frames.get(&page_id) {
            let data = frame.data;
            self.touch(page_id);
            Some(data)
        } else {
            None
        }
    }

    /// Inserts or updates a page. `dirty` marks whether the page differs from the
    /// persisted (WAL/main file) copy.
    ///
    /// If the insertion exceeds capacity, the least-recently-used page is evicted.
    /// When the evicted page is dirty, it is returned so the caller can persist it
    /// to the WAL before the data is discarded. A clean eviction returns `None`.
    #[must_use = "a returned DirtyVictim must be written to the WAL to avoid data loss"]
    pub fn put(&mut self, page_id: u32, data: [u8; PAGE_SIZE], dirty: bool) -> Option<DirtyVictim> {
        if let Some(frame) = self.frames.get_mut(&page_id) {
            frame.data = data;
            // Once dirty, a page stays dirty until checkpoint/eviction clears it.
            frame.dirty = frame.dirty || dirty;
            self.touch(page_id);
            return None;
        }

        let victim = if self.frames.len() >= self.capacity {
            self.evict_lru()
        } else {
            None
        };

        self.frames.insert(page_id, Frame { data, dirty });
        self.lru.push_back(page_id);
        victim
    }

    /// Drops the cached frame for a page (e.g. after checkpoint persisted it).
    /// Does not return a victim; the caller asserts the page is already durable.
    pub fn remove(&mut self, page_id: u32) {
        if self.frames.remove(&page_id).is_some() {
            self.lru.retain(|&id| id != page_id);
        }
    }

    /// Clears all cached frames (e.g. after checkpoint or rollback).
    pub fn clear(&mut self) {
        self.frames.clear();
        self.lru.clear();
    }

    /// Evicts the least-recently-used page. Returns it as a [`DirtyVictim`] when dirty.
    fn evict_lru(&mut self) -> Option<DirtyVictim> {
        while let Some(victim_id) = self.lru.pop_front() {
            if let Some(frame) = self.frames.remove(&victim_id) {
                return if frame.dirty {
                    Some(DirtyVictim {
                        page_id: victim_id,
                        data: frame.data,
                    })
                } else {
                    None
                };
            }
            // Stale LRU entry (frame already removed); keep scanning.
        }
        None
    }

    /// Moves the accessed page to the most-recently-used position.
    fn touch(&mut self, page_id: u32) {
        if let Some(pos) = self.lru.iter().position(|&id| id == page_id) {
            self.lru.remove(pos);
        }
        self.lru.push_back(page_id);
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
    fn capacity_is_respected() {
        let mut cache = PageCache::new(3);
        for i in 0..10u32 {
            let _ = cache.put(i, page(i as u8), false);
        }
        assert!(cache.len() <= 3);
    }
}
