//! Metrics pipeline — structured CSV output for offline analysis/plotting.
//!
//! Two row types, two output files:
//!
//!   ticks CSV (METRICS_PATH, default metrics.csv) — one row per worker
//!   event-loop iteration: WST snapshot, queue depth, and (Hermes only) the
//!   Algorithm-1 stage survivors + bitmap. Drives balance-over-time plots
//!   (conn/event standard deviation, the paper's Fig. 13 metric).
//!
//!   conns CSV (CONNS_PATH, default conns.csv) — one row per completed
//!   connection: arrival → dequeue → done timestamps. latency_us
//!   (done - arrival, i.e. queue wait + service) drives the P99-latency
//!   comparisons (paper Table 5 / §10).
//!
//! Collection plumbing: each worker buffers rows locally and writes private
//! headerless shard files when its loop ends; the parent merges shards after
//! waitpid. Deliberately *not* shared memory — a Vec's heap buffer isn't in
//! the MAP_SHARED region after fork(), and std Mutexes are UB across
//! processes on macOS (os_unfair_lock EINVAL panics). Mirrors the WST's
//! "each worker writes only its own column" partitioning, but with files.

use crate::dispatcher::Policy;
use crate::scheduler::ScheduleResult;
use crate::wst::{WorkerSnapshot, NUM_WORKERS};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

/// One row per worker event-loop iteration.
#[derive(Debug, Clone)]
pub struct TickRow {
    pub timestamp_ns: i64,
    pub worker_id: usize,
    pub iter: u32,
    pub snapshots: [WorkerSnapshot; NUM_WORKERS],
    pub queue_len: usize,
    /// Algorithm-1 output — None for the baselines (no userspace scheduler).
    pub result: Option<ScheduleResult>,
    pub policy: Policy,
}

/// One row per completed event: the initial request of a new connection
/// (kind = "accept") or a follow-up request on an established connection
/// during a synchronized burst (kind = "burst", Case 5 only). For burst
/// rows, arrival_ns is the moment the burst fired, so latency_us measures
/// how long the follow-up waited behind its siblings on the owning worker.
#[derive(Debug, Clone)]
pub struct ConnRow {
    pub conn_id: u64,
    pub worker_id: usize,
    pub arrival_ns: i64,
    pub dequeue_ns: i64,
    pub done_ns: i64,
    pub service_us: u32,
    pub policy: Policy,
    pub kind: &'static str,
}

/// Per-worker row buffers, flushed to shard files when the worker exits.
pub struct MetricsShards {
    pub ticks: Vec<TickRow>,
    pub conns: Vec<ConnRow>,
}

impl MetricsShards {
    pub fn new() -> Self {
        MetricsShards { ticks: Vec::new(), conns: Vec::new() }
    }

    pub fn write(&self, tick_path: &Path, conn_path: &Path) -> std::io::Result<()> {
        write_lines(tick_path, self.ticks.iter().map(format_tick_row))?;
        write_lines(conn_path, self.conns.iter().map(format_conn_row))
    }
}

fn write_lines(path: &Path, lines: impl Iterator<Item = String>) -> std::io::Result<()> {
    let file = OpenOptions::new().write(true).create(true).truncate(true).open(path)?;
    let mut w = BufWriter::new(file);
    for line in lines {
        writeln!(w, "{line}")?;
    }
    w.flush()
}

pub fn tick_header() -> String {
    let mut header =
        "timestamp_ns,worker_id,iter,bitmap_hex,bitmap_bin,after_stage1,after_stage2,after_stage3,queue_len,policy"
            .to_string();
    for i in 0..NUM_WORKERS {
        header.push_str(&format!(",w{i}_conns,w{i}_events"));
    }
    header.push_str(",conn_sd,events_sd");
    header
}

pub fn conn_header() -> String {
    "conn_id,worker_id,arrival_ns,dequeue_ns,done_ns,queue_wait_us,service_us,latency_us,policy,kind"
        .to_string()
}

fn format_tick_row(row: &TickRow) -> String {
    // Baselines have no Algorithm-1 output; write zeros so the schema is
    // uniform across policies (simplifies pandas-side comparison).
    let (bitmap, s1, s2, s3) = match &row.result {
        Some(r) => (r.bitmap, r.after_stage1, r.after_stage2, r.after_stage3),
        None => (0, 0, 0, 0),
    };

    let mut line = format!(
        "{},{},{},{:#06x},{:0width$b},{},{},{},{},{}",
        row.timestamp_ns,
        row.worker_id,
        row.iter,
        bitmap,
        bitmap,
        s1,
        s2,
        s3,
        row.queue_len,
        row.policy.as_str(),
        width = NUM_WORKERS,
    );

    let conns: Vec<i64> = row.snapshots.iter().map(|s| s.accumulated_conns).collect();
    let events: Vec<i64> = row.snapshots.iter().map(|s| s.pending_events).collect();
    for i in 0..NUM_WORKERS {
        line.push_str(&format!(",{},{}", conns[i], events[i]));
    }
    line.push_str(&format!(",{:.3},{:.3}", std_dev(&conns), std_dev(&events)));
    line
}

