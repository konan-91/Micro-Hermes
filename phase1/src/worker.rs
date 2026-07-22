//! Worker event loop: userspace analogue of the instrumented epoll loop
//! (Fig. 9). One forked process per worker, run-to-completion.
//!
//! Mapping of the simulation onto Fig. 9's hooks:
//!   line 12: shm_avail_update        -> stamp last_loop_entry at loop entry
//!   line 13: epoll_wait(timer=5ms)   -> poll_accept_queue (blocks <= 5 ms)
//!   line 14: shm_busy_count(+N)      -> pending_events += batch size
//!   line 25: accept + conn_count(+1) -> pop ConnDesc, accumulated_conns += 1
//!   handler: process the request     -> sleep(service_us)
//!   line 18: shm_busy_count(-1)      -> pending_events -= 1 per handled event
//!   line 37: close + conn_count(-1)  -> lifetime expiry, accumulated_conns -= 1
//!   line 20: schedule_and_sync()     -> Algorithm 1 + M_Sel write, END of loop
//!
//! A worker's load comes exclusively from what the dispatcher put in its
//! accept queue; the feedback loop is real: Algorithm 1's bitmap changes
//! which queue the next connection lands in.
//!
//! Phase 2: `poll_accept_queue` becomes a real epoll_wait over a
//! SO_REUSEPORT listen socket and the sleeps become real request handling;
//! the instrumentation lines and `schedule_and_sync` carry over unchanged

use crate::dispatcher::Policy;
use crate::metrics::{ConnRow, MetricsShards, TickRow};
use crate::scheduler::{schedule, HANG_THRESHOLD_NS};
use crate::shm::{ConnDesc, ConnQueue, SharedState};
use crate::workload::{BurstSpec, HangSpec};
use crate::wst::now_monotonic_ns;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

/// epoll_wait timeout, applied under every policy: the paper's LB runs a
/// 5 ms timer in all epoll modes (Fig. 5b measured it under epoll
/// exclusive). Under Hermes it additionally guarantees the loop (and thus
/// hang detection and the scheduler) runs at least every 5 ms even with
/// zero traffic (§5.3.2). Safe for the LIFO baseline because its wait-queue
/// priority is static (epoll_ctl registration order), so idle wakeups don't
/// reshuffle it
const EPOLL_TIMEOUT: Duration = Duration::from_millis(5);
/// Max events returned per simulated epoll_wait call (MAX_EVENTS in Fig. 9).
/// Small relative to real epoll loops because our simulated per-event cost
/// is ms-scale sleeps (real L7 events are µs-scale): a large batch would
/// stretch one loop iteration past the hang threshold and starve the
/// scheduler of fresh WST data, breaking the paper's "scheduler frequency
/// scales with load" property (§5.2.2)
const MAX_EVENTS: usize = 4;
/// Granularity of the queue-polling sleep inside poll_accept_queue
const POLL_SLEEP: Duration = Duration::from_micros(200);

