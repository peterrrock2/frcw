//! ReCom-based optimization using short bursts.
//!
//! We use the "short bursts" heuristic introduced in Cannon et al. 2020
//! (see "Voting Rights, Markov Chains, and Optimization by Short Bursts",
//!  arXiv: 2011.02288) to maximize arbitrary partition-level objective
//! functions.
use super::super::{
    node_bound, random_split, uniform_dist_pair, RecomParams, RecomProposal, RecomVariant,
};
use super::{Optimizer, ScoreValue};
use crate::buffers::{SpanningTreeBuffer, SplitBuffer, SubgraphBuffer};
use crate::graph::Graph;
use crate::partition::Partition;
use crate::spanning_tree::{RMSTSampler, RegionAwareSampler, SpanningTreeSampler};
use crate::stats::partition_attr_sums;
use anyhow::{Context, Result};
use crossbeam::scope;
use crossbeam_channel::{unbounded, Receiver, Sender};
use rand::rngs::SmallRng;
use rand::SeedableRng;
use serde_json::json;
use std::collections::HashMap;
use std::marker::Send;

/// A unit of multithreaded work.
struct OptJobPacket {
    /// The number of steps to sample (*not* the number of unique plans).
    n_steps: usize,
    /// The change in the chain state since the last batch of work.
    /// If no new proposals are accepted, this may be `None`.
    diff: Option<Partition>,
    /// A sentinel used to kill the worker thread.
    terminate: bool,
}

/// The result of a unit of multithreaded work.
struct OptResultPacket {
    /// The best proposal found in a unit of work according to an
    /// objective function.
    best_partition: Option<Partition>,
    /// The score of the best proposal.
    best_score: Option<ScoreValue>,
}

/// Starts a ReCom optimization thread.
/// ReCom optimization threads run short ReCom chains ("short bursts"), which
/// are then aggregated by the main thread.
///
/// Arguments:
/// * `graph` - The graph associated with the chain.
/// * `partition` - The initial state of the chain.
/// * `params` - The chain parameters.
/// * `obj_fn` - The objective function to evaluate proposals against.
/// * `rng_seed` - The RNG seed for the job thread. (This should differ across threads.)
/// * `buf_size` - The buffer size for various chain buffers. This should usually be twice
///   the maximum possible district size (in nodes).
/// * `job_recv` - A Crossbeam channel for receiving batches of work from the main thread.
/// * `result_send` - A Crossbeam channel for sending completed batches to the main thread.
fn start_opt_thread(
    graph: Graph,
    mut partition: Partition,
    params: RecomParams,
    obj_fn: impl Fn(&Graph, &Partition) -> ScoreValue + Send + Copy,
    _accept_fn: Option<String>,
    rng_seed: u64,
    buf_size: usize,
    job_recv: Receiver<OptJobPacket>,
    result_send: Sender<OptResultPacket>,
) -> Result<()> {
    // TODO: consider supporting other ReCom variants.
    // We generally don't (or can't) care about distributional
    // properties, so it would make little sense to support reversible
    // ReCom or the like. RMST sampling is asymptotically more efficient
    // than UST sampling, so we use it as the default for now.
    let n = graph.pops.len();
    let mut rng: SmallRng = SeedableRng::seed_from_u64(rng_seed);
    let mut subgraph_buf = SubgraphBuffer::new(n, buf_size);
    let mut st_buf = SpanningTreeBuffer::new(buf_size);
    let mut split_buf = SplitBuffer::new(buf_size, params.balance_ub as usize);
    let mut proposal_buf = RecomProposal::new_buffer(buf_size);
    let mut st_sampler: Box<dyn SpanningTreeSampler>;
    if params.variant == RecomVariant::DistrictPairsRegionAware {
        st_sampler = Box::new(RegionAwareSampler::new(
            buf_size,
            params
                .region_weights
                .clone()
                .context("No region weights available in region-aware mode")?,
        ));
    } else if params.variant == RecomVariant::DistrictPairsRMST {
        st_sampler = Box::new(RMSTSampler::new(buf_size));
    } else {
        panic!("ReCom variant not supported by optimizer.");
    }

    let mut next: OptJobPacket = job_recv.recv()?;
    let mut start_partition = partition.clone();
    while !next.terminate {
        if let Some(cand_partition) = next.diff {
            start_partition = cand_partition;
        }
        partition = start_partition.clone();

        let mut best_partition: Option<Partition> = None;
        let mut score = obj_fn(&graph, &partition);
        let mut best_score: ScoreValue = score;
        let mut step = 0;

        while step < next.n_steps {
            // Sample a ReCom step.
            let dist_pair = uniform_dist_pair(&graph, &mut partition, &mut rng);
            if dist_pair.is_none() {
                continue;
            }
            let (dist_a, dist_b) = dist_pair.context("Expected district pair")?;
            partition.subgraph_with_attr(&graph, &mut subgraph_buf, dist_a, dist_b);
            st_sampler.random_spanning_tree(&subgraph_buf.graph, &mut st_buf, &mut rng);
            let split = random_split(
                &subgraph_buf.graph,
                &mut rng,
                &st_buf.st,
                dist_a,
                dist_b,
                &mut split_buf,
                &mut proposal_buf,
                &subgraph_buf.raw_nodes,
                &params,
            );
            if split.is_ok() {
                score = obj_fn(&graph, &partition);
                partition.update(&proposal_buf);
                if score >= best_score {
                    // TODO: reduce allocations by keeping a separate
                    // buffer for the best partition.
                    best_partition = Some(partition.clone());
                    best_score = score;
                }
                step += 1;
            }
        }
        let result = match best_partition {
            Some(partition) => OptResultPacket {
                best_partition: Some(partition.clone()),
                best_score: Some(best_score),
            },
            None => OptResultPacket {
                best_partition: None,
                best_score: None,
            },
        };
        result_send.send(result)?;
        next = job_recv
            .recv()
            .context("Could not receive next job packet")?;
    }
    Ok(())
}

