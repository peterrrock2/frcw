//! Data structures and algorithms for the recombination (ReCom) Markov chain.
use crate::buffers::SplitBuffer;
use crate::graph::Graph;
use rand::rngs::SmallRng;
use rand::Rng;
use std::result::Result;

/// ReCom runners.
pub mod run;

/// A lightweight list-of-lists representation of a spanning tree.
pub type MST = Vec<Vec<usize>>;

/// A proposal generated by the ReCom chain.
///
/// We limit proposals to the merging and splitting of two districts.
/// (There is a generalized version of ReCom that merges and splits
/// an arbitrary number of districts at a time.) By convention, we
/// refer to the two districts in a merge/split operation as `a` and `b`.
#[derive(Clone)]
pub struct RecomProposal {
    /// The label of the `a`-district in the merge-split proposal.
    pub a_label: usize,
    /// The label of the `b`-district in the merge-split proposal.
    pub b_label: usize,
    /// The population of the proposed  `a`-district.
    pub a_pop: u32,
    /// The population of the proposed  `b`-district.
    pub b_pop: u32,
    /// The node indices in the proposed `a`-district.
    pub a_nodes: Vec<usize>,
    /// The node indices in the proposed `b`-district.
    pub b_nodes: Vec<usize>,
}

/// The supported variants of ReCom (unstable!)
#[derive(Copy, Clone, PartialEq)]
pub enum RecomVariant {
    /// Reversible ReCom.
    Reversible,
    /// Normal (non-reversible) ReCom with district pairs selected by
    /// choosing a random cut edge. Spanning trees are sampled from
    /// the uniform distribution.
    CutEdgesUST,
    /// Normal (non-reversible) ReCom with district pairs selected by
    /// choosing random pairs of district indices until an adjacent pair
    /// is found. Non-adjacent pairs are self-loops. Spanning trees are
    /// sampled from the uniform distribution.
    DistrictPairsUST,
    /// Normal (non-reversible) ReCom with district pairs selected by
    /// choosing a random cut edge. Spanning trees are sampled by drawing
    /// edge weights uniformly at random and finding the minimum spanning
    /// tree.
    CutEdgesRMST,
    /// Normal (non-reversible) ReCom with district pairs selected by
    /// choosing random pairs of district indices until an adjacent pair
    /// is found. Non-adjacent pairs are self-loops. Spanning trees are
    /// sampled by drawing edge weights uniformly at random and finding
    /// the minimum spanning tree.
    DistrictPairsRMST
}

/// The parameters of a ReCom chain run.
#[derive(Copy, Clone)]
pub struct RecomParams {
    /// The minimum population of a district.
    pub min_pop: u32,
    /// The maximum population of a district.
    pub max_pop: u32,
    /// A soft upper bound on the number of ε-balance nodes in a spanning tree.
    /// Only used for reversible ReCom.
    pub balance_ub: u32,
    /// The number of steps, including self-loops in the chain run.
    /// This does *not* necessarily correspond to the number of
    /// unique plans generated by the run.
    pub num_steps: u64,
    /// The seed of the random number of generator.     
    pub rng_seed: u64,
    /// The type of ReCom chain to run.
    pub variant: RecomVariant,
}

impl RecomProposal {
    /// Creates an empty ReCom proposal buffer with node lists of
    /// capacity `n`.
    pub fn new_buffer(n: usize) -> RecomProposal {
        return RecomProposal {
            a_label: 0,
            b_label: 0,
            a_pop: 0,
            b_pop: 0,
            a_nodes: Vec::<usize>::with_capacity(n),
            b_nodes: Vec::<usize>::with_capacity(n),
        };
    }

    /// Resets the proposal (useful when using as a reusable buffer).
    pub fn clear(&mut self) {
        self.a_nodes.clear();
        self.b_nodes.clear();
        // TODO: reset integer fields?
    }

    /// Returns the seam length of a proposal---that is,
    /// the number of cut edges along the boundary between the
    /// `a`-district and the `b`-district.
    ///
    /// Uses the underlying `graph`.
    pub fn seam_length(&self, graph: &Graph) -> usize {
        let mut a_mask = vec![false; graph.pops.len()];
        for &node in self.a_nodes.iter() {
            a_mask[node] = true;
        }
        let mut seam = 0;
        for &node in self.b_nodes.iter() {
            for &neighbor in graph.neighbors[node].iter() {
                if a_mask[neighbor] {
                    seam += 1;
                }
            }
        }
        return seam;
    }
}

