/// Implements Algorithm 1. Three-stage cascading filter over the WST: drop hung workers
/// (stale timestamp), drop above-average connection count, drop above-average pending events.
/// Returns a u64 bitmap: later this will instead go into a MSel eBPF map so the kernel can see
/// which workers are available.

use crate::wst::{now_monotonic_ns, Wst, WorkerSlot, NUM_WORKERS};
use std::sync::atomic::Ordering;

/// The timeout limit (200ms). Workers inactive longer than this are considered hung (§5.2.2)
pub const HANG_THRESHOLD_NS: i64 = 200_000_000; // 200ms

/// The threshold additive offset used for filtering workers.
/// Using an additive offset (matching Algorithm 1, line 13) prevents the candidate set
/// from collapsing to 0000 on startup when all metrics are 0.
pub const THETA_RATIO: f64 = 2.0;

/// Implements Algorithm 1: cascading worker filtering (3-stage filter).
///
/// Returns a bitmap where bit `i` set means worker `i` survived all three
/// filtering stages and is a scheduling candidate. In the full system,
/// this bitmap is exactly the value that gets written into the eBPF map
/// `MSel` (§5.4) for the kernel dispatcher to read. Just print for now.
pub fn schedule(wst: &Wst, hang_threshold_ns: i64, theta_ratio: f64) -> u64 {
    let now = now_monotonic_ns();

    // Stage 1: drop workers that look hung.
    let mut candidates: Vec<usize> = (0..NUM_WORKERS)
        .filter(|&i| {
            let t = wst.slot(i).last_loop_entry.load(Ordering::Relaxed);
            now - t < hang_threshold_ns
        })
        .collect();

    // Stage 2: Exclude workers with an above-average number of connections.
    candidates = filter_below_baseline(wst, &candidates, theta_ratio, |slot| {
        slot.accumulated_conns.load(Ordering::Relaxed)
    });

    // Stage 3: Exclude workers with an above-average number of pending events.
    candidates = filter_below_baseline(wst, &candidates, theta_ratio, |slot| {
        slot.pending_events.load(Ordering::Relaxed)
    });

    candidates.iter().fold(0u64, |bitmap, &i| bitmap | (1 << i))
}

/// Filters out workers whose metric exceeds the baseline (average + theta) (algo 1)
fn filter_below_baseline(
    wst: &Wst,
    candidates: &[usize],
    theta_ratio: f64,
    metric: impl Fn(&WorkerSlot) -> i64,
) -> Vec<usize> {
    if candidates.is_empty() {
        return Vec::new();
    }
    
    let sum: i64 = candidates.iter().map(|&i| metric(wst.slot(i))).sum();
    let avg = sum as f64 / candidates.len() as f64;
    
    // Fixed: Uses an additive offset parameter matching the paper's Algorithm 1.
    // If avg = 0 and theta_ratio = 2.0, baseline is 2.0. Idle workers (0.0) safely pass.
    let baseline = avg + theta_ratio; 
    
    candidates
        .iter()
        .copied()
        .filter(|&i| (metric(wst.slot(i)) as f64) < baseline)
        .collect()
}