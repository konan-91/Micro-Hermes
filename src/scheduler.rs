use crate::wst::{now_monotonic_ns, Wst, WorkerSlot, NUM_WORKERS};
use std::sync::atomic::Ordering;

/// If a worker hasn't re-entered its event loop within this many
/// nanoseconds, the scheduler treats it as hung (§5.2.2, `FilterTime`
/// in Algorithm 1).
pub const HANG_THRESHOLD_NS: i64 = 200_000_000; // 200ms

/// The θ/Avg ratio used in the second- and third-level filters
/// (§5.2.2, `FilterCount`). The paper sweeps this from 0 to 1 (Fig. 15)
/// and finds 0.5 gives the best latency/throughput trade-off -- a good
/// candidate experiment to reproduce yourself in your Evaluation chapter.
pub const THETA_RATIO: f64 = 0.5;

/// Implements Algorithm 1: cascading worker filtering.
///
/// Returns a bitmap where bit `i` set means worker `i` survived all three
/// filtering stages and is a scheduling candidate. In the full system,
/// this bitmap is exactly the value that gets written into the eBPF map
/// `MSel` (§5.4) for the kernel dispatcher to read -- we just print it
/// for now, since the eBPF side doesn't exist yet.
pub fn schedule(wst: &Wst, hang_threshold_ns: i64, theta_ratio: f64) -> u64 {
    let now = now_monotonic_ns();

    // Level 1 (Algo. 1, line 4): drop workers that look hung.
    let mut candidates: Vec<usize> = (0..NUM_WORKERS)
        .filter(|&i| {
            let t = wst.slot(i).last_loop_entry.load(Ordering::Relaxed);
            now - t < hang_threshold_ns
        })
        .collect();

    // Level 2 (line 5): drop workers with above-average accumulated
    // connections. The paper filters on conn before event -- it
    // prioritises avoiding future overload from long-lived connections
    // over minimising immediate processing delay (§5.2.2).
    candidates = filter_below_baseline(wst, &candidates, theta_ratio, |slot| {
        slot.accumulated_conns.load(Ordering::Relaxed)
    });

    // Level 3 (line 6): drop workers with above-average pending events.
    candidates = filter_below_baseline(wst, &candidates, theta_ratio, |slot| {
        slot.pending_events.load(Ordering::Relaxed)
    });

    candidates.iter().fold(0u64, |bitmap, &i| bitmap | (1 << i))
}

/// `FilterCount` from Algorithm 1: keep workers whose metric is below
/// `avg * (1 + theta_ratio)`.
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
