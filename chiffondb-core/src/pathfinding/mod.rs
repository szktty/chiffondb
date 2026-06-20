use std::collections::{HashMap, HashSet, VecDeque};

use crate::error::GraphError;
use crate::storage::file::DatabaseFile;
use crate::storage::page::RecordId;
use crate::storage::topology::TopologyStore;

/// Direction of path traversal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathDirection {
    Outgoing,
    Incoming,
    Both,
}

/// Options for path search.
#[derive(Debug, Clone)]
pub struct PathOptions {
    /// Maximum number of hops to search (default: 10).
    pub max_depth: usize,
    /// Direction in which edges are traversed.
    pub direction: PathDirection,
    /// List of edge type_ids to filter by (None = all edge types).
    pub edge_type_ids: Option<Vec<u16>>,
}

impl Default for PathOptions {
    fn default() -> Self {
        Self {
            max_depth: 10,
            direction: PathDirection::Both,
            edge_type_ids: None,
        }
    }
}

/// Result of a shortest-path search. Nodes and edges are ordered from start to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathResult {
    /// RecordIds of nodes on the path (includes start and end).
    pub node_rids: Vec<RecordId>,
    /// RecordIds of edges on the path (ordered start→end).
    pub edge_rids: Vec<RecordId>,
}

/// Connecting subgraph spanning multiple nodes.
#[derive(Debug, Clone)]
pub struct ConnectingSubgraph {
    /// All node RecordIds in the subgraph (deduplicated).
    pub node_rids: Vec<RecordId>,
    /// All edge RecordIds in the subgraph (deduplicated).
    pub edge_rids: Vec<RecordId>,
}

/// Shortest-path search engine using BFS.
///
/// Holds `&mut DatabaseFile` because topology record reads now go through the
/// file-backed `TopologyStore` (which faults pages via the bounded page cache).
pub struct PathfindingEngine<'a> {
    topo: &'a TopologyStore,
    file: &'a mut DatabaseFile,
}

impl<'a> PathfindingEngine<'a> {
    pub fn new(topo: &'a TopologyStore, file: &'a mut DatabaseFile) -> Self {
        Self { topo, file }
    }

    /// Searches for the shortest path from `from` to `to` using BFS.
    /// Returns `Ok(None)` if the target is unreachable.
    pub fn shortest_path(
        &mut self,
        from: RecordId,
        to: RecordId,
        options: &PathOptions,
    ) -> Result<Option<PathResult>, GraphError> {
        if from == to {
            return Ok(Some(PathResult {
                node_rids: vec![from],
                edge_rids: vec![],
            }));
        }

        // BFS queue: (current node, previous edge, previous node)
        // Map of visited nodes → (previous edge, previous node) for path reconstruction
        let mut visited: HashMap<RecordId, (Option<RecordId>, Option<RecordId>)> = HashMap::new();
        let mut queue: VecDeque<(RecordId, usize)> = VecDeque::new();

        visited.insert(from, (None, None));
        queue.push_back((from, 0));

        while let Some((current, depth)) = queue.pop_front() {
            if depth >= options.max_depth {
                continue;
            }

            // Enumerate adjacent edges
            let neighbors = self.neighbors(current, options)?;

            for (edge_rid, next_node) in neighbors {
                if visited.contains_key(&next_node) {
                    continue;
                }
                visited.insert(next_node, (Some(edge_rid), Some(current)));

                if next_node == to {
                    // Reconstruct the path
                    return Ok(Some(self.reconstruct_path(from, to, &visited)));
                }

                queue.push_back((next_node, depth + 1));
            }
        }

        Ok(None)
    }

    /// Computes shortest paths for all node pairs and returns the connecting subgraph.
    pub fn connecting_subgraph(
        &mut self,
        node_rids: &[RecordId],
        options: &PathOptions,
    ) -> Result<ConnectingSubgraph, GraphError> {
        let mut all_nodes: HashSet<RecordId> = HashSet::new();
        let mut all_edges: HashSet<RecordId> = HashSet::new();

        // Compute shortest paths for all unique pairs
        for i in 0..node_rids.len() {
            for j in (i + 1)..node_rids.len() {
                if let Some(path) = self.shortest_path(node_rids[i], node_rids[j], options)? {
                    all_nodes.extend(path.node_rids);
                    all_edges.extend(path.edge_rids);
                }
            }
            // Always include the specified node itself
            all_nodes.insert(node_rids[i]);
        }

        Ok(ConnectingSubgraph {
            node_rids: all_nodes.into_iter().collect(),
            edge_rids: all_edges.into_iter().collect(),
        })
    }

