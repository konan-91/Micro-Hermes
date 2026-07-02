/// Micro-Hermes Phase 1: userspace simulation of the Hermes load balancer.
///
/// Allocates the WST in shared anonymous memory, forks one worker process per
/// slot, runs the configurable workload, and writes metrics.csv on exit.
///
/// No eBPF or real TCP at this stage.  The dispatcher is a pure-Rust
/// simulation of Algorithm 2.  Phase 2 will replace it with a real eBPF
/// kernel module attached via SO_ATTACH_REUSEPORT_EBPF.
///
/// Environment variables:
///   WORKLOAD_CASE  — 1|2|3|4|default (default: "default")
///   POLICY         — hermes|lifo|reuseport (default: "hermes")
///   METRICS_PATH   — path for CSV output (default: "metrics.csv")

mod dispatcher;
mod metrics;
mod scheduler;
mod worker;
mod workload;
mod wst;

use metrics::MetricsAccumulator;
use scheduler::Policy;
use std::mem::size_of;
use std::ptr;
use workload::{WorkloadCase, WorkloadConfig};
use wst::{Wst, NUM_WORKERS};

fn main() {
    // ── Parse environment config ───────────────────────────────────────────
    let case_str = std::env::var("WORKLOAD_CASE").unwrap_or_else(|_| "default".to_string());
    let policy_str = std::env::var("POLICY").unwrap_or_else(|_| "hermes".to_string());
    let metrics_path = std::env::var("METRICS_PATH").unwrap_or_else(|_| "metrics.csv".to_string());

    let policy = match policy_str.as_str() {
        "lifo" => Policy::Lifo,
        "reuseport" => Policy::ReuseportHash,
        _ => Policy::Hermes,
    };

    let workload_case: Option<WorkloadCase> = match case_str.as_str() {
        "1" => Some(WorkloadCase::Case1),
        "2" => Some(WorkloadCase::Case2),
        "3" => Some(WorkloadCase::Case3),
        "4" => Some(WorkloadCase::Case4),
        _ => None, // "default" → original per-worker config
    };

    eprintln!(
        "[main] policy={policy_str} workload={case_str} metrics={metrics_path}"
    );

    // ── Allocate WST in shared anonymous memory ────────────────────────────
    // MAP_SHARED | MAP_ANONYMOUS: after fork(), parent and all children share
    // the same physical pages.  This is the inter-process WST from §4.1 /
    // §5.3.1 — no IPC, no pipes, just direct shared memory reads/writes.
    let wst_size = size_of::<Wst>();
    let wst_ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            wst_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if wst_ptr == libc::MAP_FAILED {
        panic!("mmap WST failed: {}", std::io::Error::last_os_error());
    }
    // Safety: MAP_ANONYMOUS is always zero-filled by the OS, matching the
    // zero-state of WorkerSlot::new().  The 'static lifetime is safe because
    // the mapping outlives all forked children (we waitpid before munmap).
    let wst: &'static Wst = unsafe { &*(wst_ptr as *const Wst) };

    // ── Allocate MetricsAccumulator in shared memory ───────────────────────
    // Each worker pushes rows; parent flushes to CSV after all children exit.
    // We use a second mmap'd region so the Mutex and Vec are in shared memory.
    //
    // NOTE: Using a Mutex across fork boundaries is safe here because:
    //   (a) we only lock from child processes (one per worker, no contention
    //       between parent and children on the metrics lock), and
    //   (b) the parent never touches the accumulator until after waitpid.
    //
    // In a production system you'd use a lock-free ring buffer; for a
    // dissertation demo this is fine.
    let acc_size = size_of::<MetricsAccumulator>();
    let acc_ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            acc_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if acc_ptr == libc::MAP_FAILED {
        panic!("mmap MetricsAccumulator failed: {}", std::io::Error::last_os_error());
    }
    // Construct the accumulator in-place in the shared region.
    let accumulator: &'static MetricsAccumulator = unsafe {
        let acc = acc_ptr as *mut MetricsAccumulator;
        std::ptr::write(acc, MetricsAccumulator::new());
        &*acc
    };

    // ── Fork one child per worker ──────────────────────────────────────────
    let mut child_pids = Vec::with_capacity(NUM_WORKERS);

    for worker_id in 0..NUM_WORKERS {
        let config = match workload_case {
            Some(case) => WorkloadConfig::for_case(case, worker_id),
            None => WorkloadConfig::default_for_worker(worker_id),
        };

        let pid = unsafe { libc::fork() };
        match pid {
            -1 => panic!("fork failed: {}", std::io::Error::last_os_error()),
            0 => {
                // ── Child process ─────────────────────────────────────────
                worker::worker_loop(wst, worker_id, config, policy, Some(accumulator));
                std::process::exit(0);
            }
            child_pid => child_pids.push(child_pid),
        }
    }

    // ── Parent: wait for all children ─────────────────────────────────────
    for pid in &child_pids {
        let mut status = 0i32;
        unsafe { libc::waitpid(*pid, &mut status, 0) };
    }

    // ── Flush metrics CSV ─────────────────────────────────────────────────
    if let Err(e) = accumulator.flush_csv(&metrics_path) {
        eprintln!("[main] failed to write CSV: {e}");
    }

    // ── Print summary ─────────────────────────────────────────────────────
    let final_conns: Vec<i64> = (0..NUM_WORKERS)
        .map(|i| {
            wst.slot(i)
                .accumulated_conns
                .load(std::sync::atomic::Ordering::Relaxed)
        })
        .collect();
    let final_events: Vec<i64> = (0..NUM_WORKERS)
        .map(|i| {
            wst.slot(i)
                .pending_events
                .load(std::sync::atomic::Ordering::Relaxed)
        })
        .collect();

    println!("\n── Final WST state ────────────────────────────────────");
    for i in 0..NUM_WORKERS {
        println!("  worker {i}: conns={:>3}  pending_events={:>3}", final_conns[i], final_events[i]);
    }

    let conn_mean = final_conns.iter().sum::<i64>() as f64 / NUM_WORKERS as f64;
    let conn_sd = {
        let var = final_conns
            .iter()
            .map(|&c| (c as f64 - conn_mean).powi(2))
            .sum::<f64>()
            / NUM_WORKERS as f64;
        var.sqrt()
    };
    println!("  conn SD = {conn_sd:.2}  (lower is better balanced; Fig. 13 target)");
    println!("  policy  = {policy_str}");
    println!("  metrics → {metrics_path}");
    println!("──────────────────────────────────────────────────────\n");

    // ── Cleanup mmap regions ──────────────────────────────────────────────
    unsafe {
        libc::munmap(wst_ptr, wst_size);
        libc::munmap(acc_ptr, acc_size);
    }
}
