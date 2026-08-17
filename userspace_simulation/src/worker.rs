//! Worker event loop, one forked process per worker, run to completion.
//! Userspace analogue of the instrumented epoll loop from Fig. 9, with
//! the accept queue standing in for epoll_wait and sleeps standing in for
//! request handling. A worker's load comes exclusively from what the
//! dispatcher put in its queue, so the feedback loop is real, Algorithm
//! 1's bitmap changes where the next connection lands

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

/// 5ms epoll timeout under every policy (Fig. 5b), so the loop and hence
/// hang detection and the scheduler run even with zero traffic (§5.3.2)
const EPOLL_TIMEOUT: Duration = Duration::from_millis(5);
/// Max events per simulated epoll_wait. Small because the simulated
/// per-event cost is ms-scale sleeps, a large batch would stretch one
/// iteration past the hang threshold and starve the scheduler
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
    // Case 5, once burst.at passes every connection this worker holds
    // open generates one ready follow-up event
    let mut burst_pending = burst;
    let mut burst_backlog: Vec<u64> = Vec::new();
    let mut burst_trigger_ns: i64 = 0;
    let mut iter: u32 = 0;

    loop {
        // stamp loop entry (Fig. 9 line 12)
        slot.last_loop_entry.store(now_monotonic_ns(), Ordering::Relaxed);

        // Block before epoll_wait without re-stamping, which is exactly
        // what a worker stuck in a handler looks like to Stage 1
        if let Some(h) = hang_pending {
            if start.elapsed() >= h.at {
                eprintln!(
                    "[w{worker_id}] injecting {}ms hang at t={:?}",
                    h.duration.as_millis(),
                    start.elapsed()
                );
                thread::sleep(h.duration);
                hang_pending = None;
                // recover naturally, re-entering the loop re-stamps
                continue;
            }
        }

        // close connections whose lifetime expired (Fig. 9 line 37)
        let now = now_monotonic_ns();
        open_conns.retain(|&(_, deadline)| {
            if deadline <= now {
                slot.accumulated_conns.fetch_sub(1, Ordering::Relaxed);
                false
            } else {
                true
            }
        });

        // Case 5 burst, every open connection fires one follow-up at
        // once. Connection affinity means only this worker can process
        // them, so they queue as its ready-event backlog
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

        // Burst follow-ups are already-ready events, so while any remain
        // epoll_wait would return immediately with them and the accept
        // queue waits its turn
        if !burst_backlog.is_empty() {
            let n = burst_backlog.len().min(MAX_EVENTS);
            let ids: Vec<u64> = burst_backlog.drain(..n).collect();
            // busy count += batch size (Fig. 9 line 14)
            slot.pending_events.fetch_add(ids.len() as i64, Ordering::Relaxed);
            for conn_id in ids {
                let dequeue_ns = now_monotonic_ns();
                let service = burst.expect("backlog implies burst spec").service;
                thread::sleep(service);
                let done_ns = now_monotonic_ns();
                // busy count -= 1, no conn count change since the
                // connection already exists and stays open
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

            // busy count += batch size (Fig. 9 line 14)
            slot.pending_events.fetch_add(batch.len() as i64, Ordering::Relaxed);

            for desc in &batch {
                // conn count += 1 (Fig. 9 line 25)
                slot.accumulated_conns.fetch_add(1, Ordering::Relaxed);
                let dequeue_ns = now_monotonic_ns();

                // The L7 work itself (SSL, compression, ...), simulated
                thread::sleep(Duration::from_micros(desc.service_us as u64));
                let done_ns = now_monotonic_ns();

                // busy count -= 1 (Fig. 9 line 18)
                slot.pending_events.fetch_sub(1, Ordering::Relaxed);

                // connection stays open for its lifetime, closes at line 37
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

        // end of loop, run Algorithm 1 and publish M_Sel (hermes only)
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

/// Simulated epoll_wait on the accept queue. Returns as soon as at least
/// one event is ready (up to MAX_EVENTS) or an empty batch after timeout.
/// Also wakes on shutdown so workers exit promptly
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
