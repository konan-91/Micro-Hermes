/// Worker event loop — stand-in for the real epoll event loop (Fig. 9).
///
/// Faithfully implements all instrumentation hooks from Fig. 9:
///   line 12 — shm_avail_update  (last_loop_entry timestamp)
///   line 14 — shm_busy_count(+N) (pending_events += event_count)
///   line 18 — shm_busy_count(-1) (pending_events -= 1 per handled event)
///   line 20 — schedule_and_sync() (run Algorithm 1, update bitmap)
///   line 25 — shm_conn_count(+1) (accumulated_conns++)
///   line 37 — shm_conn_count(-1) (accumulated_conns--)
///
/// The actual epoll/accept machinery is replaced with synthetic sleeps whose
/// duration is controlled by WorkloadConfig, allowing all four paper cases
/// to be simulated without real sockets.
///
/// The dispatcher call closes the loop: every iteration the worker runs
/// Algorithm 1 and then asks the dispatcher which worker would receive the
/// next synthetic connection under the current policy.  This demonstrates
/// that scheduling decisions actually change where load goes — the gap
/// the previous version left open.

use crate::dispatcher::{dispatch, next_conn_hash};
use crate::metrics::{print_tick, MetricsAccumulator, MetricsRow};
use crate::scheduler::{schedule, Policy, HANG_THRESHOLD_NS, THETA};
use crate::workload::WorkloadConfig;
use crate::wst::{now_monotonic_ns, Wst};
use std::sync::atomic::Ordering;
use std::thread;


/// Run one worker's event loop.
///
/// `metrics` is a pointer to shared storage written by the parent before
/// forking.  In Phase 1 we don't need cross-process metrics sharing (each
/// worker writes its own CSV rows and they're merged post-hoc), so we accept
/// an Option; passing None just skips CSV accumulation.
pub fn worker_loop(
    wst: &'static Wst,
    worker_id: usize,
    config: WorkloadConfig,
    policy: Policy,
    accumulator: Option<&'static MetricsAccumulator>,
) {
    let slot = wst.slot(worker_id);

    for iter in 0..config.iterations {
        // ── Fig. 9, line 12: shm_avail_update ────────────────────────────────
        slot.last_loop_entry.store(now_monotonic_ns(), Ordering::Relaxed);

        // ── Hang injection: simulate a stuck worker to exercise Stage 1 ───────
        if let Some(hang_iter) = config.hang_at_iter {
            if iter == hang_iter {
                eprintln!(
                    "[w{worker_id}] injecting hang for {}ms at iter {iter}",
                    config.hang_duration.as_millis()
                );
                thread::sleep(config.hang_duration);
                // Re-stamp after waking so the worker recovers naturally.
                slot.last_loop_entry.store(now_monotonic_ns(), Ordering::Relaxed);
            }
        }

        // ── Fig. 9, line 13: epoll_wait() — here we just decide event count ──
        let event_count = config.events_per_batch;

        // ── Fig. 9, line 14: shm_busy_count(+N) ─────────────────────────────
        slot.pending_events.fetch_add(event_count, Ordering::Relaxed);

        // ── Fig. 9, lines 16-18: handle each event ───────────────────────────
        for event_idx in 0..event_count {
            let per_event_time = config.processing_time.sample(
                // Seed with worker, iteration, and event index for variety.
                (worker_id as u64).wrapping_mul(1000)
                    .wrapping_add(iter as u64 * 100)
                    .wrapping_add(event_idx as u64),
            );
            thread::sleep(per_event_time);

            // ── Fig. 9, line 18: shm_busy_count(-1) ─────────────────────────
            slot.pending_events.fetch_sub(1, Ordering::Relaxed);
        }

        // ── Fig. 9, line 25: shm_conn_count(+1) ─────────────────────────────
        slot.accumulated_conns.fetch_add(1, Ordering::Relaxed);

        // Occasionally close a connection (Fig. 9 line 37).
        if iter % 4 == 3 {
            let current = slot.accumulated_conns.load(Ordering::Relaxed);
            if current >= 1 {
                slot.accumulated_conns.fetch_sub(1, Ordering::Relaxed);
            }
        }

        // ── Fig. 9, line 20: schedule_and_sync() ────────────────────────────
        // Run Algorithm 1 to produce the candidate bitmap.
        let result = schedule(wst, HANG_THRESHOLD_NS, THETA, policy);

        // ── Dispatcher: close the loop ───────────────────────────────────────
        // Ask the userspace dispatcher which worker would receive the next
        // synthetic connection.  In Phase 2 this bitmap goes into MSel and the
        // eBPF kernel module does the selection; here we mirror that logic.
        let conn_hash = next_conn_hash();
        let dispatched_to = dispatch(&result, conn_hash, policy);

        // Print per-iteration summary.
        print_tick(worker_id, iter, &result, Some(dispatched_to), policy);

        // Accumulate metrics for CSV output.
        if let Some(acc) = accumulator {
            acc.push(MetricsRow {
                timestamp_ns: now_monotonic_ns(),
                worker_id,
                iter,
                result: result.clone(),
                dispatched_to: Some(dispatched_to),
                policy,
            });
        }
    }
}