    /// Returns a list of (edge RecordId, adjacent node RecordId) reachable from the current node.
    fn neighbors(
        &mut self,
        node: RecordId,
        options: &PathOptions,
    ) -> Result<Vec<(RecordId, RecordId)>, GraphError> {
        let mut result = Vec::new();

        // Outgoing edges
        if matches!(
            options.direction,
            PathDirection::Outgoing | PathDirection::Both
        ) {
            for edge in self.topo.collect_out_edges(self.file, node)? {
                if Self::edge_matches(edge.edge_type_id, options) {
                    result.push((edge.id, edge.to_node));
                }
            }
        }

        // Incoming edges
        if matches!(
            options.direction,
            PathDirection::Incoming | PathDirection::Both
        ) {
            for edge in self.topo.collect_in_edges(self.file, node)? {
                if Self::edge_matches(edge.edge_type_id, options) {
                    result.push((edge.id, edge.from_node));
                }
            }
        }

        Ok(result)
    }

    fn edge_matches(type_id: u16, options: &PathOptions) -> bool {
        match &options.edge_type_ids {
            None => true,
            Some(ids) => ids.contains(&type_id),
        }
    }

    /// Reconstructs the start → end path from the visited map.
    fn reconstruct_path(
        &self,
        start: RecordId,
        end: RecordId,
        visited: &HashMap<RecordId, (Option<RecordId>, Option<RecordId>)>,
    ) -> PathResult {
        let mut node_rids = Vec::new();
        let mut edge_rids = Vec::new();

        let mut current = end;
        loop {
            node_rids.push(current);
            let (prev_edge, prev_node) = visited[&current];
            match (prev_edge, prev_node) {
                (Some(eid), Some(prev)) => {
                    edge_rids.push(eid);
                    if prev == start {
                        node_rids.push(start);
                        break;
                    }
                    current = prev;
                }
                _ => break,
            }
        }

        node_rids.reverse();
        edge_rids.reverse();

        PathResult {
            node_rids,
            edge_rids,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::file::DatabaseFile;
    use crate::storage::page::PAGE_SIZE;
    use crate::storage::topology::TopologyStore;
    use proptest::prelude::*;

    fn default_opts() -> PathOptions {
        PathOptions::default()
    }

    fn make_store() -> (TopologyStore, DatabaseFile) {
        let mut file = DatabaseFile::create_in_memory().unwrap();
        let empty = [0u8; PAGE_SIZE];
        for _ in 1..64 {
            file.append_page(&empty).unwrap();
        }
        (TopologyStore::new(1, 64), file)
    }

    /// Builds a linear graph of N nodes (0→1→...→N-1) and returns the node RecordId list.
    fn build_chain(n: usize) -> (TopologyStore, DatabaseFile, Vec<RecordId>) {
        let (mut topo, mut file) = make_store();
        let mut nodes: Vec<RecordId> = Vec::new();
        for _ in 0..n {
            nodes.push(topo.alloc_node(&mut file, 1).unwrap());
        }
        for i in 0..n - 1 {
            let eid = topo
                .alloc_edge(&mut file, 1, nodes[i], nodes[i + 1])
                .unwrap();
            topo.append_out_edge(&mut file, nodes[i], eid).unwrap();
            topo.append_in_edge(&mut file, nodes[i + 1], eid).unwrap();
        }
        (topo, file, nodes)
    }

    // ---- Basic tests ----

    #[test]
    fn same_node_returns_trivial_path() {
        let (topo, mut file) = make_store();
        let mut engine = PathfindingEngine::new(&topo, &mut file);
        let r0 = RecordId::new(0, 0); // dummy (no alloc)
        let r = engine.shortest_path(r0, r0, &default_opts()).unwrap();
        let path = r.unwrap();
        assert_eq!(path.node_rids, vec![r0]);
        assert!(path.edge_rids.is_empty());
    }

    #[test]
    fn direct_edge_path() {
        let (topo, mut file, nodes) = build_chain(2);
        let mut engine = PathfindingEngine::new(&topo, &mut file);
        let path = engine
            .shortest_path(nodes[0], nodes[1], &default_opts())
            .unwrap()
            .unwrap();
        assert_eq!(path.node_rids, vec![nodes[0], nodes[1]]);
        assert_eq!(path.edge_rids.len(), 1);
    }

    #[test]
    fn two_hop_path() {
        let (topo, mut file, nodes) = build_chain(3);
        let mut engine = PathfindingEngine::new(&topo, &mut file);
        let path = engine
            .shortest_path(nodes[0], nodes[2], &default_opts())
            .unwrap()
            .unwrap();
        assert_eq!(path.node_rids, vec![nodes[0], nodes[1], nodes[2]]);
        assert_eq!(path.edge_rids.len(), 2);
    }

    #[test]
    fn shortest_of_two_paths() {
        // When both 0→1→2 (2 hops) and 0→2 (1 hop) exist, the 1-hop path is returned
        let (mut topo, mut file) = make_store();
        let n0 = topo.alloc_node(&mut file, 1).unwrap();
        let n1 = topo.alloc_node(&mut file, 1).unwrap();
        let n2 = topo.alloc_node(&mut file, 1).unwrap();
        let e01 = topo.alloc_edge(&mut file, 1, n0, n1).unwrap();
        topo.append_out_edge(&mut file, n0, e01).unwrap();
        topo.append_in_edge(&mut file, n1, e01).unwrap();
        let e12 = topo.alloc_edge(&mut file, 1, n1, n2).unwrap();
        topo.append_out_edge(&mut file, n1, e12).unwrap();
        topo.append_in_edge(&mut file, n2, e12).unwrap();
        let e02 = topo.alloc_edge(&mut file, 1, n0, n2).unwrap();
        topo.append_out_edge(&mut file, n0, e02).unwrap();
        topo.append_in_edge(&mut file, n2, e02).unwrap();

        let mut engine = PathfindingEngine::new(&topo, &mut file);
        let path = engine
            .shortest_path(n0, n2, &default_opts())
            .unwrap()
            .unwrap();
        assert_eq!(path.node_rids.len(), 2);
        assert_eq!(path.edge_rids.len(), 1);
    }

    #[test]
    fn unreachable_returns_none() {
        // n0→n1, n2 is isolated
        let (mut topo, mut file) = make_store();
        let n0 = topo.alloc_node(&mut file, 1).unwrap();
        let n1 = topo.alloc_node(&mut file, 1).unwrap();
        let n2 = topo.alloc_node(&mut file, 1).unwrap();
        let e = topo.alloc_edge(&mut file, 1, n0, n1).unwrap();
        topo.append_out_edge(&mut file, n0, e).unwrap();
        topo.append_in_edge(&mut file, n1, e).unwrap();

        let mut engine = PathfindingEngine::new(&topo, &mut file);
        assert!(engine
            .shortest_path(n0, n2, &default_opts())
            .unwrap()
            .is_none());
    }

    #[test]
    fn max_depth_limits_search() {
        // 0→1→2→3 (3 hops), unreachable when max_depth=2
        let (topo, mut file, nodes) = build_chain(4);
        let mut engine = PathfindingEngine::new(&topo, &mut file);
        let opts = PathOptions {
            max_depth: 2,
            ..default_opts()
        };
        assert!(engine
            .shortest_path(nodes[0], nodes[3], &opts)
            .unwrap()
            .is_none());
    }

    #[test]
    fn direction_outgoing_only() {
        let (topo, mut file, nodes) = build_chain(2);
        let mut engine = PathfindingEngine::new(&topo, &mut file);
        let opts = PathOptions {
            direction: PathDirection::Outgoing,
            ..default_opts()
        };
        // Reverse direction is unreachable with Outgoing
        assert!(engine
            .shortest_path(nodes[1], nodes[0], &opts)
            .unwrap()
            .is_none());
        // Forward direction is reachable
        assert!(engine
            .shortest_path(nodes[0], nodes[1], &opts)
            .unwrap()
            .is_some());
    }

    #[test]
    fn direction_incoming_only() {
        let (topo, mut file, nodes) = build_chain(2);
        let mut engine = PathfindingEngine::new(&topo, &mut file);
        let opts = PathOptions {
            direction: PathDirection::Incoming,
            ..default_opts()
        };
        // Reverse direction is reachable with Incoming
        assert!(engine
            .shortest_path(nodes[1], nodes[0], &opts)
            .unwrap()
            .is_some());
        // Forward direction is not reachable with Incoming
        assert!(engine
            .shortest_path(nodes[0], nodes[1], &opts)
            .unwrap()
            .is_none());
    }

    #[test]
    fn edge_type_filter() {
        let (mut topo, mut file) = make_store();
        let n0 = topo.alloc_node(&mut file, 1).unwrap();
        let n1 = topo.alloc_node(&mut file, 1).unwrap();
        let n2 = topo.alloc_node(&mut file, 1).unwrap();
        // Edge with type_id=1: n0→n1
        let e1 = topo.alloc_edge(&mut file, 1, n0, n1).unwrap();
        topo.append_out_edge(&mut file, n0, e1).unwrap();
        topo.append_in_edge(&mut file, n1, e1).unwrap();
        // Edge with type_id=2: n1→n2
        let e2 = topo.alloc_edge(&mut file, 2, n1, n2).unwrap();
        topo.append_out_edge(&mut file, n1, e2).unwrap();
        topo.append_in_edge(&mut file, n2, e2).unwrap();

        // type_id=1 only: n0→n1 reachable, n0→n2 not (n1→n2 has type_id=2)
        let opts1 = PathOptions {
            edge_type_ids: Some(vec![1]),
            ..default_opts()
        };
        {
            let mut engine = PathfindingEngine::new(&topo, &mut file);
            assert!(engine.shortest_path(n0, n1, &opts1).unwrap().is_some());
            assert!(engine.shortest_path(n0, n2, &opts1).unwrap().is_none());
        }

        // No filter: n0→n2 is also reachable
        {
            let mut engine = PathfindingEngine::new(&topo, &mut file);
            assert!(engine
                .shortest_path(n0, n2, &default_opts())
                .unwrap()
                .is_some());
        }
    }

    #[test]
    fn connecting_subgraph_includes_all_paths() {
        // n0→n1→n2: the connecting subgraph for [n0, n2] includes n0, n1, n2
        let (topo, mut file, nodes) = build_chain(3);
        let mut engine = PathfindingEngine::new(&topo, &mut file);
        let sg = engine
            .connecting_subgraph(&[nodes[0], nodes[2]], &default_opts())
            .unwrap();
        assert!(sg.node_rids.contains(&nodes[0]));
        assert!(sg.node_rids.contains(&nodes[1]));
        assert!(sg.node_rids.contains(&nodes[2]));
        assert_eq!(sg.edge_rids.len(), 2);
    }

    #[test]
    fn connecting_subgraph_deduplicates() {
        // n0→n1→n2: no duplicates even when all nodes are specified
        let (topo, mut file, nodes) = build_chain(3);
        let mut engine = PathfindingEngine::new(&topo, &mut file);
        let sg = engine
            .connecting_subgraph(&[nodes[0], nodes[1], nodes[2]], &default_opts())
            .unwrap();
        assert_eq!(sg.node_rids.len(), 3);
        assert_eq!(sg.edge_rids.len(), 2);
    }

    // ---- proptest ----

    proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(50))]

        #[test]
        fn path_length_matches_node_count(chain_len in 2usize..8usize) {
            let (topo, mut file, nodes) = build_chain(chain_len + 1);
            let mut engine = PathfindingEngine::new(&topo, &mut file);
            let path = engine
                .shortest_path(nodes[0], nodes[chain_len], &default_opts())
                .unwrap()
                .unwrap();
            prop_assert_eq!(path.node_rids.len(), chain_len + 1);
            prop_assert_eq!(path.edge_rids.len(), chain_len);
        }
    }
}
