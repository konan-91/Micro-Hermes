//! Algorithm 1 (§5.2.2), hermes only. A three-stage cascading filter over
//! the WST. Stage 1 drops hung workers (stale loop timestamp), stage 2
//! drops above-average(+θ) connection counts, stage 3 drops
//! above-average(+θ) pending events.
//!
//! Pure function, snapshots in, bitmap out. The worker writes the bitmap
//! into the simulated M_Sel map, and this module survives phase 2
//! unchanged

use crate::wst::{WorkerSnapshot, Wst, NUM_WORKERS};

/// Workers whose last loop entry is older than this are considered hung.
/// The paper leaves the value implementation-defined; 200 ms comfortably
/// exceeds the 5 ms epoll timeout plus any healthy batch's processing time
/// for our workloads, while catching injected hangs quickly
pub const HANG_THRESHOLD_NS: i64 = 200_000_000;

/// Offset θ added to the average in FilterCount. The paper's Fig. 15
/// finds θ/avg = 0.5 optimal, and the floor keeps the filter permissive
/// near cold start where avg ~ 0
pub const THETA_RATIO: f64 = 0.5;
pub const THETA_MIN: f64 = 1.0;

/// Result of one scheduling pass
#[derive(Debug, Clone)]
pub struct ScheduleResult {
    /// Bitmap of surviving candidates (bit i -> worker i). Written to M_Sel
    pub bitmap: u64,
    /// Candidate count after each cascading stage (for metrics/CSV)
    pub after_stage1: usize,
    pub after_stage2: usize,
    pub after_stage3: usize,
}

/// Run Algorithm 1 over the live WST
pub fn schedule(wst: &Wst, now: i64, hang_threshold_ns: i64) -> ScheduleResult {
    schedule_from_snapshots(&wst.snapshot_all(), now, hang_threshold_ns)
}

fn schedule_from_snapshots(
    snapshots: &[WorkerSnapshot; NUM_WORKERS],
    now: i64,
    hang_threshold_ns: i64,
) -> ScheduleResult {
    // Stage 1, drop workers that look hung
    let after1: Vec<usize> = (0..NUM_WORKERS)
        .filter(|&i| {
            let t = snapshots[i].last_loop_entry;
            // a zero timestamp means the worker hasn't entered its first
            // loop yet, treat it as alive rather than hung
            t == 0 || (now - t < hang_threshold_ns)
        })
        .collect();

    // Stage 2, filter on connection counts
    let after2 = filter_below_baseline(&after1, |i| snapshots[i].accumulated_conns);

    // Stage 3, filter on pending events
    let after3 = filter_below_baseline(&after2, |i| snapshots[i].pending_events);

    let bitmap = after3.iter().fold(0u64, |b, &i| b | (1u64 << i));

    ScheduleResult {
        bitmap,
        after_stage1: after1.len(),
        after_stage2: after2.len(),
        after_stage3: after3.len(),
    }
}

/// FilterCount, keep candidates whose metric is below avg + θ, with
/// θ = max(THETA_RATIO * avg, THETA_MIN)
fn filter_below_baseline(candidates: &[usize], metric: impl Fn(usize) -> i64) -> Vec<usize> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let sum: i64 = candidates.iter().map(|&i| metric(i)).sum();
    let avg = sum as f64 / candidates.len() as f64;
    let theta = (THETA_RATIO * avg).max(THETA_MIN);
    let baseline = avg + theta;
    candidates
        .iter()
        .copied()
        .filter(|&i| (metric(i) as f64) < baseline)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wst::now_monotonic_ns;

    fn make_snapshots(entries: [(i64, i64, i64); NUM_WORKERS]) -> [WorkerSnapshot; NUM_WORKERS] {
        std::array::from_fn(|i| WorkerSnapshot {
            last_loop_entry: entries[i].0,
            pending_events: entries[i].1,
            accumulated_conns: entries[i].2,
        })
    }

    #[test]
    fn test_all_idle_all_survive() {
        let now = now_monotonic_ns();
        let snaps = make_snapshots([(now, 0, 0), (now, 0, 0), (now, 0, 0), (now, 0, 0)]);
        let res = schedule_from_snapshots(&snaps, now, HANG_THRESHOLD_NS);
        assert_eq!(res.after_stage1, 4);
        assert_eq!(res.after_stage3, 4, "all idle workers should survive");
        assert_eq!(res.bitmap, 0b1111);
    }

    #[test]
    fn test_hung_worker_filtered_stage1() {
        let now = now_monotonic_ns();
        let old = now - HANG_THRESHOLD_NS - 1;
        let snaps = make_snapshots([(old, 0, 0), (now, 0, 0), (now, 0, 0), (now, 0, 0)]);
        let res = schedule_from_snapshots(&snaps, now, HANG_THRESHOLD_NS);
        assert_eq!(res.after_stage1, 3, "hung worker must be dropped at stage 1");
        assert!(res.bitmap & 1 == 0, "worker 0 bit must be clear");
    }

    #[test]
    fn test_cold_start_zero_timestamp_treated_as_alive() {
        let now = now_monotonic_ns();
        let snaps = make_snapshots([(0, 0, 0), (now, 0, 0), (now, 0, 0), (now, 0, 0)]);
        let res = schedule_from_snapshots(&snaps, now, HANG_THRESHOLD_NS);
        assert_eq!(res.after_stage1, 4, "cold-start worker must not be marked hung");
    }

    #[test]
    fn test_high_conn_worker_filtered_stage2() {
        let now = now_monotonic_ns();
        // worker 3 has 100 conns, avg = 25.75, θ = 12.875, baseline ~ 38.6
        let snaps = make_snapshots([(now, 0, 1), (now, 0, 1), (now, 0, 1), (now, 0, 100)]);
        let res = schedule_from_snapshots(&snaps, now, HANG_THRESHOLD_NS);
        assert!(res.bitmap & (1 << 3) == 0, "overloaded worker 3 must be filtered");
        assert_eq!(res.after_stage2, 3);
    }

    #[test]
    fn test_high_events_worker_filtered_stage3() {
        let now = now_monotonic_ns();
        let snaps = make_snapshots([(now, 0, 0), (now, 0, 0), (now, 50, 0), (now, 0, 0)]);
        let res = schedule_from_snapshots(&snaps, now, HANG_THRESHOLD_NS);
        assert!(res.bitmap & (1 << 2) == 0, "busy worker 2 must be filtered at stage 3");
    }

    #[test]
    fn test_theta_floor_keeps_equal_zero_loads() {
        // all metrics zero, θ floors at THETA_MIN so everyone passes
        let candidates: Vec<usize> = (0..4).collect();
        let result = filter_below_baseline(&candidates, |_| 0);
        assert_eq!(result.len(), 4, "zero-load workers must all pass");
    }

    #[test]
    fn test_theta_scales_with_average() {
        // loads [10, 10, 10, 14] give avg = 11, θ = 5.5, baseline = 16.5,
        // so all pass despite worker 3 being above average
        let loads = [10i64, 10, 10, 14];
        let candidates: Vec<usize> = (0..4).collect();
        let result = filter_below_baseline(&candidates, |i| loads[i]);
        assert_eq!(result.len(), 4, "mildly above-average worker must survive θ margin");
    }
}
