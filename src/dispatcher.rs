//! Dispatcher: the simulated kernel side of connection dispatch.
//!
//! Runs in the generator (parent) process, standing in for the kernel: for
//! each new connection it decides which worker's accept queue receives it.
//!
//! Three mechanisms, matching the paper's comparison set:
//!   - Hermes:    Algorithm 2 (§5.4), read the M_Sel bitmap, reciprocal_scale
//!                the 4-tuple hash into the candidate count, pick the Nth set
//!                bit. In Phase 2 `dispatch_hermes` is ported to eBPF
//!                (attached via SO_ATTACH_REUSEPORT_EBPF); it is written
//!                loop-bounded and Vec-free so the port is mechanical.
//!   - Reuseport: the kernel's default, stateless hash over all workers.
//!   - Lifo:      models epoll-exclusive wakeup (see dispatch_lifo).
//!
//! In Phase 2 the two baselines are not code we write at all: they are the
//! kernel's own EPOLLEXCLUSIVE / SO_REUSEPORT behavior; only the Hermes path
//! survives as an eBPF program

use crate::shm::SharedState;
use crate::wst::NUM_WORKERS;
use std::sync::atomic::Ordering;

/// Which dispatch mechanism is under test
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Full Hermes: Algorithm 1 in workers + Algorithm 2 here
    Hermes,
    /// Baseline: epoll-exclusive-style LIFO wakeup
    Lifo,
    /// Baseline: SO_REUSEPORT stateless 4-tuple hash
    ReuseportHash,
}

impl Policy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Policy::Hermes => "hermes",
            Policy::Lifo => "lifo",
            Policy::ReuseportHash => "reuseport",
        }
    }
}

/// Pick the worker that receives `conn`. Called once per new connection
pub fn dispatch(shared: &SharedState, policy: Policy, conn_hash: u64) -> usize {
    match policy {
        Policy::Hermes => dispatch_hermes(shared.msel.load(), conn_hash),
        Policy::Lifo => dispatch_lifo(shared),
        Policy::ReuseportHash => reuseport_hash(conn_hash),
    }
}

/// Algorithm 2 (§5.4). Written eBPF-portable: no heap, bounded loop.
///
/// Fallback rule from the paper: if the candidate count n <= 1 the kernel
/// keeps the default reuseport hash (a single candidate would concentrate
/// all connections arriving between scheduler updates onto one worker)
fn dispatch_hermes(bitmap: u64, conn_hash: u64) -> usize {
    let n = bitmap.count_ones();
    if n <= 1 {
        return reuseport_hash(conn_hash);
    }
    let nth = reciprocal_scale(conn_hash, n);
    find_nth_set_bit(bitmap, nth)
}

/// The kernel's reciprocal_scale(): maps a 32-bit hash uniformly into
/// [0, n) with one multiply and one shift, no modulo (include/linux/kernel.h)
fn reciprocal_scale(hash: u64, n: u32) -> u32 {
    (((hash as u32 as u64) * n as u64) >> 32) as u32
}

/// Position of the Nth (0-based) set bit. Caller guarantees nth < popcount
fn find_nth_set_bit(bitmap: u64, nth: u32) -> usize {
    let mut seen = 0u32;
    for i in 0..NUM_WORKERS {
        if bitmap & (1u64 << i) != 0 {
            if seen == nth {
                return i;
            }
            seen += 1;
        }
    }
    // Unreachable when the caller upholds nth < popcount; be safe anyway
    NUM_WORKERS - 1
}

/// SO_REUSEPORT baseline: stateless hash, no awareness of worker state
fn reuseport_hash(conn_hash: u64) -> usize {
    reciprocal_scale(conn_hash, NUM_WORKERS as u32) as usize
}

