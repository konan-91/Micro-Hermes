//! Worker event loop — userspace analogue of the instrumented epoll loop
//! (Fig. 9). One forked process per worker, run-to-completion.
//!
//! Mapping of the simulation onto Fig. 9's hooks:
//!   line 12 — shm_avail_update      → stamp last_loop_entry at loop entry
//!   line 13 — epoll_wait(timer=5ms) → poll_accept_queue (blocks ≤ 5 ms)
//!   line 14 — shm_busy_count(+N)    → pending_events += batch size
//!   line 25 — accept + conn_count(+1) → pop ConnDesc, accumulated_conns += 1
//!   handler — process the request   → sleep(service_us)
//!   line 18 — shm_busy_count(-1)    → pending_events -= 1 per handled event
//!   line 37 — close + conn_count(-1)→ lifetime expiry, accumulated_conns -= 1
//!   line 20 — schedule_and_sync()   → Algorithm 1 + M_Sel write, END of loop
//!
//! A worker's load comes exclusively from what the dispatcher put in its
//! accept queue — the feedback loop is real: Algorithm 1's bitmap changes
//! which queue the next connection lands in.
//!
//! Phase 2: `poll_accept_queue` becomes a real epoll_wait over a
//! SO_REUSEPORT listen socket and the sleeps become real request handling;
//! the instrumentation lines and `schedule_and_sync` carry over unchanged.

use crate::dispatcher::Policy;
use crate::metrics::{ConnRow, MetricsShards, TickRow};
use crate::scheduler::{schedule, HANG_THRESHOLD_NS};
use crate::shm::{ConnDesc, ConnQueue, SharedState};
use crate::workload::HangSpec;
use crate::wst::now_monotonic_ns;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

/// Hermes epoll_wait timeout (§4.2): guarantees the loop — and thus hang
/// detection and the scheduler — runs at least every 5 ms even with zero
/// traffic. This timer is part of Hermes's instrumentation, not baseline
/// behavior.
const HERMES_EPOLL_TIMEOUT: Duration = Duration::from_millis(5);
/// Baseline workers have no scheduler to keep live, so they model vanilla
/// epoll_wait(-1): block until work arrives (the poll also wakes on
/// shutdown, so this effectively never expires within a run). This matters
/// for LIFO fidelity: a blocked worker keeps its wait-queue position
/// (last_loop_entry stays put), so the most-recently-active worker stays at
/// the head and concentrates load persistently — the epoll-exclusive
/// pathology the paper describes. Any periodic idle timer here re-inserts
/// workers at the head on each expiry; because forked workers start in
/// phase, that rotates the concentration target in lock-step and
/// artificially evens out per-worker totals (observed with a 1 s timer).
const BASELINE_EPOLL_TIMEOUT: Duration = Duration::from_secs(30);
/// Max events returned per simulated epoll_wait call (MAX_EVENTS in Fig. 9).
/// Small relative to real epoll loops because our simulated per-event cost
/// is ms-scale sleeps (real L7 events are µs-scale): a large batch would
/// stretch one loop iteration past the hang threshold and starve the
/// scheduler of fresh WST data, breaking the paper's "scheduler frequency
/// scales with load" property (§5.2.2).
const MAX_EVENTS: usize = 4;
/// Granularity of the queue-polling sleep inside poll_accept_queue.
const POLL_SLEEP: Duration = Duration::from_micros(200);

