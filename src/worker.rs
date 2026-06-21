use crate::scheduler::{schedule, HANG_THRESHOLD_NS, THETA_RATIO};
use crate::wst::{now_monotonic_ns, Wst, NUM_WORKERS};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

/// Simulates one worker's epoll event loop. The `+` lines from Fig. 9 in
/// the paper -- the actual Hermes instrumentation -- are implemented for
/// real here; the surrounding epoll/accept machinery is replaced with a
/// synthetic workload, so we can exercise the scheduler end-to-end before
/// the eBPF dispatcher exists (that's next week's job).
pub fn worker_loop(wst: &'static Wst, worker_id: usize, iterations: u32) {
    let slot = wst.slot(worker_id);

    // Worker 0 simulates a CPU-heavy workload (think TLS termination or
    // compression -- paper §6.2, Case 2) so its metrics diverge from the
    // others and we can watch the scheduler correctly route around it.
    let per_event_work = if worker_id == 0 {
        Duration::from_millis(40)
    } else {
        Duration::from_millis(2)
    };

    for iter in 0..iterations {
        // epoll_wait() equivalent: record loop entry, then "receive" events.
        // (Fig. 9, line 12: shm_avail_update / line 13: epoll_wait)
        slot.last_loop_entry
            .store(now_monotonic_ns(), Ordering::Relaxed);
        let event_count = 2 + (worker_id as i64 % 3);
        slot.pending_events
            .fetch_add(event_count, Ordering::Relaxed); // line 14

        // Handle each event (Fig. 9, lines 16-19).
        for _ in 0..event_count {
            thread::sleep(per_event_work);
            slot.pending_events.fetch_sub(1, Ordering::Relaxed); // line 18
        }

        // Simulate a connection accept, with an occasional close.
        // (Fig. 9, line 25 / line 37)
        slot.accumulated_conns.fetch_add(1, Ordering::Relaxed);
        if iter % 4 == 3 {
            // Only close if we actually have connections to close
            let current = slot.accumulated_conns.load(Ordering::Relaxed);
            if current >= 1 {
                slot.accumulated_conns.fetch_sub(1, Ordering::Relaxed);
            }
        }

        // schedule_and_sync() (Fig. 9, line 20 / Algorithm 1).
        let bitmap = schedule(wst, HANG_THRESHOLD_NS, THETA_RATIO);
        println!(
            "[worker {worker_id}] iter {iter:>3} | busy={:>3} conn={:>3} | candidates={:0width$b}",
            slot.pending_events.load(Ordering::Relaxed),
            slot.accumulated_conns.load(Ordering::Relaxed),
            bitmap,
            width = NUM_WORKERS,
        );
    }
}
