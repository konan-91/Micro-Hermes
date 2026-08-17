//! Connection arrival path, run in the parent after forking the workers.
//! Paces arrivals at the workload's CPS, computes a synthetic 4-tuple
//! hash, picks a worker via the dispatch mechanism under test and pushes
//! the connection into that worker's accept queue. This is the only
//! source of work in the simulation, so scheduling decisions directly
//! shape the load distribution

use crate::dispatcher::{dispatch, Policy};
use crate::shm::{ConnDesc, SharedState};
use crate::workload::WorkloadConfig;
use crate::wst::now_monotonic_ns;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Instant;

/// Outcome of one generation run
#[derive(Debug)]
pub struct GenStats {
    pub generated: u64,
    pub dropped: u64,
    /// Connections dispatched to each worker (pre-drop)
    pub dispatched_per_worker: Vec<u64>,
}

/// Generate cfg.cps connections/sec for cfg.duration, then signal
/// shutdown. seed perturbs the hash sequence so repeated trials draw
/// different but reproducible service times
pub fn run(shared: &SharedState, cfg: &WorkloadConfig, policy: Policy, seed: u64) -> GenStats {
    let interval = std::time::Duration::from_secs_f64(1.0 / cfg.cps as f64);
    let start = Instant::now();
    let mut next_arrival = start;
    let mut stats = GenStats {
        generated: 0,
        dropped: 0,
        dispatched_per_worker: vec![0; crate::wst::NUM_WORKERS],
    };

    while next_arrival.duration_since(start) < cfg.duration {
        // absolute-deadline pacing so oversleep doesn't accumulate drift
        let now = Instant::now();
        if next_arrival > now {
            thread::sleep(next_arrival - now);
        }

        let conn_id = stats.generated;
        // synthetic 4-tuple hash, a seeded counter mixed by Knuth's
        // constant, standing in for the kernel's skb hash
        let hash = (conn_id ^ seed.wrapping_mul(0xff51afd7ed558ccd))
            .wrapping_mul(0x9e3779b97f4a7c15);
        let service = cfg.service.sample(hash);

        let target = dispatch(shared, policy, hash);
        let desc = ConnDesc {
            conn_id,
            hash,
            arrival_ns: now_monotonic_ns(),
            service_us: service.as_micros() as u32,
            lifetime_ms: cfg.lifetime.as_millis() as u32,
        };
        stats.dispatched_per_worker[target] += 1;
        if !shared.queues[target].push(desc) {
            // accept queue overflow, the kernel would drop the SYN
            shared.drops[target].fetch_add(1, Ordering::Relaxed);
            stats.dropped += 1;
        }

        stats.generated += 1;
        next_arrival += interval;
    }

    shared.request_shutdown();
    stats
}
