use crate::error::GraphError;
use crate::storage::file::DatabaseFile;
use crate::storage::page::{Page, RecordId, SlotId, PAGE_SIZE};
use crate::storage::record::{EdgeRecord, NodeRecord, EDGE_RECORD_SIZE, NODE_RECORD_SIZE};

/// File-backed topology store.
///
/// Node and edge record pages live in the database file's topology segment and are
/// read/written through `DatabaseFile`, which fronts them with its bounded page cache.
/// The store itself keeps only O(1) metadata (segment start + logical page counts);
/// it never holds the record pages resident, so topology memory does not grow with
/// node/edge count (see `docs/plan-page-cache.md` Phase 2).
///
/// Logical→physical page mapping within the topology segment (node/edge interleaved):
///   node logical page i → file page `topo_start + i*2`
///   edge logical page i → file page `topo_start + i*2 + 1`
///
/// Slot occupancy is tracked solely by each page's own bitmap (`Page::is_used` /
/// `alloc_slot` / `free_slot`); there is no separate in-memory occupancy index.
#[derive(Clone)]
pub struct TopologyStore {
    topo_start: u32,
    /// First file page after the topology segment (= property_segment_start).
    /// The topology segment is the fixed range `[topo_start, seg_end)`.
    seg_end: u32,
    node_page_count: usize,
    edge_page_count: usize,
}

impl TopologyStore {
    /// Creates a store for a fresh database (one node page and one edge page).
    /// `topo_start`/`seg_end` bound the fixed topology segment.
    pub fn new(topo_start: u32, seg_end: u32) -> Self {
        Self {
            topo_start,
            seg_end,
            node_page_count: 1,
            edge_page_count: 1,
        }
    }

    /// Creates a store for an existing database with known logical page counts.
    pub fn with_counts(
        topo_start: u32,
        seg_end: u32,
        node_page_count: usize,
        edge_page_count: usize,
    ) -> Self {
        Self {
            topo_start,
            seg_end,
            node_page_count: node_page_count.max(1),
            edge_page_count: edge_page_count.max(1),
        }
    }

    /// Capacity (in logical pages) of each interleaved stream within the segment.
    fn node_capacity(&self) -> usize {
        (self.seg_end - self.topo_start).div_ceil(2) as usize
    }

    fn edge_capacity(&self) -> usize {
        ((self.seg_end - self.topo_start) / 2) as usize
    }

    fn node_pid(&self, logical: usize) -> u32 {
        self.topo_start + (logical as u32) * 2
    }

    fn edge_pid(&self, logical: usize) -> u32 {
        self.topo_start + (logical as u32) * 2 + 1
    }

    fn load_node_page(&self, file: &mut DatabaseFile, logical: usize) -> Result<Page, GraphError> {
        let bytes = file.read_page(self.node_pid(logical))?;
        Ok(Page::from_bytes(bytes, NODE_RECORD_SIZE))
    }

    fn load_edge_page(&self, file: &mut DatabaseFile, logical: usize) -> Result<Page, GraphError> {
        let bytes = file.read_page(self.edge_pid(logical))?;
        Ok(Page::from_bytes(bytes, EDGE_RECORD_SIZE))
    }

    /// Ensures file page `pid` exists (appends zeroed pages as needed).
    fn ensure_page(file: &mut DatabaseFile, pid: u32) -> Result<(), GraphError> {
        let empty = [0u8; PAGE_SIZE];
        while file.page_count()? <= pid {
            file.append_page(&empty)?;
        }
        Ok(())
    }

    // ---- Counts / page access ----

    pub fn node_page_count(&self) -> usize {
        self.node_page_count
    }

    pub fn edge_page_count(&self) -> usize {
        self.edge_page_count
    }

    /// Reads node logical page `idx` as a `Page` (for slot-occupancy scans).
    pub fn node_page(&self, file: &mut DatabaseFile, idx: usize) -> Result<Page, GraphError> {
        self.load_node_page(file, idx)
    }

    /// Reads edge logical page `idx` as a `Page` (for slot-occupancy scans).
    pub fn edge_page(&self, file: &mut DatabaseFile, idx: usize) -> Result<Page, GraphError> {
        self.load_edge_page(file, idx)
    }

