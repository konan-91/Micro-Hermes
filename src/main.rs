//! Micro-Hermes Phase 1: userspace simulation of the Hermes load balancer.
//!
//! Process layout mirrors the real system's division of labor:
//!   parent  = kernel stand-in — paces connection arrivals (workload CPS),
//!             runs the dispatch mechanism under test (Algorithm 2 /
//!             reuseport hash / epoll-exclusive LIFO), pushes each
//!             connection into the chosen worker's accept queue.
//!   children = one forked worker per slot, each running the instrumented
//!             event loop (Fig. 9) and — under the Hermes policy — the
//!             Algorithm-1 scheduler that feeds the M_Sel bitmap back.
//!
//! All cross-process state (WST, M_Sel, accept queues) lives in one
//! mmap(MAP_SHARED | MAP_ANONYMOUS) region — see shm.rs.
//!
//! Environment variables:
//!   POLICY         — hermes|lifo|reuseport            (default: hermes)
//!   WORKLOAD_CASE  — 1|2|3|4|default                  (default: default)
//!   METRICS_PATH   — per-iteration tick CSV           (default: metrics.csv)
//!   CONNS_PATH     — per-connection latency CSV       (default: conns.csv)
//!   VERBOSE        — 1 to print every worker loop iteration

mod dispatcher;
mod generator;
mod metrics;
mod scheduler;
mod shm;
mod worker;
mod workload;
mod wst;

use dispatcher::Policy;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use workload::{WorkloadCase, WorkloadConfig};
use wst::NUM_WORKERS;

fn main() {
    // ── Parse environment config ───────────────────────────────────────────
    let case_str = std::env::var("WORKLOAD_CASE").unwrap_or_else(|_| "default".to_string());
    let policy_str = std::env::var("POLICY").unwrap_or_else(|_| "hermes".to_string());
    let metrics_path = std::env::var("METRICS_PATH").unwrap_or_else(|_| "metrics.csv".to_string());
    let conns_path = std::env::var("CONNS_PATH").unwrap_or_else(|_| "conns.csv".to_string());
    let verbose = std::env::var("VERBOSE").map(|v| v == "1").unwrap_or(false);
    let seed: u64 = std::env::var("SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0);

    let policy = match policy_str.as_str() {
        "lifo" => Policy::Lifo,
        "reuseport" => Policy::ReuseportHash,
        _ => Policy::Hermes,
    };

    let config = match case_str.as_str() {
        "1" => WorkloadConfig::for_case(WorkloadCase::Case1),
        "2" => WorkloadConfig::for_case(WorkloadCase::Case2),
        "3" => WorkloadConfig::for_case(WorkloadCase::Case3),
        "4" => WorkloadConfig::for_case(WorkloadCase::Case4),
        "5" => WorkloadConfig::for_case(WorkloadCase::Case5),
        _ => WorkloadConfig::default_case(),
    };

    eprintln!(
        "[main] policy={} case={case_str} cps={} duration={:?} seed={seed} → {metrics_path}, {conns_path}",
        policy.as_str(),
        config.cps,
        config.duration,
    );

    // ── Shared state + per-worker metrics shard files ──────────────────────
    let shared = shm::mmap_shared_state();

    let shard_dir = std::env::temp_dir().join(format!("micro_hermes_shards_{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&shard_dir) {
        panic!("failed to create metrics shard dir {shard_dir:?}: {e}");
    }
    let tick_shards: Vec<PathBuf> =
        (0..NUM_WORKERS).map(|i| shard_dir.join(format!("w{i}_ticks.csv"))).collect();
    let conn_shards: Vec<PathBuf> =
        (0..NUM_WORKERS).map(|i| shard_dir.join(format!("w{i}_conns.csv"))).collect();

    // ── Fork one child per worker ──────────────────────────────────────────
    let mut child_pids = Vec::with_capacity(NUM_WORKERS);
    for worker_id in 0..NUM_WORKERS {
        let pid = unsafe { libc::fork() };
        match pid {
            -1 => panic!("fork failed: {}", std::io::Error::last_os_error()),
            0 => {
                worker::worker_loop(
                    shared,
                    worker_id,
                    config.hang,
                    config.burst,
                    policy,
                    &tick_shards[worker_id],
                    &conn_shards[worker_id],
                    verbose,
                );
                std::process::exit(0);
            }
            child_pid => child_pids.push(child_pid),
        }
    }

    // ── Parent: generate traffic (kernel stand-in), then reap children ─────
    let gen_stats = generator::run(shared, &config, policy, seed);
    for pid in &child_pids {
        let mut status = 0i32;
        unsafe { libc::waitpid(*pid, &mut status, 0) };
    }

    // ── Merge metrics shards ────────────────────────────────────────────────
    match metrics::merge_shards(&tick_shards, &metrics_path, &metrics::tick_header()) {
        Ok(rows) => eprintln!("[metrics] {} tick rows → {metrics_path}", rows.len()),
        Err(e) => eprintln!("[main] failed to write tick CSV: {e}"),
    }
    let conn_lines = match metrics::merge_shards(&conn_shards, &conns_path, &metrics::conn_header()) {
        Ok(rows) => {
            eprintln!("[metrics] {} conn rows → {conns_path}", rows.len());
            rows
        }
        Err(e) => {
            eprintln!("[main] failed to write conn CSV: {e}");
            Vec::new()
        }
    };
    let _ = std::fs::remove_dir_all(&shard_dir);

    // ── Summary ─────────────────────────────────────────────────────────────
    let summary = metrics::summarize_conns(&conn_lines);
    let final_open: Vec<i64> = (0..NUM_WORKERS)
        .map(|i| shared.wst.slot(i).accumulated_conns.load(Ordering::Relaxed))
        .collect();
    let dropped: Vec<u64> =
        (0..NUM_WORKERS).map(|i| shared.drops[i].load(Ordering::Relaxed)).collect();
    let completed: Vec<i64> =
        summary.completed_per_worker.iter().map(|&c| c as i64).collect();

    println!("\n── Run summary ({} / case {case_str}) ─────────────────────", policy.as_str());
    println!("  generated={}  completed={}  dropped={}", gen_stats.generated, summary.completed, gen_stats.dropped);
    for i in 0..NUM_WORKERS {
        println!(
            "  worker {i}: dispatched={:>5}  completed={:>5}  open_at_exit={:>4}  dropped={:>4}",
            gen_stats.dispatched_per_worker[i], completed[i], final_open[i], dropped[i]
        );
    }
    println!(
        "  balance: completed SD = {:.2}   open-conn SD = {:.2}  (lower = better, Fig. 13)",
        metrics::std_dev(&completed),
        metrics::std_dev(&final_open),
    );
    println!(
        "  latency (arrival→done): mean = {:.1}ms  p50 = {:.1}ms  p99 = {:.1}ms  max = {:.1}ms",
        summary.mean_us / 1_000.0,
        summary.p50_us as f64 / 1_000.0,
        summary.p99_us as f64 / 1_000.0,
        summary.max_us as f64 / 1_000.0,
    );
    println!("──────────────────────────────────────────────────────────\n");
}