/// Epoll-exclusive baseline, modeled on the kernel's actual mechanism:
///
/// The listen socket's wait queue is ordered by epoll_ctl(ADD) registration,
/// with each registration inserted at the *head* of the list (fs/eventpoll.c),
/// so the queue order is STATIC: the last-registered worker sits at the
/// head permanently. A wakeup traverses from the head and stops at the first
/// non-busy worker (EPOLLEXCLUSIVE, Fig. A2). The paper's walkthrough is
/// explicit: "new connections will be prioritized to W3 unless it is busy"
/// (Fig. A3, W3 = last of three registered workers).
///
/// Simulation using generator-visible state:
///   - registration order = fork order, so worker N-1 registered last,
///     i.e. highest index = head of the wait queue;
///   - "busy" (skipped in the traversal) ~ non-empty accept queue or
///     pending events mid-processing;
///   - "all busy" ~ the connection sits in the shared accept queue until a
///     worker returns to epoll_wait, approximated by handing it to the
///     shortest backlog (tie-break toward the wait-queue head).
///
/// Net effect matches the paper: connections concentrate on the head worker
/// (and the few behind it), while a hung worker's growing backlog keeps it
/// out of the idle set
fn dispatch_lifo(shared: &SharedState) -> usize {
    let idle = (0..NUM_WORKERS).rev().find(|&i| {
        shared.queues[i].is_empty()
            && shared.wst.slot(i).pending_events.load(Ordering::Relaxed) == 0
    });
    if let Some(worker) = idle {
        return worker;
    }
    (0..NUM_WORKERS)
        .min_by_key(|&i| (shared.queues[i].len(), std::cmp::Reverse(i)))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm::{test_state, ConnDesc};

    #[test]
    fn test_reciprocal_scale_stays_in_range() {
        for hash in [0u64, 1, 0xffff_ffff, 0x1234_5678_9abc_def0, u64::MAX] {
            let scaled = reciprocal_scale(hash, NUM_WORKERS as u32);
            assert!((scaled as usize) < NUM_WORKERS);
        }
    }

    #[test]
    fn test_find_nth_set_bit() {
        // bitmap 0b1101 -> set bits at positions 0, 2, 3
        assert_eq!(find_nth_set_bit(0b1101, 0), 0);
        assert_eq!(find_nth_set_bit(0b1101, 1), 2);
        assert_eq!(find_nth_set_bit(0b1101, 2), 3);
    }

    #[test]
    fn test_hermes_dispatch_stays_within_candidates() {
        // Workers 0 and 2 available (bitmap = 0101)
        let bitmap: u64 = 0b0101;
        let workers: std::collections::HashSet<usize> = (0..1000)
            .map(|h| dispatch_hermes(bitmap, (h as u64).wrapping_mul(0x9e3779b97f4a7c15)))
            .collect();
        assert!(workers.contains(&0));
        assert!(workers.contains(&2));
        assert!(!workers.contains(&1));
        assert!(!workers.contains(&3));
    }

    #[test]
    fn test_hermes_single_candidate_falls_back_to_hash() {
        // Paper §5.3.2: n <= 1 -> default reuseport hashing, otherwise every
        // connection between scheduler updates would hit the same worker
        let bitmap: u64 = 0b0100;
        let workers: std::collections::HashSet<usize> = (0..1000)
            .map(|h| dispatch_hermes(bitmap, (h as u64).wrapping_mul(0x9e3779b97f4a7c15)))
            .collect();
        assert!(workers.len() > 1, "single-candidate bitmap must fall back to full hash");
    }

    #[test]
    fn test_empty_bitmap_falls_back_to_hash() {
        let worker = dispatch_hermes(0, 42);
        assert!(worker < NUM_WORKERS);
    }

    #[test]
    fn test_lifo_prefers_last_registered_idle_worker() {
        let state = test_state();
        // All idle -> the last-registered worker (highest index) is the head
        // of the wait queue and takes the connection (paper Fig. A3:
        // "prioritized to W3 unless it is busy")
        assert_eq!(dispatch_lifo(&state), NUM_WORKERS - 1);
        // Head worker busy -> traversal continues to the next registration
        state.wst.slot(NUM_WORKERS - 1).pending_events.store(3, Ordering::Relaxed);
        assert_eq!(dispatch_lifo(&state), NUM_WORKERS - 2);
    }

    #[test]
    fn test_lifo_all_busy_picks_shortest_backlog() {
        let state = test_state();
        let desc = ConnDesc { conn_id: 0, hash: 0, arrival_ns: 0, service_us: 0, lifetime_ms: 0 };
        // Every worker has a non-empty queue (nobody blocked in epoll_wait)
        for i in 0..NUM_WORKERS {
            for _ in 0..(i + 2) {
                state.queues[i].push(desc);
            }
        }
        // Worker 0 has the shortest backlog (2 entries)
        assert_eq!(dispatch_lifo(&state), 0);
    }
}