pub fn worker_loop(
    shared: &'static SharedState,
    worker_id: usize,
    hang: Option<HangSpec>,
    policy: Policy,
    tick_shard: &Path,
    conn_shard: &Path,
    verbose: bool,
) {
    let slot = shared.wst.slot(worker_id);
    let queue = &shared.queues[worker_id];
    let start = Instant::now();
    let mut shards = MetricsShards::new();
    // Close deadlines (monotonic ns) of connections this worker holds open.
    let mut open_conns: Vec<i64> = Vec::new();
    let mut hang_pending = hang.filter(|h| h.worker_id == worker_id);
    let mut iter: u32 = 0;

    loop {
        // ── Fig. 9 line 12: shm_avail_update ─────────────────────────────
        slot.last_loop_entry.store(now_monotonic_ns(), Ordering::Relaxed);

        // ── Hang injection: block before epoll_wait without re-stamping,
        // exactly what a worker stuck in a handler looks like to Stage 1.
        if let Some(h) = hang_pending {
            if start.elapsed() >= h.at {
                eprintln!(
                    "[w{worker_id}] injecting {}ms hang at t={:?}",
                    h.duration.as_millis(),
                    start.elapsed()
                );
                thread::sleep(h.duration);
                hang_pending = None;
                // Recover naturally: re-enter the loop, which re-stamps.
                continue;
            }
        }

        // ── Fig. 9 line 37: close connections whose lifetime expired ─────
        let now = now_monotonic_ns();
        open_conns.retain(|&deadline| {
            if deadline <= now {
                slot.accumulated_conns.fetch_sub(1, Ordering::Relaxed);
                false
            } else {
                true
            }
        });

        // ── Fig. 9 line 13: epoll_wait(timer = 5ms) ──────────────────────
        let timeout = if policy == Policy::Hermes {
            HERMES_EPOLL_TIMEOUT
        } else {
            BASELINE_EPOLL_TIMEOUT
        };
        let batch = poll_accept_queue(shared, queue, timeout);

        // ── Fig. 9 line 14: shm_busy_count(+event_num) ───────────────────
        slot.pending_events.fetch_add(batch.len() as i64, Ordering::Relaxed);

        // ── Fig. 9 lines 16-18: handle each event ────────────────────────
        for desc in &batch {
            // accept_handler: line 25, shm_conn_count(+1).
            slot.accumulated_conns.fetch_add(1, Ordering::Relaxed);
            let dequeue_ns = now_monotonic_ns();

            // The L7 work itself (SSL, compression, ...) — simulated.
            thread::sleep(Duration::from_micros(desc.service_us as u64));
            let done_ns = now_monotonic_ns();

            // Fig. 9 line 18: shm_busy_count(-1).
            slot.pending_events.fetch_sub(1, Ordering::Relaxed);

            // Connection stays open for its lifetime, then closes (line 37).
            open_conns.push(done_ns + desc.lifetime_ms as i64 * 1_000_000);

            shards.conns.push(ConnRow {
                conn_id: desc.conn_id,
                worker_id,
                arrival_ns: desc.arrival_ns,
                dequeue_ns,
                done_ns,
                service_us: desc.service_us,
                policy,
            });
        }

        // ── Fig. 9 line 20: schedule_and_sync(), END of loop ─────────────
        // Hermes only — the baselines have no userspace scheduler at all.
        let result = if policy == Policy::Hermes {
            let result = schedule(&shared.wst, now_monotonic_ns(), HANG_THRESHOLD_NS);
            shared.msel.store(result.bitmap);
            Some(result)
        } else {
            None
        };

        let tick = TickRow {
            timestamp_ns: now_monotonic_ns(),
            worker_id,
            iter,
            snapshots: shared.wst.snapshot_all(),
            queue_len: queue.len(),
            result,
            policy,
        };
        if verbose {
            tick.print();
        }
        shards.ticks.push(tick);

        if shared.shutdown_requested() && queue.is_empty() {
            break;
        }
        iter = iter.wrapping_add(1);
    }

    if let Err(e) = shards.write(tick_shard, conn_shard) {
        eprintln!("[w{worker_id}] failed to write metrics shards: {e}");
    }
}

/// Simulated epoll_wait on the accept queue: returns as soon as at least one
/// event is ready (draining up to MAX_EVENTS, like a real ready-list sweep),
/// or an empty batch after `timeout`. Also wakes on shutdown so baseline
/// workers with long timeouts exit promptly (harness plumbing, not Fig. 9).
fn poll_accept_queue(
    shared: &SharedState,
    queue: &ConnQueue,
    timeout: Duration,
) -> Vec<ConnDesc> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut batch = Vec::new();
        while batch.len() < MAX_EVENTS {
            match queue.pop() {
                Some(desc) => batch.push(desc),
                None => break,
            }
        }
        if !batch.is_empty() || Instant::now() >= deadline || shared.shutdown_requested() {
            return batch;
        }
        thread::sleep(POLL_SLEEP);
    }
}