/// Sends a batch of work to a ReCom optimization thread.
fn next_batch(
    send: &Sender<OptJobPacket>,
    diff: Option<Partition>,
    burst_length: usize,
) -> Result<()> {
    send.send(OptJobPacket {
        n_steps: burst_length,
        diff: diff,
        terminate: false,
    })?;
    Ok(())
}

/// Stops a ReCom optimization thread.
fn stop_opt_thread(send: &Sender<OptJobPacket>) -> Result<()> {
    send.send(OptJobPacket {
        n_steps: 0,
        diff: None,
        terminate: true,
    })?;
    Ok(())
}

pub struct ShortBurstsOptimizer {
    /// Chain parameters.
    params: RecomParams,
    /// The number of worker threads (excluding the main thread).
    n_threads: usize,
    /// The number of steps per burst.
    burst_length: usize,
    /// Print the best intermediate results?
    verbose: bool,
}

impl ShortBurstsOptimizer {
    pub fn new(
        params: RecomParams,
        n_threads: usize,
        burst_length: usize,
        verbose: bool,
    ) -> ShortBurstsOptimizer {
        ShortBurstsOptimizer {
            params: params,
            n_threads: n_threads,
            burst_length: burst_length,
            verbose: verbose,
        }
    }
}

impl Optimizer for ShortBurstsOptimizer {
    /// Runs a multi-threaded ReCom short bursts optimizer.
    ///
    /// # Arguments
    ///
    /// * `graph` - The graph associated with `partition`.
    /// * `partition` - The partition to start the chain run from (updated in place).
    /// * `obj_fn` - The objective to maximize.
    fn optimize(
        &self,
        graph: &Graph,
        mut partition: Partition,
        obj_fn: impl Fn(&Graph, &Partition) -> ScoreValue + Send + Clone + Copy,
        _accept_fn: Option<String>
    ) -> Partition {
        let mut step = 0;
        let node_ub = node_bound(&graph.pops, self.params.max_pop);
        let mut job_sends = vec![]; // main thread sends work to job threads
        let mut job_recvs = vec![]; // job threads receive work from main thread
        for _ in 0..self.n_threads {
            let (s, r): (Sender<OptJobPacket>, Receiver<OptJobPacket>) = unbounded();
            job_sends.push(s);
            job_recvs.push(r);
        }
        // All optimization threads send a summary of chain results back to the main thread.
        let (result_send, result_recv): (Sender<OptResultPacket>, Receiver<OptResultPacket>) =
            unbounded();
        let mut score = obj_fn(&graph, &partition);

        scope(|scope| {
            // Start optimization threads.
            for t_idx in 0..self.n_threads {
                // TODO: is this (+ t_idx) a sensible way to seed?
                let rng_seed = self.params.rng_seed + t_idx as u64 + 1;
                let job_recv = job_recvs[t_idx].clone();
                let result_send = result_send.clone();
                let partition = partition.clone();

                scope.spawn(move |_| {
                    start_opt_thread(
                        graph.clone(),
                        partition,
                        self.params.clone(),
                        obj_fn,
                        None,  // TODO: accept_fn.clone(),
                        rng_seed,
                        node_ub,
                        job_recv,
                        result_send,
                    ).unwrap();
                });
            }

            if self.params.num_steps > 0 {
                for job in job_sends.iter() {
                    next_batch(job, None, self.burst_length).unwrap();
                }
            }

            while step <= self.params.num_steps {
                let mut diff = None;
                for _ in 0..self.n_threads {
                    let packet: OptResultPacket = result_recv.recv().unwrap();  // TODO: un-unwrap
                    if let Some(cand_partition) = packet.best_partition {
                        if let Some(cand_score) = packet.best_score {
                            partition = cand_partition;
                            score = cand_score;
                            diff = Some(partition.clone());
                        }
                    }

                }
                step += (self.n_threads * self.burst_length) as u64;
                if diff.is_some() && self.verbose {
                    let min_pops = partition_attr_sums(&graph, &partition, "APBVAP20");
                    let total_pops = partition_attr_sums(&graph, &partition, "VAP20");
                    let seat_count = min_pops.iter().zip(total_pops.iter()).filter(|(&m, &t)| 2 * m >= t).count();

                    println!("{}", json!({
                        "step": step,
                        "type": "opt",
                        "score": score,
                        "bvap_maj": seat_count,
                        "assignment": partition.assignments.clone().into_iter().enumerate().collect::<HashMap<usize, u32>>()
                    }).to_string());
                }

                for job in job_sends.iter() {
                    next_batch(job, diff.clone(), self.burst_length).unwrap();
                }
            }

            // Terminate worker threads.
            for job in job_sends.iter() {
                stop_opt_thread(job).unwrap();
            }
            partition
        })
        .unwrap()
    }
}
