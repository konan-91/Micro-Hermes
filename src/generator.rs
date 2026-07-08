//! Load generator — the simulated kernel's connection-arrival path.
//!
//! Runs in the parent process after forking the workers, standing in for
//! everything the kernel does per new connection in the real system:
//! a SYN completes (paced at the workload's CPS), the kernel computes the
//! 4-tuple hash, the dispatch mechanism under test picks a worker, and the
//! connection lands in that worker's accept queue.
//!
//! Crucially, this is the *only* source of work in the simulation — workers
//! process exactly what is dispatched to them, so scheduling decisions
//! directly shape the load distribution (the closed loop the paper's Fig. 8
//! describes).
//!
//! Phase 2: replaced entirely by real clients + the kernel's own accept
//! path; nothing here needs porting.

use crate::dispatcher::{dispatch, Policy};
use crate::shm::{ConnDesc, SharedState};
use crate::workload::WorkloadConfig;
use crate::wst::now_monotonic_ns;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Instant;

/// Outcome of one generation run.
#[derive(Debug)]
pub struct GenStats {
    pub generated: u64,
    pub dropped: u64,
    /// How many connections each worker was dispatched (pre-drop).
    pub dispatched_per_worker: Vec<u64>,
}

/// Generate `cfg.cps` connections/sec for `cfg.duration`, dispatching each
/// via `policy`, then signal shutdown. Blocks until traffic is done.
///
/// `seed` perturbs the synthetic 4-tuple hash sequence so repeated benchmark
/// trials draw different (but reproducible) service-time assignments rather
/// than replaying the identical connection stream.
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
        // Absolute-deadline pacing: sleep oversleep doesn't accumulate drift.
        let now = Instant::now();
        if next_arrival > now {
            thread::sleep(next_arrival - now);
        }

        let conn_id = stats.generated;
        // Synthetic 4-tuple hash: a counter (xor'd with the scrambled trial
        // seed) mixed by Knuth's multiplicative constant, standing in for
        // the kernel's skb hash. seed = 0 reproduces the canonical stream.
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
            // Accept-queue overflow: the kernel would drop the SYN.
            shared.drops[target].fetch_add(1, Ordering::Relaxed);
            stats.dropped += 1;
        }

        stats.generated += 1;
        next_arrival += interval;
    }

    shared.request_shutdown();
    stats
}
