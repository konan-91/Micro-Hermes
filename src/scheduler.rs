/// Scheduler — Algorithm 1 (§5.2.2).
///
/// Three-stage cascading filter over the WST:
///   Stage 1 — drop hung workers (stale timestamp)
///   Stage 2 — drop above-average+θ connection count
///   Stage 3 — drop above-average+θ pending events
///
/// Returns a ScheduleResult containing:
///   • bitmap  — u64 where bit i set means worker i passed all three stages.
///     In Phase 2 this value is written into the MSel eBPF map (§5.4) so
///     the kernel dispatcher can read it.  In Phase 1 the userspace
///     dispatcher consumes it directly.
///   • survivors after each stage (for metrics / CSV output)

use crate::wst::{now_monotonic_ns, WorkerSnapshot, Wst, NUM_WORKERS};

/// Workers inactive longer than this are considered hung (§5.2.2).
/// 200 ms matches the Hermes paper.
pub const HANG_THRESHOLD_NS: i64 = 200_000_000;

/// Additive offset θ added to the average in FilterCount (Algo. 1 line 13).
/// Prevents the candidate set from collapsing to 0000 on cold-start when all
/// metrics are zero (avg=0, threshold=0 would eliminate every worker).
/// Paper's Fig. 15 shows θ/avg ≈ 0.5 is optimal in production; we default
/// to a constant additive value of 2.0 because avg can legitimately be 0.
pub const THETA: f64 = 2.0;

/// Result of one scheduling pass.
#[derive(Debug, Clone)]
pub struct ScheduleResult {
    /// Bitmap of surviving candidates (bit i → worker i).
    pub bitmap: u64,
    /// Workers alive after each stage (for logging/CSV).
    pub after_stage1: usize,
    pub after_stage2: usize,
    pub after_stage3: usize,
    /// Raw snapshots used for this decision (useful for CSV output).
    pub snapshots: [WorkerSnapshot; NUM_WORKERS],
}

/// Scheduling policy — selects what the coarse-grained filter compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Full Hermes Algorithm 1: Time → Conn → Event.
    Hermes,
    /// Baseline: LIFO-like — always return the highest-ID available worker.
    /// Mimics epoll exclusive's "most recently woken" bias.
    Lifo,
    /// Baseline: stateless reuseport-style hash — ignore load, use identity.
    ReuseportHash,
}

/// Run Algorithm 1 over the WST snapshots.
pub fn schedule(wst: &Wst, hang_threshold_ns: i64, theta: f64, policy: Policy) -> ScheduleResult {
    let now = now_monotonic_ns();
    let snapshots = wst.snapshot_all();

    match policy {
        Policy::Lifo => schedule_lifo(&snapshots),
        Policy::ReuseportHash => schedule_reuseport_hash(&snapshots),
        Policy::Hermes => schedule_hermes(&snapshots, now, hang_threshold_ns, theta),
    }
}

fn schedule_hermes(
    snapshots: &[WorkerSnapshot; NUM_WORKERS],
    now: i64,
    hang_threshold_ns: i64,
    theta: f64,
) -> ScheduleResult {
    // Stage 1: timestamp filter — drop workers that look hung.
    let after1: Vec<usize> = (0..NUM_WORKERS)
        .filter(|&i| {
            let t = snapshots[i].last_loop_entry;
            // Cold-start guard: if last_loop_entry == 0 the worker hasn't
            // entered its first loop yet; treat it as alive rather than hung.
            t == 0 || (now - t < hang_threshold_ns)
        })
        .collect();

    // Stage 2: connection count filter — drop above-average+θ.
    let after2 = filter_below_baseline(&after1, theta, |i| snapshots[i].accumulated_conns);

    // Stage 3: pending-event filter — drop above-average+θ.
    let after3 = filter_below_baseline(&after2, theta, |i| snapshots[i].pending_events);

    let bitmap = after3.iter().fold(0u64, |b, &i| b | (1u64 << i));

    ScheduleResult {
        bitmap,
        after_stage1: after1.len(),
        after_stage2: after2.len(),
        after_stage3: after3.len(),
        snapshots: *snapshots,
    }
}

/// Baseline: highest-ID available worker (mimics LIFO / epoll exclusive bias).
fn schedule_lifo(snapshots: &[WorkerSnapshot; NUM_WORKERS]) -> ScheduleResult {
    // Pick the highest-ID worker with non-zero last_loop_entry (has started).
    let chosen = (0..NUM_WORKERS)
        .rev()
        .find(|&i| snapshots[i].last_loop_entry != 0)
        .unwrap_or(NUM_WORKERS - 1);
    let bitmap = 1u64 << chosen;
    ScheduleResult {
        bitmap,
        after_stage1: 1,
        after_stage2: 1,
        after_stage3: 1,
        snapshots: *snapshots,
    }
}