/// Attempts to propose a random recombination (spanning tree-based merge
/// and split) of districts `a` and `b` using a provided random MST. Returns
/// a `Result` containing either an error (to represent a self-loop) or
/// the number of balance nodes found when proposing (to represent a successful
/// proposal). The [RecomProposal] buffer (`buf`) is populated in place.
///
/// # Arguments
///
/// * `subgraph` - A graph containing the union of nodes in districts `a` and `b`.
/// * `rng` - The random number generator used to generate the proposal.
/// * `mst` - A minimum spanning tree of `subgraph`.
/// * `a` - The label of the `a`-district.
/// * `b` - The label of the `b`-district.
/// * `buf` - A buffer for use during split generation.
/// * `proposal` - The buffer to store the generated proposal in
///     (if the proposal is successful).
/// * `subgraph_map` - A map between the node IDs in the subgraph and the node IDs
///   of the parent graph. (Proposals use the node IDs in the parent graph.)
/// * `params` - The parameters of the parent ReCom chain.
pub fn random_split(
    subgraph: &Graph,
    rng: &mut SmallRng,
    mst: &MST,
    a: usize,
    b: usize,
    buf: &mut SplitBuffer,
    proposal: &mut RecomProposal,
    subgraph_map: &Vec<usize>,
    params: &RecomParams,
) -> Result<usize, String> {
    // TODO: split up into smaller private methods.
    buf.clear();
    proposal.clear();
    let n = subgraph.pops.len();
    let mut root = 0;
    while root < n {
        if subgraph.neighbors[root].len() > 1 {
            break;
        }
        root += 1;
    }
    if root == n {
        return Err("no leaf nodes in MST".to_string());
    }
    // Traverse the MST.
    buf.deque.push_back(root);
    while let Some(next) = buf.deque.pop_front() {
        buf.visited[next] = true;
        for &neighbor in mst[next].iter() {
            if !buf.visited[neighbor] {
                buf.deque.push_back(neighbor);
                buf.succ[next].push(neighbor);
                buf.pred[neighbor] = next;
            }
        }
    }

    // Recursively compute populations of subtrees.
    buf.deque.push_back(root);
    while let Some(next) = buf.deque.pop_back() {
        if !buf.pop_found[next] {
            if subgraph.neighbors[next].len() == 1 {
                buf.tree_pops[next] = subgraph.pops[next];
                buf.pop_found[next] = true;
            } else {
                // Populations of all child nodes found. :)
                if buf.succ[next].iter().all(|&node| buf.pop_found[node]) {
                    buf.tree_pops[next] =
                        buf.succ[next].iter().map(|&node| buf.tree_pops[node]).sum();
                    buf.tree_pops[next] += subgraph.pops[next];
                    buf.pop_found[next] = true;
                } else {
                    // Come back later.
                    buf.deque.push_back(next);
                    for &neighbor in buf.succ[next].iter() {
                        if !buf.pop_found[neighbor] {
                            buf.deque.push_back(neighbor);
                        }
                    }
                }
            }
        }
    }

    // Find ε-balanced cuts.
    for (index, &pop) in buf.tree_pops.iter().enumerate() {
        if pop >= params.min_pop
            && pop <= params.max_pop
            && subgraph.total_pop - pop >= params.min_pop
            && subgraph.total_pop - pop <= params.max_pop
        {
            buf.balance_nodes.push(index);
        }
    }
    if buf.balance_nodes.is_empty() {
        return Err("no balanced cuts".to_string());
    } /* else if buf.balance_nodes.len() > params.balance_ub as usize {
          // TODO: It might be useful to keep statistics here.
          println!(
              "Warning: found {} balance nodes (soft upper bound {})",
              buf.balance_nodes.len(),
              params.M
          );
      } */
    let balance_node = buf.balance_nodes[rng.gen_range(0..buf.balance_nodes.len())];
    buf.deque.push_back(balance_node);

    // Extract the nodes for a random cut.
    let mut a_pop = 0;
    while let Some(next) = buf.deque.pop_front() {
        if !buf.in_a[next] {
            proposal.a_nodes.push(subgraph_map[next]);
            a_pop += subgraph.pops[next];
            buf.in_a[next] = true;
            for &node in buf.succ[next].iter() {
                buf.deque.push_back(node);
            }
        }
    }
    for index in 0..n {
        if !buf.in_a[index] {
            proposal.b_nodes.push(subgraph_map[index]);
        }
    }
    proposal.a_label = a;
    proposal.b_label = b;
    proposal.a_pop = a_pop;
    proposal.b_pop = subgraph.total_pop - a_pop;
    return Ok(buf.balance_nodes.len());
}
