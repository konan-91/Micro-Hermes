/// Metrics pipeline — structured CSV output for offline analysis and plotting.
///
/// Each tick one MetricsRow is recorded.  At program exit the full table is
/// written to metrics.csv (appending policy: each run overwrites).
///
/// Columns mirror the quantities the Hermes paper measures (Fig. 13, Table 5):
///   timestamp_ns  — monotonic clock at scheduling time
///   worker_id     — which worker ran the scheduler this tick
///   iter          — iteration number within that worker's loop
///   bitmap        — hex scheduling result
///   after_stage1/2/3 — survivors after each cascading filter
///   w{0..N}_conns — per-worker connection count
///   w{0..N}_events — per-worker pending events
///   conn_sd       — standard deviation of connection counts (Fig. 13 metric)
///   events_sd     — standard deviation of pending events
///   dispatched_to — worker ID the dispatcher selected this tick (-1 = none)
///   policy        — "hermes" | "lifo" | "reuseport"

use crate::scheduler::{Policy, ScheduleResult};
use crate::wst::NUM_WORKERS;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::sync::Mutex;

/// One row in the metrics CSV.
#[derive(Debug, Clone)]
pub struct MetricsRow {
    pub timestamp_ns: i64,
    pub worker_id: usize,
    pub iter: u32,
    pub result: ScheduleResult,
    pub dispatched_to: Option<usize>,
    pub policy: Policy,
}

/// Thread-safe accumulator — workers append rows; main flushes at the end.
pub struct MetricsAccumulator {
    rows: Mutex<Vec<MetricsRow>>,
}

impl MetricsAccumulator {
    pub fn new() -> Self {
        MetricsAccumulator { rows: Mutex::new(Vec::new()) }
    }

    pub fn push(&self, row: MetricsRow) {
        self.rows.lock().unwrap().push(row);
    }

    /// Write all accumulated rows to `path` as CSV, sorted by timestamp.
    pub fn flush_csv(&self, path: &str) -> std::io::Result<()> {
        let mut rows = self.rows.lock().unwrap();
        rows.sort_by_key(|r| r.timestamp_ns);

        let file = OpenOptions::new().write(true).create(true).truncate(true).open(path)?;
        let mut w = BufWriter::new(file);

        // Header
        let mut header = "timestamp_ns,worker_id,iter,bitmap,after_stage1,after_stage2,after_stage3,policy,dispatched_to".to_string();
        for i in 0..NUM_WORKERS {
            header.push_str(&format!(",w{i}_conns,w{i}_events"));
        }
        header.push_str(",conn_sd,events_sd");
        writeln!(w, "{header}")?;

        // Rows
        for row in rows.iter() {
            let policy_str = match row.policy {
                Policy::Hermes => "hermes",
                Policy::Lifo => "lifo",
                Policy::ReuseportHash => "reuseport",
            };
            let dispatched = match row.dispatched_to {
                Some(id) => id.to_string(),
                None => "-1".to_string(),
            };

            let mut line = format!(
                "{},{},{},{:#010x},{},{},{},{},{}",
                row.timestamp_ns,
                row.worker_id,
                row.iter,
                row.result.bitmap,
                row.result.after_stage1,
                row.result.after_stage2,
                row.result.after_stage3,
                policy_str,
                dispatched,
            );

            let conns: Vec<i64> = (0..NUM_WORKERS)
                .map(|i| row.result.snapshots[i].accumulated_conns)
                .collect();
            let events: Vec<i64> = (0..NUM_WORKERS)
                .map(|i| row.result.snapshots[i].pending_events)
                .collect();

            for i in 0..NUM_WORKERS {
                line.push_str(&format!(",{},{}", conns[i], events[i]));
            }

            let conn_sd = std_dev(&conns);
            let events_sd = std_dev(&events);
            line.push_str(&format!(",{conn_sd:.3},{events_sd:.3}"));

            writeln!(w, "{line}")?;
        }

        w.flush()?;
        eprintln!("[metrics] wrote {} rows to {path}", rows.len());
        Ok(())
    }
}

fn std_dev(values: &[i64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<i64>() as f64 / n;
    let variance = values.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / n;
    variance.sqrt()
}

/// Pretty-print a scheduling result to stdout (used by workers each iter).
pub fn print_tick(
    worker_id: usize,
    iter: u32,
    result: &ScheduleResult,
    dispatched_to: Option<usize>,
    policy: Policy,
) {
    let policy_tag = match policy {
        Policy::Hermes => "hermes",
        Policy::Lifo => "lifo ",
        Policy::ReuseportHash => "rport",
    };
    let dispatch_str = match dispatched_to {
        Some(id) => format!("→ w{id}"),
        None => "→ --".to_string(),
    };
    println!(
        "[w{worker_id}][{policy_tag}] iter {iter:>3} | \
         conns=[{}] events=[{}] | \
         stages: {}/{}/{} | \
         bitmap={:0width$b} {dispatch_str}",
        (0..crate::wst::NUM_WORKERS)
            .map(|i| format!("{:>2}", result.snapshots[i].accumulated_conns))
            .collect::<Vec<_>>()
            .join(" "),
        (0..crate::wst::NUM_WORKERS)
            .map(|i| format!("{:>2}", result.snapshots[i].pending_events))
            .collect::<Vec<_>>()
            .join(" "),
        result.after_stage1,
        result.after_stage2,
        result.after_stage3,
        result.bitmap,
        width = crate::wst::NUM_WORKERS,
    );
}