    /// Returns the RecordIds of all live (used) node slots, scanning each page once.
    pub fn live_node_rids(&self, file: &mut DatabaseFile) -> Result<Vec<RecordId>, GraphError> {
        let mut rids = Vec::new();
        for logical in 0..self.node_page_count {
            let page = self.load_node_page(file, logical)?;
            for slot_idx in 0..page.slot_count() {
                let slot = SlotId(slot_idx as u16);
                if page.is_used(slot) {
                    rids.push(RecordId::new(logical as u32, slot_idx as u16));
                }
            }
        }
        Ok(rids)
    }

    /// Returns the RecordIds of all live (used) edge slots, scanning each page once.
    pub fn live_edge_rids(&self, file: &mut DatabaseFile) -> Result<Vec<RecordId>, GraphError> {
        let mut rids = Vec::new();
        for logical in 0..self.edge_page_count {
            let page = self.load_edge_page(file, logical)?;
            for slot_idx in 0..page.slot_count() {
                let slot = SlotId(slot_idx as u16);
                if page.is_used(slot) {
                    rids.push(RecordId::new(logical as u32, slot_idx as u16));
                }
            }
        }
        Ok(rids)
    }

    /// Returns whether the given node slot is currently used.
    pub fn node_slot_used(&self, file: &mut DatabaseFile, rid: RecordId) -> bool {
        let logical = rid.page_id.0 as usize;
        if logical >= self.node_page_count {
            return false;
        }
        self.load_node_page(file, logical)
            .map(|p| p.is_used(rid.slot_id))
            .unwrap_or(false)
    }

    /// Returns whether the given edge slot is currently used.
    pub fn edge_slot_used(&self, file: &mut DatabaseFile, rid: RecordId) -> bool {
        let logical = rid.page_id.0 as usize;
        if logical >= self.edge_page_count {
            return false;
        }
        self.load_edge_page(file, logical)
            .map(|p| p.is_used(rid.slot_id))
            .unwrap_or(false)
    }

    // ---- Node operations ----

    pub fn alloc_node(
        &mut self,
        file: &mut DatabaseFile,
        node_type_id: u16,
    ) -> Result<RecordId, GraphError> {
        let (logical, slot) = self.alloc_node_slot(file)?;
        let rid = RecordId::new(logical as u32, slot.0);
        let node = NodeRecord {
            id: rid,
            node_type_id,
            first_out_edge: None,
            first_in_edge: None,
            property_ref: None,
            label_ref: None,
            flags: 0,
        };
        self.write_node(file, &node)?;
        Ok(rid)
    }

    pub fn read_node(
        &self,
        file: &mut DatabaseFile,
        rid: RecordId,
    ) -> Result<NodeRecord, GraphError> {
        let logical = rid.page_id.0 as usize;
        if logical >= self.node_page_count {
            return Err(GraphError::StorageCorrupted(rid.page_id.0));
        }
        let page = self.load_node_page(file, logical)?;
        let bytes = page.read_slot(rid.slot_id)?;
        NodeRecord::deserialize(bytes.try_into().unwrap())
    }

    pub fn write_node(
        &mut self,
        file: &mut DatabaseFile,
        node: &NodeRecord,
    ) -> Result<(), GraphError> {
        let logical = node.id.page_id.0 as usize;
        if logical >= self.node_page_count {
            return Err(GraphError::StorageCorrupted(node.id.page_id.0));
        }
        let mut page = self.load_node_page(file, logical)?;
        page.write_slot(node.id.slot_id, &node.serialize())?;
        file.write_page(self.node_pid(logical), page.as_bytes())
    }

    // ---- Edge operations ----

    pub fn alloc_edge(
        &mut self,
        file: &mut DatabaseFile,
        edge_type_id: u16,
        from_node: RecordId,
        to_node: RecordId,
    ) -> Result<RecordId, GraphError> {
        let (logical, slot) = self.alloc_edge_slot(file)?;
        let rid = RecordId::new(logical as u32, slot.0);
        let edge = EdgeRecord {
            id: rid,
            edge_type_id,
            from_node,
            to_node,
            next_out_edge: None,
            next_in_edge: None,
            property_ref: None,
            flags: 0,
            label_ref: None,
        };
        self.write_edge(file, &edge)?;
        Ok(rid)
    }

    pub fn read_edge(
        &self,
        file: &mut DatabaseFile,
        rid: RecordId,
    ) -> Result<EdgeRecord, GraphError> {
        let logical = rid.page_id.0 as usize;
        if logical >= self.edge_page_count {
            return Err(GraphError::StorageCorrupted(rid.page_id.0));
        }
        let page = self.load_edge_page(file, logical)?;
        let bytes = page.read_slot(rid.slot_id)?;
        EdgeRecord::deserialize(bytes.try_into().unwrap())
    }