fn format_conn_row(row: &ConnRow) -> String {
    let queue_wait_us = (row.dequeue_ns - row.arrival_ns) / 1_000;
    let latency_us = (row.done_ns - row.arrival_ns) / 1_000;
    format!(
        "{},{},{},{},{},{},{},{},{},{}",
        row.conn_id,
        row.worker_id,
        row.arrival_ns,
        row.dequeue_ns,
        row.done_ns,
        queue_wait_us,
        row.service_us,
        latency_us,
        row.policy.as_str(),
        row.kind,
    )
}

impl TickRow {
    /// One console line per iteration (enabled with VERBOSE=1).
    pub fn print(&self) {
        let fmt_metric = |f: fn(&WorkerSnapshot) -> i64| {
            self.snapshots.iter().map(|s| format!("{:>3}", f(s))).collect::<Vec<_>>().join(" ")
        };
        let sched = match &self.result {
            Some(r) => format!(
                "stages {}/{}/{} bitmap={:0width$b}",
                r.after_stage1, r.after_stage2, r.after_stage3, r.bitmap,
                width = NUM_WORKERS
            ),
            None => "(no scheduler)".to_string(),
        };
        println!(
            "[w{}][{}] iter {:>4} | conns=[{}] events=[{}] qlen={:>3} | {}",
            self.worker_id,
            self.policy.as_str(),
            self.iter,
            fmt_metric(|s| s.accumulated_conns),
            fmt_metric(|s| s.pending_events),
            self.queue_len,
            sched,
        );
    }
}

/// Merge headerless shard files into `out_path` with `header`, sorted
/// numerically by the first column (timestamp for ticks, conn_id for conns).
/// Returns the merged data lines for further summarization.
pub fn merge_shards(
    shard_paths: &[PathBuf],
    out_path: &str,
    header: &str,
) -> std::io::Result<Vec<String>> {
    let mut lines: Vec<(i64, String)> = Vec::new();
    for path in shard_paths {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => continue, // worker may have produced zero rows
        };
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            let key: i64 = line.split(',').next().unwrap_or("0").parse().unwrap_or(0);
            lines.push((key, line));
        }
    }
    lines.sort_by_key(|(key, _)| *key);

    let file = OpenOptions::new().write(true).create(true).truncate(true).open(out_path)?;
    let mut w = BufWriter::new(file);
    writeln!(w, "{header}")?;
    for (_, line) in &lines {
        writeln!(w, "{line}")?;
    }
    w.flush()?;
    Ok(lines.into_iter().map(|(_, l)| l).collect())
}

/// Latency stats computed from merged conn-CSV lines (see conn_header).
#[derive(Debug, Default)]
pub struct ConnSummary {
    pub completed: usize,
    pub completed_per_worker: [usize; NUM_WORKERS],
    pub mean_us: f64,
    pub p50_us: i64,
    pub p99_us: i64,
    pub max_us: i64,
}

pub fn summarize_conns(lines: &[String]) -> ConnSummary {
    let mut summary = ConnSummary::default();
    let mut latencies: Vec<i64> = Vec::with_capacity(lines.len());
    for line in lines {
        let fields: Vec<&str> = line.split(',').collect();
        // Columns: see conn_header — worker_id is 1, latency_us is 7.
        let (Some(worker), Some(latency)) = (
            fields.get(1).and_then(|f| f.parse::<usize>().ok()),
            fields.get(7).and_then(|f| f.parse::<i64>().ok()),
        ) else {
            continue;
        };
        if worker < NUM_WORKERS {
            summary.completed_per_worker[worker] += 1;
        }
        latencies.push(latency);
    }
    if latencies.is_empty() {
        return summary;
    }
    latencies.sort_unstable();
    summary.completed = latencies.len();
    summary.mean_us = latencies.iter().sum::<i64>() as f64 / latencies.len() as f64;
    summary.p50_us = percentile(&latencies, 0.50);
    summary.p99_us = percentile(&latencies, 0.99);
    summary.max_us = *latencies.last().unwrap();
    summary
}

/// Nearest-rank percentile over a sorted slice.
fn percentile(sorted: &[i64], p: f64) -> i64 {
    let rank = ((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

pub fn std_dev(values: &[i64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<i64>() as f64 / n;
    let variance = values.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / n;
    variance.sqrt()
}
