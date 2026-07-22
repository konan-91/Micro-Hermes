//! Tick metrics: one CSV row per worker event-loop iteration (WST snapshot,
//! Algorithm-1 stage survivors + bitmap). Drives the balance-over-time
//! plots (paper Fig. 13) exactly like phase 1's `metrics.rs`.
//!
//! What's different from phase 1: connection *latency* is no longer
//! measured here at all. In phase 1 the worker was the only thing that
//! could see arrival/dequeue/done timestamps (everything lived in one
//! mmap). In phase 2 the client (`hermes-bench`) makes a real TCP
//! connection and can time its own request/response round trip directly —
//! a strictly better measurement (it's what the connection's actual user
//! experiences), so `hermes-bench` owns the conns CSV entirely and this
//! module only ever produces ticks.
//!
//! Streaming, not buffered: phase 1's worker ran for a fixed benchmark
//! duration and wrote its shard once at exit. This worker is a long-running
//! server, so `TickWriter` opens its file up front and appends one line per
//! iteration with a periodic flush, rather than accumulating an unbounded
//! `Vec` for the lifetime of the process.

use crate::loader::Policy;
use crate::scheduler::ScheduleResult;
use crate::wst::{WorkerSnapshot, NUM_WORKERS};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct TickRow {
    pub timestamp_ns: i64,
    pub worker_id: usize,
    pub iter: u32,
    pub snapshots: [WorkerSnapshot; NUM_WORKERS],
    /// Connections this worker currently holds open (real socket count,
    /// replaces phase 1's simulated `queue_len`).
    pub open_conns: usize,
    /// Algorithm-1 output, `None` for the baselines (no userspace scheduler).
    pub result: Option<ScheduleResult>,
    pub policy: Policy,
}

impl TickRow {
    /// One console line per iteration (enabled with `--verbose`).
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
            "[w{}][{}] iter {:>4} | conns=[{}] events=[{}] open={:>3} | {}",
            self.worker_id,
            self.policy.as_str(),
            self.iter,
            fmt_metric(|s| s.accumulated_conns),
            fmt_metric(|s| s.pending_events),
            self.open_conns,
            sched,
        );
    }
}

pub fn tick_header() -> String {
    let mut header =
        "timestamp_ns,worker_id,iter,bitmap_hex,bitmap_bin,after_stage1,after_stage2,after_stage3,open_conns,policy"
            .to_string();
    for i in 0..NUM_WORKERS {
        header.push_str(&format!(",w{i}_conns,w{i}_events"));
    }
    header.push_str(",conn_sd,events_sd");
    header
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
        row.open_conns,
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

/// Streaming per-worker tick writer: one file per worker (`w{id}_ticks.csv`,
/// see `main.rs`), flushed every `FLUSH_EVERY` rows so a `kill -9` during a
/// long run loses at most a fraction of a second of data, not the whole file.
pub struct TickWriter {
    writer: BufWriter<File>,
    pending: u32,
}

const FLUSH_EVERY: u32 = 64;

impl TickWriter {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        writeln!(writer, "{}", tick_header())?;
        Ok(Self { writer, pending: 0 })
    }

    pub fn write(&mut self, row: &TickRow) -> std::io::Result<()> {
        writeln!(self.writer, "{}", format_tick_row(row))?;
        self.pending += 1;
        if self.pending >= FLUSH_EVERY {
            self.writer.flush()?;
            self.pending = 0;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
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