/// Baseline: stateless hash — all workers always available, ignore load.
fn schedule_reuseport_hash(snapshots: &[WorkerSnapshot; NUM_WORKERS]) -> ScheduleResult {
    // Return full bitmap; the dispatcher will hash into it.
    let bitmap = (1u64 << NUM_WORKERS) - 1;
    ScheduleResult {
        bitmap,
        after_stage1: NUM_WORKERS,
        after_stage2: NUM_WORKERS,
        after_stage3: NUM_WORKERS,
        snapshots: *snapshots,
    }
}

/// Retain only candidates whose metric < avg + θ  (Algo. 1 lines 11-13).
fn filter_below_baseline(
    candidates: &[usize],
    theta: f64,
    metric: impl Fn(usize) -> i64,
) -> Vec<usize> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let sum: i64 = candidates.iter().map(|&i| metric(i)).sum();
    let avg = sum as f64 / candidates.len() as f64;
    let baseline = avg + theta;
    candidates
        .iter()
        .copied()
        .filter(|&i| (metric(i) as f64) < baseline)
        .collect()
}

/// Bitmap → sorted list of candidate worker IDs.
pub fn bitmap_to_candidates(bitmap: u64) -> Vec<usize> {
    (0..NUM_WORKERS).filter(|&i| bitmap & (1u64 << i) != 0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wst::{WorkerSnapshot, NUM_WORKERS};

    fn make_snapshots(entries: [(i64, i64, i64); NUM_WORKERS]) -> [WorkerSnapshot; NUM_WORKERS] {
        std::array::from_fn(|i| WorkerSnapshot {
            last_loop_entry: entries[i].0,
            pending_events: entries[i].1,
            accumulated_conns: entries[i].2,
        })
    }

    #[test]
    fn test_all_idle_all_survive() {
        // All workers freshly entered loop, no load.
        let now = now_monotonic_ns();
        let snaps = make_snapshots([
            (now, 0, 0),
            (now, 0, 0),
            (now, 0, 0),
            (now, 0, 0),
        ]);
        let res = schedule_hermes(&snaps, now, HANG_THRESHOLD_NS, THETA);
        assert_eq!(res.after_stage1, 4);
        assert_eq!(res.after_stage3, 4, "all idle workers should survive");
    }

    #[test]
    fn test_hung_worker_filtered_stage1() {
        let now = now_monotonic_ns();
        let old = now - HANG_THRESHOLD_NS - 1;
        let snaps = make_snapshots([
            (old, 0, 0), // worker 0 is hung
            (now, 0, 0),
            (now, 0, 0),
            (now, 0, 0),
        ]);
        let res = schedule_hermes(&snaps, now, HANG_THRESHOLD_NS, THETA);
        assert_eq!(res.after_stage1, 3, "hung worker must be dropped at stage 1");
        assert!(res.bitmap & 1 == 0, "worker 0 bit must be clear");
    }

    #[test]
    fn test_cold_start_zero_timestamp_treated_as_alive() {
        let now = now_monotonic_ns();
        // Worker 0 has last_loop_entry == 0 (hasn't started yet).
        let snaps = make_snapshots([
            (0, 0, 0),
            (now, 0, 0),
            (now, 0, 0),
            (now, 0, 0),
        ]);
        let res = schedule_hermes(&snaps, now, HANG_THRESHOLD_NS, THETA);
        assert_eq!(res.after_stage1, 4, "cold-start worker must not be marked hung");
    }

    #[test]
    fn test_high_conn_worker_filtered_stage2() {
        let now = now_monotonic_ns();
        // Worker 3 has 100 connections; others have 1. avg ≈ 25.75, baseline ≈ 27.75.
        let snaps = make_snapshots([
            (now, 0, 1),
            (now, 0, 1),
            (now, 0, 1),
            (now, 0, 100),
        ]);
        let res = schedule_hermes(&snaps, now, HANG_THRESHOLD_NS, THETA);
        assert!(res.bitmap & (1 << 3) == 0, "overloaded worker 3 must be filtered");
    }

    #[test]
    fn test_high_events_worker_filtered_stage3() {
        let now = now_monotonic_ns();
        // Worker 2 has 50 pending events; others have 0.
        let snaps = make_snapshots([
            (now, 0, 0),
            (now, 0, 0),
            (now, 50, 0),
            (now, 0, 0),
        ]);
        let res = schedule_hermes(&snaps, now, HANG_THRESHOLD_NS, THETA);
        assert!(res.bitmap & (1 << 2) == 0, "busy worker 2 must be filtered at stage 3");
    }

    #[test]
    fn test_filter_below_baseline_additive_offset() {
        // All values equal; with θ=2.0, baseline = avg+2 = 2 → all pass (value=0 < 2).
        let candidates: Vec<usize> = (0..4).collect();
        let result = filter_below_baseline(&candidates, 2.0, |_| 0);
        assert_eq!(result.len(), 4, "all zero-load workers should pass with additive θ");
    }

    #[test]
    fn test_bitmap_to_candidates_roundtrip() {
        let bitmap: u64 = 0b1010;
        let candidates = bitmap_to_candidates(bitmap);
        assert_eq!(candidates, vec![1, 3]);
    }
}
