// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 afterautism project contributors

//! Graph algorithms over [`VisibleGraph`]: traversal, shortest path,
//! connected components, degrees, cycle detection. All deterministic.

use crate::core::NodeId;
use crate::topology::graph::VisibleGraph;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Traversal direction for path/component queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Follow outgoing edges.
    Outgoing,
    /// Follow incoming edges.
    Incoming,
    /// Follow both directions.
    Both,
}

/// Breadth-first traversal from `start`, returning nodes in discovery
/// order (start first).
pub fn bfs(
    graph: &VisibleGraph,
    start: NodeId,
    direction: Direction,
    max_depth: Option<usize>,
) -> Vec<NodeId> {
    let mut order = Vec::new();
    let mut seen: BTreeSet<NodeId> = BTreeSet::new();
    let mut queue: VecDeque<(NodeId, usize)> = VecDeque::new();
    if graph.contains_node(&start) {
        queue.push_back((start, 0));
        seen.insert(start);
    }
    while let Some((node, depth)) = queue.pop_front() {
        if let Some(max) = max_depth {
            if depth > max {
                continue;
            }
        }
        order.push(node);
        for neighbor in neighbors(graph, node, direction) {
            if seen.insert(neighbor) {
                queue.push_back((neighbor, depth + 1));
            }
        }
    }
    order
}

/// Depth-first traversal from `start`, returning nodes in discovery order.
pub fn dfs(
    graph: &VisibleGraph,
    start: NodeId,
    direction: Direction,
    max_depth: Option<usize>,
) -> Vec<NodeId> {
    let mut order = Vec::new();
    let mut seen: BTreeSet<NodeId> = BTreeSet::new();
    let mut stack: Vec<(NodeId, usize)> = Vec::new();
    if graph.contains_node(&start) {
        stack.push((start, 0));
        seen.insert(start);
    }
    while let Some((node, depth)) = stack.pop() {
        if let Some(max) = max_depth {
            if depth > max {
                continue;
            }
        }
        order.push(node);
        for neighbor in neighbors(graph, node, direction) {
            if seen.insert(neighbor) {
                stack.push((neighbor, depth + 1));
            }
        }
    }
    order
}

/// All direct neighbors of `node` in the given direction.
pub fn neighbors(graph: &VisibleGraph, node: NodeId, direction: Direction) -> Vec<NodeId> {
    let mut out = Vec::new();
    match direction {
        Direction::Outgoing | Direction::Both => {
            for idx in graph.outgoing_edges(&node) {
                if let Some(edge) = graph.edge(*idx) {
                    out.push(edge.to);
                }
            }
        }
        Direction::Incoming => {}
    }
    match direction {
        Direction::Incoming | Direction::Both => {
            for idx in graph.incoming_edges(&node) {
                if let Some(edge) = graph.edge(*idx) {
                    out.push(edge.from);
                }
            }
        }
        Direction::Outgoing => {}
    }
    out
}

/// Shortest path (BFS, unweighted) from `start` to `target`.
/// Returns `Some(vec![start, ..., target])` when reachable, `None`
/// otherwise. Ties break deterministically by id.
pub fn shortest_path(
    graph: &VisibleGraph,
    start: NodeId,
    target: NodeId,
    direction: Direction,
) -> Option<Vec<NodeId>> {
    if start == target {
        return Some(vec![start]);
    }
    let mut prev: BTreeMap<NodeId, NodeId> = BTreeMap::new();
    let mut queue: VecDeque<NodeId> = VecDeque::new();
    queue.push_back(start);
    prev.insert(start, start);
    while let Some(node) = queue.pop_front() {
        for neighbor in neighbors(graph, node, direction) {
            if let std::collections::btree_map::Entry::Vacant(e) = prev.entry(neighbor) {
                e.insert(node);
                if neighbor == target {
                    // Reconstruct.
                    let mut path = vec![target];
                    let mut cur = target;
                    while cur != start {
                        cur = prev[&cur];
                        path.push(cur);
                    }
                    path.reverse();
                    return Some(path);
                }
                queue.push_back(neighbor);
            }
        }
    }
    None
}

/// Connected components (undirected view — both directions).
/// Returns components as id-ordered lists, components ordered by their
/// smallest member id.
pub fn connected_components(graph: &VisibleGraph) -> Vec<Vec<NodeId>> {
    let nodes: Vec<NodeId> = graph.visible_nodes().iter().copied().collect();
    let mut unseen: BTreeSet<NodeId> = nodes.iter().copied().collect();
    let mut components = Vec::new();
    while let Some(&seed) = unseen.iter().next() {
        let component = bfs(graph, seed, Direction::Both, None);
        for n in &component {
            unseen.remove(n);
        }
        components.push(component);
    }
    components.sort_by_key(|c| c.first().copied().unwrap_or(NodeId::unknown()));
    components
}

/// Degree centrality for every node (`(node, in_degree, out_degree)`),
/// ordered by node id.
pub fn degrees(graph: &VisibleGraph) -> Vec<(NodeId, usize, usize)> {
    let mut out = Vec::new();
    for node in graph.visible_nodes() {
        let in_deg = graph.incoming_edges(node).len();
        let out_deg = graph.outgoing_edges(node).len();
        out.push((*node, in_deg, out_deg));
    }
    out.sort_by_key(|(id, _, _)| id.to_raw());
    out
}