    pub fn write_edge(
        &mut self,
        file: &mut DatabaseFile,
        edge: &EdgeRecord,
    ) -> Result<(), GraphError> {
        let logical = edge.id.page_id.0 as usize;
        if logical >= self.edge_page_count {
            return Err(GraphError::StorageCorrupted(edge.id.page_id.0));
        }
        let mut page = self.load_edge_page(file, logical)?;
        page.write_slot(edge.id.slot_id, &edge.serialize())?;
        file.write_page(self.edge_pid(logical), page.as_bytes())
    }

    // ---- Topology operations ----

    /// Adds an outgoing edge to a node (inserted at the head of the adjacency list).
    pub fn append_out_edge(
        &mut self,
        file: &mut DatabaseFile,
        node_rid: RecordId,
        edge_rid: RecordId,
    ) -> Result<(), GraphError> {
        let mut node = self.read_node(file, node_rid)?;
        let mut edge = self.read_edge(file, edge_rid)?;
        edge.next_out_edge = node.first_out_edge;
        node.first_out_edge = Some(edge_rid);
        self.write_edge(file, &edge)?;
        self.write_node(file, &node)
    }

    /// Adds an incoming edge to a node (inserted at the head of the adjacency list).
    pub fn append_in_edge(
        &mut self,
        file: &mut DatabaseFile,
        node_rid: RecordId,
        edge_rid: RecordId,
    ) -> Result<(), GraphError> {
        let mut node = self.read_node(file, node_rid)?;
        let mut edge = self.read_edge(file, edge_rid)?;
        edge.next_in_edge = node.first_in_edge;
        node.first_in_edge = Some(edge_rid);
        self.write_edge(file, &edge)?;
        self.write_node(file, &node)
    }

    /// Deletes an edge by bypassing it from both the outgoing and incoming edge lists.
    pub fn delete_edge(
        &mut self,
        file: &mut DatabaseFile,
        edge_rid: RecordId,
    ) -> Result<(), GraphError> {
        let edge = self.read_edge(file, edge_rid)?;
        let next_out = edge.next_out_edge;
        let next_in = edge.next_in_edge;
        let from = edge.from_node;
        let to = edge.to_node;

        self.remove_from_out_list(file, from, edge_rid, next_out)?;
        self.remove_from_in_list(file, to, edge_rid, next_in)?;

        let logical = edge_rid.page_id.0 as usize;
        let mut page = self.load_edge_page(file, logical)?;
        page.free_slot(edge_rid.slot_id)?;
        file.write_page(self.edge_pid(logical), page.as_bytes())
    }

    /// Collects the outgoing edges of a node.
    ///
    /// Propagates errors rather than silently truncating the walk: a failed
    /// `read_node`/`read_edge` signals storage corruption or a broken pointer chain
    /// and must surface, not be misreported as "fewer edges".
    pub fn collect_out_edges(
        &self,
        file: &mut DatabaseFile,
        node_rid: RecordId,
    ) -> Result<Vec<EdgeRecord>, GraphError> {
        self.collect_edge_chain(file, node_rid, EdgeDirection::Out)
    }

    /// Collects the incoming edges of a node. See [`collect_out_edges`] for error semantics.
    pub fn collect_in_edges(
        &self,
        file: &mut DatabaseFile,
        node_rid: RecordId,
    ) -> Result<Vec<EdgeRecord>, GraphError> {
        self.collect_edge_chain(file, node_rid, EdgeDirection::In)
    }

    fn collect_edge_chain(
        &self,
        file: &mut DatabaseFile,
        node_rid: RecordId,
        direction: EdgeDirection,
    ) -> Result<Vec<EdgeRecord>, GraphError> {
        let node = self.read_node(file, node_rid)?;
        let mut next = match direction {
            EdgeDirection::Out => node.first_out_edge,
            EdgeDirection::In => node.first_in_edge,
        };
        let mut out = Vec::new();
        while let Some(rid) = next {
            let edge = self.read_edge(file, rid)?;
            next = match direction {
                EdgeDirection::Out => edge.next_out_edge,
                EdgeDirection::In => edge.next_in_edge,
            };
            out.push(edge);
        }
        Ok(out)
    }

    /// Frees the slot occupied by a node.
    pub fn free_node(&mut self, file: &mut DatabaseFile, rid: RecordId) -> Result<(), GraphError> {
        let logical = rid.page_id.0 as usize;
        let mut page = self.load_node_page(file, logical)?;
        page.free_slot(rid.slot_id)?;
        file.write_page(self.node_pid(logical), page.as_bytes())
    }

