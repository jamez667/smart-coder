//! PageRank via power iteration (spec 05 — the PageRank repo map).
//!
//! Hand-rolled rather than pulling in a graph crate: it's a few lines of power
//! iteration and keeps the dependency footprint to just the tree-sitter grammars.
//! Supports **personalization** (a non-uniform teleport vector) so we can bias the
//! ranking toward symbols named in the current task and files already in play —
//! the boosts that make the map relevant *now* (aider).

/// A directed edge `from -> to` with a weight (defaults to 1.0).
#[derive(Debug, Clone, Copy)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub weight: f64,
}

/// Run personalized PageRank over `n` nodes and `edges`.
///
/// * `damping` — the usual ~0.85 follow-vs-teleport split.
/// * `personalization` — per-node teleport weights (boosts). If empty, uniform.
///   Need not be normalized; it's normalized internally.
///
/// Returns a score per node, summing to ~1.0. Deterministic: fixed iteration
/// count, no randomness.
pub fn pagerank(
    n: usize,
    edges: &[Edge],
    damping: f64,
    personalization: &[f64],
    iterations: usize,
) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }

    // Teleport distribution (normalized personalization, or uniform).
    let teleport = normalized_teleport(n, personalization);

    // Out-weight per node, to split each node's rank among its out-edges.
    let mut out_weight = vec![0.0; n];
    for e in edges {
        if e.from < n && e.to < n {
            out_weight[e.from] += e.weight;
        }
    }

    let mut rank = teleport.clone();
    for _ in 0..iterations {
        let mut next = vec![0.0; n];

        // Dangling mass (nodes with no out-edges) is redistributed by teleport.
        let mut dangling = 0.0;
        for i in 0..n {
            if out_weight[i] == 0.0 {
                dangling += rank[i];
            }
        }

        for i in 0..n {
            next[i] += (1.0 - damping) * teleport[i];
            next[i] += damping * dangling * teleport[i];
        }
        for e in edges {
            if e.from < n && e.to < n && out_weight[e.from] > 0.0 {
                next[e.to] += damping * rank[e.from] * (e.weight / out_weight[e.from]);
            }
        }
        rank = next;
    }
    rank
}

fn normalized_teleport(n: usize, personalization: &[f64]) -> Vec<f64> {
    if personalization.len() == n {
        let sum: f64 = personalization.iter().map(|x| x.max(0.0)).sum();
        if sum > 0.0 {
            return personalization.iter().map(|x| x.max(0.0) / sum).collect();
        }
    }
    vec![1.0 / n as f64; n]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(from: usize, to: usize) -> Edge {
        Edge {
            from,
            to,
            weight: 1.0,
        }
    }

    #[test]
    fn empty_graph_is_empty() {
        assert!(pagerank(0, &[], 0.85, &[], 20).is_empty());
    }

    #[test]
    fn scores_sum_to_about_one() {
        let edges = [edge(0, 1), edge(1, 2), edge(2, 0)];
        let r = pagerank(3, &edges, 0.85, &[], 50);
        let sum: f64 = r.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "sum={sum}");
    }

    #[test]
    fn a_referenced_node_outranks_a_leaf() {
        // 0,1,2 all point at 3; 3 points nowhere. 3 should rank highest.
        let edges = [edge(0, 3), edge(1, 3), edge(2, 3)];
        let r = pagerank(4, &edges, 0.85, &[], 100);
        let hub = r[3];
        assert!(hub > r[0] && hub > r[1] && hub > r[2], "ranks={r:?}");
    }

    #[test]
    fn personalization_biases_the_ranking() {
        // Two disconnected nodes; personalization favors node 1 heavily.
        let r = pagerank(2, &[], 0.85, &[1.0, 9.0], 50);
        assert!(r[1] > r[0], "ranks={r:?}");
    }

    #[test]
    fn is_deterministic() {
        let edges = [edge(0, 1), edge(1, 2), edge(0, 2)];
        let a = pagerank(3, &edges, 0.85, &[], 30);
        let b = pagerank(3, &edges, 0.85, &[], 30);
        assert_eq!(a, b);
    }
}