/// True when the graph contains at least one cycle (directed, following
/// outgoing edges).
/// Three-color DFS cycle detection state.
#[derive(Clone, Copy, PartialEq)]
enum Color {
    White,
    Gray,
    Black,
}

fn visit(graph: &VisibleGraph, node: NodeId, color: &mut BTreeMap<NodeId, Color>) -> bool {
    color.insert(node, Color::Gray);
    for idx in graph.outgoing_edges(&node) {
        if let Some(edge) = graph.edge(*idx) {
            // Copy the color so the borrow ends before the recursive call.
            let neighbor_color = color.get(&edge.to).copied();
            match neighbor_color {
                Some(Color::Gray) => return true, // back edge
                Some(Color::White) if visit(graph, edge.to, color) => return true,
                _ => {}
            }
        }
    }
    color.insert(node, Color::Black);
    false
}

/// True when the graph contains at least one cycle (directed, following
/// outgoing edges). Detected with an iterative three-color DFS: a back
/// edge (gray neighbor) is a cycle.
pub fn has_cycle(graph: &VisibleGraph) -> bool {
    let mut color: BTreeMap<NodeId, Color> = graph
        .visible_nodes()
        .iter()
        .map(|n| (*n, Color::White))
        .collect();

    let nodes: Vec<NodeId> = graph.visible_nodes().iter().copied().collect();
    for node in nodes {
        if color[&node] == Color::White && visit(graph, node, &mut color) {
            return true;
        }
    }
    false
}

/// The largest connected component (by node count), ties by smallest id.
pub fn largest_component(graph: &VisibleGraph) -> Vec<NodeId> {
    connected_components(graph)
        .into_iter()
        .max_by_key(|c| {
            (
                c.len(),
                std::cmp::Reverse(c.first().copied().unwrap_or(NodeId::unknown()).to_raw()),
            )
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::EdgeType;
    use crate::topology::graph::VisibleGraph;

    fn nid(raw: u64) -> NodeId {
        NodeId::from_raw(raw)
    }

    /// Graph: 1->2, 2->3, 2->4, 3->1 (cycle 1-2-3), plus isolated 5.
    fn e(from: u64, to: u64) -> crate::adapter::Edge {
        crate::adapter::Edge {
            from: nid(from),
            to: nid(to),
            edge_type: EdgeType::new("parent"),
        }
    }

    fn sample() -> VisibleGraph {
        let nodes = vec![nid(1), nid(2), nid(3), nid(4), nid(5)];
        let edges = vec![e(1, 2), e(2, 3), e(2, 4), e(3, 1)];
        VisibleGraph::from_nodes_and_edges(nodes, edges)
    }

    #[test]
    fn bfs_discovers_in_order() {
        let g = sample();
        let order = bfs(&g, nid(1), Direction::Outgoing, None);
        assert_eq!(order[0], nid(1));
        assert_eq!(order.len(), 4, "1,2,3,4 reachable; 5 isolated");
        assert!(!order.contains(&nid(5)));
    }

    #[test]
    fn bfs_respects_max_depth() {
        let g = sample();
        let order = bfs(&g, nid(1), Direction::Outgoing, Some(1));
        assert_eq!(order.len(), 2, "only 1 and its direct neighbor 2");
    }

    #[test]
    fn shortest_path_found() {
        let g = sample();
        let path = shortest_path(&g, nid(1), nid(4), Direction::Outgoing).expect("path");
        assert_eq!(path, vec![nid(1), nid(2), nid(4)]);
    }

    #[test]
    fn shortest_path_missing_is_none() {
        let g = sample();
        assert!(shortest_path(&g, nid(1), nid(5), Direction::Outgoing).is_none());
    }

    #[test]
    fn components_split_isolation() {
        let g = sample();
        let comps = connected_components(&g);
        assert_eq!(comps.len(), 2);
        assert!(comps.iter().any(|c| c == &vec![nid(5)]));
        assert!(comps.iter().any(|c| c.len() == 4));
    }

    #[test]
    fn degrees_are_correct() {
        let g = sample();
        let deg = degrees(&g);
        let d2 = deg.iter().find(|(id, _, _)| *id == nid(2)).unwrap();
        assert_eq!(d2.1, 1, "in-degree of 2");
        assert_eq!(d2.2, 2, "out-degree of 2");
    }

    #[test]
    fn cycle_detected() {
        let g = sample();
        assert!(has_cycle(&g), "1->2->3->1 is a cycle");
    }

    #[test]
    fn acyclic_graph_has_no_cycle() {
        let nodes = vec![nid(1), nid(2), nid(3)];
        let edges = vec![e(1, 2), e(2, 3)];
        let g = VisibleGraph::from_nodes_and_edges(nodes, edges);
        assert!(!has_cycle(&g));
    }

    #[test]
    fn largest_component_is_the_four_node_one() {
        let g = sample();
        assert_eq!(largest_component(&g).len(), 4);
    }
}