    // ---- Internal helpers ----

    /// Finds (or creates) a free node slot, returning its (logical page, slot).
    fn alloc_node_slot(&mut self, file: &mut DatabaseFile) -> Result<(usize, SlotId), GraphError> {
        for logical in 0..self.node_page_count {
            let mut page = self.load_node_page(file, logical)?;
            if let Some(slot) = page.alloc_slot() {
                file.write_page(self.node_pid(logical), page.as_bytes())?;
                return Ok((logical, slot));
            }
        }
        // All pages full: add a new logical node page (if the fixed segment allows it).
        let logical = self.node_page_count;
        if logical >= self.node_capacity() {
            return Err(GraphError::CapacityExceeded {
                kind: "node",
                needed: logical + 1,
                available: self.node_capacity(),
            });
        }
        Self::ensure_page(file, self.node_pid(logical))?;
        let mut page = Page::new(NODE_RECORD_SIZE);
        let slot = page
            .alloc_slot()
            .ok_or(GraphError::StorageCorrupted(self.node_pid(logical)))?;
        file.write_page(self.node_pid(logical), page.as_bytes())?;
        self.node_page_count += 1;
        // Persist the grown count through the WAL so it survives a crash without flush
        // (the new page would otherwise be unreachable after recovery).
        file.header.node_page_count = self.node_page_count as u32;
        file.write_header()?;
        Ok((logical, slot))
    }

    /// Finds (or creates) a free edge slot, returning its (logical page, slot).
    fn alloc_edge_slot(&mut self, file: &mut DatabaseFile) -> Result<(usize, SlotId), GraphError> {
        for logical in 0..self.edge_page_count {
            let mut page = self.load_edge_page(file, logical)?;
            if let Some(slot) = page.alloc_slot() {
                file.write_page(self.edge_pid(logical), page.as_bytes())?;
                return Ok((logical, slot));
            }
        }
        let logical = self.edge_page_count;
        if logical >= self.edge_capacity() {
            return Err(GraphError::CapacityExceeded {
                kind: "edge",
                needed: logical + 1,
                available: self.edge_capacity(),
            });
        }
        Self::ensure_page(file, self.edge_pid(logical))?;
        let mut page = Page::new(EDGE_RECORD_SIZE);
        let slot = page
            .alloc_slot()
            .ok_or(GraphError::StorageCorrupted(self.edge_pid(logical)))?;
        file.write_page(self.edge_pid(logical), page.as_bytes())?;
        self.edge_page_count += 1;
        file.header.edge_page_count = self.edge_page_count as u32;
        file.write_header()?;
        Ok((logical, slot))
    }

    fn remove_from_out_list(
        &mut self,
        file: &mut DatabaseFile,
        node_rid: RecordId,
        target: RecordId,
        target_next: Option<RecordId>,
    ) -> Result<(), GraphError> {
        let mut node = self.read_node(file, node_rid)?;
        if node.first_out_edge == Some(target) {
            node.first_out_edge = target_next;
            return self.write_node(file, &node);
        }
        let mut cur_opt = node.first_out_edge;
        while let Some(cur_rid) = cur_opt {
            let mut cur_edge = self.read_edge(file, cur_rid)?;
            if cur_edge.next_out_edge == Some(target) {
                cur_edge.next_out_edge = target_next;
                self.write_edge(file, &cur_edge)?;
                return Ok(());
            }
            cur_opt = cur_edge.next_out_edge;
        }
        Ok(())
    }