pub fn worker_loop(
    shared: &'static SharedState,
    worker_id: usize,
    hang: Option<HangSpec>,
    burst: Option<BurstSpec>,
    policy: Policy,
    tick_shard: &Path,
    conn_shard: &Path,
    verbose: bool,
) {
    let slot = shared.wst.slot(worker_id);
    let queue = &shared.queues[worker_id];
    let start = Instant::now();
    let mut shards = MetricsShards::new();
    // (conn_id, close deadline in monotonic ns) of connections held open
    let mut open_conns: Vec<(u64, i64)> = Vec::new();
    let mut hang_pending = hang.filter(|h| h.worker_id == worker_id);
    // Synchronized burst (Case 5): once `burst.at` passes, every connection
    // this worker holds open generates one ready follow-up event
    let mut burst_pending = burst;
    let mut burst_backlog: Vec<u64> = Vec::new();
    let mut burst_trigger_ns: i64 = 0;
    let mut iter: u32 = 0;

    loop {
        // Fig. 9 line 12: shm_avail_update
        slot.last_loop_entry.store(now_monotonic_ns(), Ordering::Relaxed);

        // Hang injection: block before epoll_wait without re-stamping,
        // exactly what a worker stuck in a handler looks like to Stage 1
        if let Some(h) = hang_pending {
            if start.elapsed() >= h.at {
                eprintln!(
                    "[w{worker_id}] injecting {}ms hang at t={:?}",
                    h.duration.as_millis(),
                    start.elapsed()
                );
                thread::sleep(h.duration);
                hang_pending = None;
                // Recover naturally: re-enter the loop, which re-stamps
                continue;
            }
        }

        // Fig. 9 line 37: close connections whose lifetime expired
        let now = now_monotonic_ns();
        open_conns.retain(|&(_, deadline)| {
            if deadline <= now {
                slot.accumulated_conns.fetch_sub(1, Ordering::Relaxed);
                false
            } else {
                true
            }
        });

        // Burst trigger (Case 5): every open connection fires one
        // follow-up event, all at once. Connection affinity means only this
        // worker can process them, so they queue as its ready-event backlog
        if let Some(b) = burst_pending {
            if start.elapsed() >= b.at {
                burst_backlog = open_conns.iter().map(|&(id, _)| id).collect();
                burst_trigger_ns = now_monotonic_ns();
                eprintln!(
                    "[w{worker_id}] burst: {} follow-up events at t={:?}",
                    burst_backlog.len(),
                    start.elapsed()
                );
                burst_pending = None;
            }
        }

        // Fig. 9 line 13: epoll_wait(timer = 5ms)
        // Burst follow-ups are already-ready events, so while any remain
        // epoll_wait would return immediately with them; the accept queue
        // waits its turn, exactly like a saturated run-to-completion worker
        if !burst_backlog.is_empty() {
            let n = burst_backlog.len().min(MAX_EVENTS);
            let ids: Vec<u64> = burst_backlog.drain(..n).collect();
            // Fig. 9 line 14: shm_busy_count(+event_num)
            slot.pending_events.fetch_add(ids.len() as i64, Ordering::Relaxed);
            for conn_id in ids {
                let dequeue_ns = now_monotonic_ns();
                let service = burst.expect("backlog implies burst spec").service;
                thread::sleep(service);
                let done_ns = now_monotonic_ns();
                // Fig. 9 line 18: shm_busy_count(-1). No conn count change,
                // the connection already exists and stays open
                slot.pending_events.fetch_sub(1, Ordering::Relaxed);
                shards.conns.push(ConnRow {
                    conn_id,
                    worker_id,
                    arrival_ns: burst_trigger_ns,
                    dequeue_ns,
                    done_ns,
                    service_us: service.as_micros() as u32,
                    policy,
                    kind: "burst",
                });
            }
        } else {
            let batch = poll_accept_queue(shared, queue, EPOLL_TIMEOUT);

            // Fig. 9 line 14: shm_busy_count(+event_num)
            slot.pending_events.fetch_add(batch.len() as i64, Ordering::Relaxed);

            // Fig. 9 lines 16-18: handle each event
            for desc in &batch {
                // accept_handler: line 25, shm_conn_count(+1)
                slot.accumulated_conns.fetch_add(1, Ordering::Relaxed);
                let dequeue_ns = now_monotonic_ns();

                // The L7 work itself (SSL, compression, ...), simulated
                thread::sleep(Duration::from_micros(desc.service_us as u64));
                let done_ns = now_monotonic_ns();

                // Fig. 9 line 18: shm_busy_count(-1)
                slot.pending_events.fetch_sub(1, Ordering::Relaxed);

                // Connection stays open for its lifetime, closes at line 37
                open_conns.push((desc.conn_id, done_ns + desc.lifetime_ms as i64 * 1_000_000));

                shards.conns.push(ConnRow {
                    conn_id: desc.conn_id,
                    worker_id,
                    arrival_ns: desc.arrival_ns,
                    dequeue_ns,
                    done_ns,
                    service_us: desc.service_us,
                    policy,
                    kind: "accept",
                });
            }
        }

        // Fig. 9 line 20: schedule_and_sync(), END of loop
        // Hermes only, the baselines have no userspace scheduler at all
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

        if shared.shutdown_requested() && queue.is_empty() && burst_backlog.is_empty() {
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
/// or an empty batch after `timeout`. Also wakes on shutdown so workers exit
/// promptly (harness plumbing, not Fig. 9)
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
