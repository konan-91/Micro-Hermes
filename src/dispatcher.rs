/// Dispatcher — userspace simulation of Algorithm 2 (§5.4).
///
/// In the real Hermes system the kernel eBPF module reads the MSel bitmap and
/// uses reciprocal_scale + FindNthNonZeroBit to pick a worker for each new
/// TCP connection (Algo. 2).  In Phase 1 there are no real sockets, so we
/// replicate that logic in pure Rust.
///
/// The dispatcher is called by the workload generator each time it wants to
/// "send" a synthetic connection; it returns the worker ID that would receive
/// the connection according to the current bitmap and the active policy.

use crate::scheduler::{bitmap_to_candidates, Policy, ScheduleResult};
use crate::wst::NUM_WORKERS;
use std::sync::atomic::{AtomicU64, Ordering};

/// Simulated kernel dispatcher — mirrors Algo. 2.
///
/// `conn_hash` is a synthetic 4-tuple hash value (the dispatcher uses the
/// pre-computed kernel hash in the real system, §5.4 line 5).
pub fn dispatch(result: &ScheduleResult, conn_hash: u64, policy: Policy) -> usize {
    match policy {
        Policy::Hermes => dispatch_hermes(result.bitmap, conn_hash),
        Policy::Lifo => dispatch_lifo(result.bitmap),
        Policy::ReuseportHash => dispatch_reuseport_hash(conn_hash),
    }
}

/// Hermes dispatch: scale hash into [0, n) then find the Nth set bit.
/// Mirrors Algo. 2 lines 3-6.
fn dispatch_hermes(bitmap: u64, conn_hash: u64) -> usize {
    let candidates = bitmap_to_candidates(bitmap);
    let n = candidates.len();

    if n == 0 {
        // Fallback: reuseport-style hash when no candidates (§5.3.2).
        return dispatch_reuseport_hash(conn_hash);
    }

    // reciprocal_scale equivalent: map hash uniformly into [0, n).
    let nth = (conn_hash % n as u64) as usize;
    candidates[nth]
}

/// LIFO baseline: always pick the single highest-ID bit in the bitmap.
fn dispatch_lifo(bitmap: u64) -> usize {
    if bitmap == 0 {
        return NUM_WORKERS - 1;
    }
    // Highest set bit = most-recently-active worker (LIFO bias).
    63 - bitmap.leading_zeros() as usize
}

/// Reuseport baseline: stateless hash across all workers, ignores bitmap.
fn dispatch_reuseport_hash(conn_hash: u64) -> usize {
    (conn_hash % NUM_WORKERS as u64) as usize
}

/// Simple connection-ID counter used to generate synthetic 4-tuple hashes.
static CONN_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn next_conn_hash() -> u64 {
    // In the kernel, reciprocal_scale operates on the precomputed 4-tuple
    // hash.  We simulate this with a monotone counter mixed via a cheap
    // multiplicative hash (Knuth's constant) to avoid modular bias patterns.
    let n = CONN_COUNTER.fetch_add(1, Ordering::Relaxed);
    n.wrapping_mul(0x9e3779b97f4a7c15)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::ScheduleResult;
    use crate::wst::{WorkerSnapshot, NUM_WORKERS};

    fn dummy_snapshots() -> [WorkerSnapshot; NUM_WORKERS] {
        std::array::from_fn(|_| WorkerSnapshot {
            last_loop_entry: 0,
            pending_events: 0,
            accumulated_conns: 0,
        })
    }

    fn make_result(bitmap: u64) -> ScheduleResult {
        ScheduleResult {
            bitmap,
            after_stage1: 0,
            after_stage2: 0,
            after_stage3: 0,
            snapshots: dummy_snapshots(),
        }
    }

    #[test]
    fn test_hermes_dispatch_single_candidate() {
        let result = make_result(0b0100); // only worker 2
        let worker = dispatch_hermes(result.bitmap, 12345);
        assert_eq!(worker, 2);
    }

    #[test]
    fn test_hermes_dispatch_distributes_across_candidates() {
        // Workers 0 and 2 available (bitmap = 0101).
        let bitmap: u64 = 0b0101;
        let workers: std::collections::HashSet<usize> = (0..100)
            .map(|h| dispatch_hermes(bitmap, h))
            .collect();
        assert!(workers.contains(&0));
        assert!(workers.contains(&2));
        assert!(!workers.contains(&1));
        assert!(!workers.contains(&3));
    }

    #[test]
    fn test_lifo_picks_highest_id() {
        let bitmap: u64 = 0b1010; // workers 1 and 3
        assert_eq!(dispatch_lifo(bitmap), 3);
    }

    #[test]
    fn test_reuseport_hash_ignores_bitmap() {
        // Should distribute across all NUM_WORKERS regardless of load.
        let workers: std::collections::HashSet<usize> = (0..NUM_WORKERS * 10)
            .map(|h| dispatch_reuseport_hash(h as u64))
            .collect();
        assert_eq!(workers.len(), NUM_WORKERS);
    }

    #[test]
    fn test_empty_bitmap_falls_back_to_reuseport() {
        let result = make_result(0);
        // Should not panic; falls back to reuseport hash.
        let worker = dispatch(&result, 42, Policy::Hermes);
        assert!(worker < NUM_WORKERS);
    }
}