    fn remove_from_in_list(
        &mut self,
        file: &mut DatabaseFile,
        node_rid: RecordId,
        target: RecordId,
        target_next: Option<RecordId>,
    ) -> Result<(), GraphError> {
        let mut node = self.read_node(file, node_rid)?;
        if node.first_in_edge == Some(target) {
            node.first_in_edge = target_next;
            return self.write_node(file, &node);
        }
        let mut cur_opt = node.first_in_edge;
        while let Some(cur_rid) = cur_opt {
            let mut cur_edge = self.read_edge(file, cur_rid)?;
            if cur_edge.next_in_edge == Some(target) {
                cur_edge.next_in_edge = target_next;
                self.write_edge(file, &cur_edge)?;
                return Ok(());
            }
            cur_opt = cur_edge.next_in_edge;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum EdgeDirection {
    Out,
    In,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::{HashMap, HashSet};

    fn make_store() -> (TopologyStore, DatabaseFile) {
        // In-memory backend: topology segment starts at page 1, pre-allocate pages 1 and 2
        // (one node page, one edge page) so the initial logical pages exist.
        let mut file = DatabaseFile::create_in_memory().unwrap();
        let empty = [0u8; PAGE_SIZE];
        // Pre-allocate a small topology segment [1, 64) like Database::create does.
        for _ in 1..64 {
            file.append_page(&empty).unwrap();
        }
        (TopologyStore::new(1, 64), file)
    }

    #[test]
    fn append_and_collect_out_edges() {
        let (mut store, mut f) = make_store();
        let n1 = store.alloc_node(&mut f, 1).unwrap();
        let n2 = store.alloc_node(&mut f, 1).unwrap();
        let e1 = store.alloc_edge(&mut f, 1, n1, n2).unwrap();
        let e2 = store.alloc_edge(&mut f, 1, n1, n2).unwrap();
        store.append_out_edge(&mut f, n1, e1).unwrap();
        store.append_out_edge(&mut f, n1, e2).unwrap();

        let out_edges: HashSet<RecordId> = store
            .collect_out_edges(&mut f, n1)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(out_edges, HashSet::from([e1, e2]));
    }

    #[test]
    fn append_and_collect_in_edges() {
        let (mut store, mut f) = make_store();
        let n1 = store.alloc_node(&mut f, 1).unwrap();
        let n2 = store.alloc_node(&mut f, 1).unwrap();
        let e1 = store.alloc_edge(&mut f, 1, n1, n2).unwrap();
        let e2 = store.alloc_edge(&mut f, 1, n1, n2).unwrap();
        store.append_in_edge(&mut f, n2, e1).unwrap();
        store.append_in_edge(&mut f, n2, e2).unwrap();

        let in_edges: HashSet<RecordId> = store
            .collect_in_edges(&mut f, n2)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(in_edges, HashSet::from([e1, e2]));
    }

    #[test]
    fn delete_edge_removes_from_both_lists() {
        let (mut store, mut f) = make_store();
        let n1 = store.alloc_node(&mut f, 1).unwrap();
        let n2 = store.alloc_node(&mut f, 1).unwrap();
        let e1 = store.alloc_edge(&mut f, 1, n1, n2).unwrap();
        let e2 = store.alloc_edge(&mut f, 1, n1, n2).unwrap();
        store.append_out_edge(&mut f, n1, e1).unwrap();
        store.append_out_edge(&mut f, n1, e2).unwrap();
        store.append_in_edge(&mut f, n2, e1).unwrap();
        store.append_in_edge(&mut f, n2, e2).unwrap();

        store.delete_edge(&mut f, e1).unwrap();

        let out_edges: HashSet<RecordId> = store
            .collect_out_edges(&mut f, n1)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        let in_edges: HashSet<RecordId> = store
            .collect_in_edges(&mut f, n2)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert!(!out_edges.contains(&e1));
        assert!(!in_edges.contains(&e1));
        assert!(out_edges.contains(&e2));
        assert!(in_edges.contains(&e2));
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(50))]
        #[test]
        fn random_append_delete_edges_consistent(
            ops in proptest::collection::vec(any::<bool>(), 1..100usize)
        ) {
            let (mut store, mut f) = make_store();
            let n1 = store.alloc_node(&mut f, 1).unwrap();
            let n2 = store.alloc_node(&mut f, 1).unwrap();

            let mut model: HashMap<RecordId, bool> = HashMap::new();

            for append in ops {
                if append {
                    let e = store.alloc_edge(&mut f, 1, n1, n2).unwrap();
                    store.append_out_edge(&mut f, n1, e).unwrap();
                    store.append_in_edge(&mut f, n2, e).unwrap();
                    model.insert(e, true);
                } else if let Some(&target) = model.keys().next().cloned().as_ref() {
                    store.delete_edge(&mut f, target).unwrap();
                    model.remove(&target);
                }
            }

            let expected: HashSet<RecordId> = model.keys().cloned().collect();
            let actual_out: HashSet<RecordId> = store.collect_out_edges(&mut f, n1).unwrap()
                .into_iter().map(|e| e.id).collect();
            let actual_in: HashSet<RecordId> = store.collect_in_edges(&mut f, n2).unwrap()
                .into_iter().map(|e| e.id).collect();
            prop_assert_eq!(actual_out, expected.clone());
            prop_assert_eq!(actual_in, expected);
        }
    }
}
