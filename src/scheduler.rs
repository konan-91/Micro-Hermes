use crate::wst::{now_monotonic_ns, Wst, WorkerSlot, NUM_WORKERS};
use std::sync::atomic::Ordering;

/// The timeout limit (200ms). Workers inactive longer than this are considered hung (§5.2.2)
pub const HANG_THRESHOLD_NS: i64 = 200_000_000; // 200ms

/// The threshold ratio used for filtering workers. 
/// 0.5 provides a good balance between latency and throughput (§5.2.2).
pub const THETA_RATIO: f64 = 0.5;

/// Implements Algorithm 1: cascading worker filtering (3-stage filter).
///
/// Returns a bitmap where bit `i` set means worker `i` survived all three
/// filtering stages and is a scheduling candidate. In the full system,
/// this bitmap is exactly the value that gets written into the eBPF map
/// `MSel` (§5.4) for the kernel dispatcher to read - just print for now.
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

/// Filters out workers whose metric exceeds the baseline (average * (1 + theta_ratio)) (algo 1)
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
    let baseline = avg * (1.0 + theta_ratio);
    candidates
        .iter()
        .copied()
        .filter(|&i| (metric(wst.slot(i)) as f64) < baseline)
        .collect()
}
