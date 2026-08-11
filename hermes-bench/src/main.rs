//! Load generator. Talks real TCP to a running hermes LB under any
//! policy. See benchmark/run_case.sh for the orchestration that starts
//! the LB, runs this against it and collects both CSVs

mod client;
mod workload;

use anyhow::{Context, Result};
use clap::Parser;
use client::{csv_header, run_connection, ConnRow};
use std::io::{BufWriter, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};
use tokio::sync::mpsc;
use tokio::time::Instant as TokioInstant;
use workload::{LoadLevel, WorkloadCase, WorkloadConfig};

/// Every connection closes at min(lifetime, duration + GRACE) after
/// generator start, so runs with 60s lifetimes (Cases 3 and 5) still
/// finish promptly. GRACE just has to cover the last wave's round trip
const GRACE: Duration = Duration::from_millis(500);

#[derive(Parser, Debug)]
#[command(about = "Real-socket load generator for micro-hermes phase 2")]
struct Cli {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    #[arg(long, default_value_t = hermes_common::DEFAULT_PORT)]
    port: u16,

    /// Workload case 1|2|3|4|5, or omit for a quick smoke-test profile
    #[arg(long)]
    case: Option<String>,

    /// Load level, light|medium|heavy (defaults to the case's
    /// characteristic level, see workload.rs)
    #[arg(long)]
    load: Option<String>,

    /// Perturbs the per-connection cost sequence so repeated trials draw
    /// different but reproducible service times
    #[arg(long, default_value_t = 0)]
    seed: u64,

    /// Tag written into the label column (e.g. the policy under test)
    #[arg(long, default_value = "run")]
    label: String,

    /// Output conns CSV path
    #[arg(long, default_value = "conns.csv")]
    out: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    let cfg = match &cli.case {
        Some(c) => {
            let case = WorkloadCase::from_str(c)
                .with_context(|| format!("unknown --case '{c}' (want 1|2|3|4|5)"))?;
            match cli.load.as_deref().map(LoadLevel::from_str) {
                Some(Some(l)) => WorkloadConfig::for_case_with_load(case, l),
                Some(None) => anyhow::bail!("unknown --load '{}' (want light|medium|heavy)", cli.load.unwrap()),
                None => WorkloadConfig::for_case(case),
            }
        }
        None => WorkloadConfig::default_case(),
    };

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port)
        .parse()
        .with_context(|| format!("parsing {}:{}", cli.host, cli.port))?;
    let label: Arc<str> = Arc::from(cli.label.as_str());

    log::info!(
        "case={:?} cps={} duration={:?} lifetime={:?} burst={} -> {}",
        cli.case, cfg.cps, cfg.duration, cfg.lifetime, cfg.burst.is_some(), cli.out.display()
    );

    let (tx, rx) = mpsc::unbounded_channel::<ConnRow>();
    let out_path = cli.out.clone();
    let writer = tokio::spawn(collect(out_path, rx));

    let clock_start = StdInstant::now();
    let gen_start = TokioInstant::now();
    let interval = Duration::from_secs_f64(1.0 / cfg.cps as f64);
    let close_at = gen_start + cfg.lifetime.min(cfg.duration + GRACE);

    let mut next_arrival = gen_start;
    let mut conn_id: u64 = 0;
    while next_arrival.duration_since(gen_start) < cfg.duration {
        tokio::time::sleep_until(next_arrival).await;

        // Deterministic stand-in for the 4-tuple hash, used only to
        // sample the cost distribution. The hash Algorithm 2 actually
        // sees is computed by the kernel from the real connection
        let hash = (conn_id ^ cli.seed.wrapping_mul(0xff51afd7ed558ccd)).wrapping_mul(0x9e3779b97f4a7c15);
        let service_us = cfg.service.sample(hash).as_micros() as u32;

        // Detached tasks. collect() only returns once every task's tx
        // clone has dropped, and a panicking task still drops its clone
        // during unwind, so this can't hang
        tokio::spawn(run_connection(
            addr, conn_id, service_us, close_at, cfg.burst, clock_start, gen_start, label.clone(), tx.clone(),
        ));

        conn_id += 1;
        next_arrival += interval;
    }
    let generated = conn_id;
    drop(tx);

    let summary = writer.await??;
    log::info!(
        "generated={generated} completed={} errors={} drops={} -> {}",
        summary.completed, summary.errors, summary.drops, cli.out.display()
    );
    if summary.completed > 0 {
        println!(
            "latency (send->recv): mean={:.1}ms p50={:.1}ms p99={:.1}ms max={:.1}ms",
            summary.mean_us / 1000.0,
            summary.p50_us as f64 / 1000.0,
            summary.p99_us as f64 / 1000.0,
            summary.max_us as f64 / 1000.0,
        );
    }
    Ok(())
}

#[derive(Default)]
struct Summary {
    completed: usize,
    errors: usize,
    drops: usize,
    mean_us: f64,
    p50_us: i64,
    p99_us: i64,
    max_us: i64,
}

/// Streams rows to the CSV as they arrive and accumulates latencies for
/// the summary. Blocking file I/O is fine at these row rates
async fn collect(path: PathBuf, mut rx: mpsc::UnboundedReceiver<ConnRow>) -> Result<Summary> {
    let file = std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    let mut w = BufWriter::new(file);
    writeln!(w, "{}", csv_header())?;

    let mut latencies: Vec<i64> = Vec::new();
    let mut summary = Summary::default();
    let mut since_flush = 0u32;

    while let Some(row) = rx.recv().await {
        writeln!(w, "{}", row.to_csv_line())?;
        match row.kind {
            "drop" => summary.drops += 1,
            "error" => summary.errors += 1,
            _ => {
                summary.completed += 1;
                if let Some(l) = row.latency_us() {
                    latencies.push(l);
                }
            }
        }
        since_flush += 1;
        if since_flush >= 256 {
            w.flush()?;
            since_flush = 0;
        }
    }
    w.flush()?;

    if !latencies.is_empty() {
        latencies.sort_unstable();
        summary.mean_us = latencies.iter().sum::<i64>() as f64 / latencies.len() as f64;
        summary.p50_us = percentile(&latencies, 0.50);
        summary.p99_us = percentile(&latencies, 0.99);
        summary.max_us = *latencies.last().unwrap();
    }
    Ok(summary)
}

fn percentile(sorted: &[i64], p: f64) -> i64 {
    let rank = ((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}
