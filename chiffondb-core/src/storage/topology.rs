use crate::error::GraphError;
use crate::storage::file::{DatabaseFile, TopologyKind};
use crate::storage::page::{Page, RecordId, SlotId};
use crate::storage::record::{EdgeRecord, NodeRecord, EDGE_RECORD_SIZE, NODE_RECORD_SIZE};

/// File-backed topology store.
///
/// Node and edge record pages are read/written through `DatabaseFile`, which fronts them with
/// its bounded page cache and never holds the record pages resident, so topology memory does
/// not grow with node/edge count (see `docs/plan-page-cache.md` Phase 2).
///
/// Node and edge logical pages resolve to physical file pages through per-kind `PageDirectory`
/// instances held by `DatabaseFile` (roots in the header; mapped lengths are the header's
/// `node_page_count` / `edge_page_count`). `TopologyStore` therefore keeps no segment metadata of
/// its own — it is a stateless façade over `DatabaseFile`'s topology page helpers. This removes
/// the fixed interleaved segment and its ~2000-node cap (design §3.2).
///
/// Slot occupancy is tracked solely by each page's own bitmap (`Page::is_used` /
/// `alloc_slot` / `free_slot`); there is no separate in-memory occupancy index.
#[derive(Clone, Default)]
pub struct TopologyStore;

impl TopologyStore {
    /// Creates a topology façade. All state lives in `DatabaseFile`'s header / directories.
    pub fn new() -> Self {
        Self
    }

    fn load_node_page(&self, file: &mut DatabaseFile, logical: usize) -> Result<Page, GraphError> {
        let bytes = file.read_topology_page(TopologyKind::Node, logical)?;
        Ok(Page::from_bytes(bytes, NODE_RECORD_SIZE))
    }

    fn load_edge_page(&self, file: &mut DatabaseFile, logical: usize) -> Result<Page, GraphError> {
        let bytes = file.read_topology_page(TopologyKind::Edge, logical)?;
        Ok(Page::from_bytes(bytes, EDGE_RECORD_SIZE))
    }

    // ---- Counts / page access ----

    pub fn node_page_count(&self, file: &DatabaseFile) -> usize {
        file.header.node_page_count as usize
    }

    pub fn edge_page_count(&self, file: &DatabaseFile) -> usize {
        file.header.edge_page_count as usize
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
        for logical in 0..self.node_page_count(file) {
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
        for logical in 0..self.edge_page_count(file) {
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
        if logical >= self.node_page_count(file) {
            return false;
        }
        self.load_node_page(file, logical)
            .map(|p| p.is_used(rid.slot_id))
            .unwrap_or(false)
    }

    /// Returns whether the given edge slot is currently used.
    pub fn edge_slot_used(&self, file: &mut DatabaseFile, rid: RecordId) -> bool {
        let logical = rid.page_id.0 as usize;
        if logical >= self.edge_page_count(file) {
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
        if logical >= self.node_page_count(file) {
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
        if logical >= self.node_page_count(file) {
            return Err(GraphError::StorageCorrupted(node.id.page_id.0));
        }
        let mut page = self.load_node_page(file, logical)?;
        page.write_slot(node.id.slot_id, &node.serialize())?;
        file.write_topology_page(TopologyKind::Node, logical, page.as_bytes())
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
        if logical >= self.edge_page_count(file) {
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
        if logical >= self.edge_page_count(file) {
            return Err(GraphError::StorageCorrupted(edge.id.page_id.0));
        }
        let mut page = self.load_edge_page(file, logical)?;
        page.write_slot(edge.id.slot_id, &edge.serialize())?;
        file.write_topology_page(TopologyKind::Edge, logical, page.as_bytes())
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
        file.write_topology_page(TopologyKind::Edge, logical, page.as_bytes())
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
        file.write_topology_page(TopologyKind::Node, logical, page.as_bytes())
    }

    // ---- Internal helpers ----

    /// Finds (or creates) a free node slot, returning its (logical page, slot).
    fn alloc_node_slot(&mut self, file: &mut DatabaseFile) -> Result<(usize, SlotId), GraphError> {
        for logical in 0..self.node_page_count(file) {
            let mut page = self.load_node_page(file, logical)?;
            if let Some(slot) = page.alloc_slot() {
                file.write_topology_page(TopologyKind::Node, logical, page.as_bytes())?;
                return Ok((logical, slot));
            }
        }
        // All pages full: map a fresh logical node page through the directory (append + push +
        // header persist, all crash-safe via the WAL). No fixed-segment capacity check — the
        // only ceiling now is the u32 logical page space.
        let mut page = Page::new(NODE_RECORD_SIZE);
        let slot = page.alloc_slot().ok_or(GraphError::StorageCorrupted(0))?;
        let logical = file.alloc_topology_page(TopologyKind::Node, page.as_bytes())?;
        Ok((logical, slot))
    }

    /// Finds (or creates) a free edge slot, returning its (logical page, slot).
    fn alloc_edge_slot(&mut self, file: &mut DatabaseFile) -> Result<(usize, SlotId), GraphError> {
        for logical in 0..self.edge_page_count(file) {
            let mut page = self.load_edge_page(file, logical)?;
            if let Some(slot) = page.alloc_slot() {
                file.write_topology_page(TopologyKind::Edge, logical, page.as_bytes())?;
                return Ok((logical, slot));
            }
        }
        let mut page = Page::new(EDGE_RECORD_SIZE);
        let slot = page.alloc_slot().ok_or(GraphError::StorageCorrupted(0))?;
        let logical = file.alloc_topology_page(TopologyKind::Edge, page.as_bytes())?;
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
        // Topology pages are mapped on demand through the node/edge directories, so no
        // pre-allocation is needed — the first alloc maps logical page 0.
        let file = DatabaseFile::create_in_memory().unwrap();
        (TopologyStore::new(), file)
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
