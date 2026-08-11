//! Tick metrics, one CSV row per worker loop iteration (WST snapshot plus
//! Algorithm 1 stage counts and bitmap). Drives the balance-over-time
//! plots (Fig. 13). Latency is measured client-side by hermes-bench, so
//! this module only produces ticks. Rows are streamed with a periodic
//! flush since the worker is a long-running server

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
    /// Connections this worker currently holds open
    pub open_conns: usize,
    /// Algorithm 1 output, None for the baselines
    pub result: Option<ScheduleResult>,
    pub policy: Policy,
}

impl TickRow {
    /// One console line per iteration (--verbose)
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
    // baselines have no Algorithm 1 output, write zeros so the schema is
    // uniform across policies
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

/// Per-worker tick writer, one file per worker, flushed every FLUSH_EVERY
/// rows so a kill -9 loses at most a fraction of a second of data
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
