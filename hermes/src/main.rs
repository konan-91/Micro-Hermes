//! Micro-Hermes phase 2: the real load balancer.
//!
//! Process layout mirrors phase 1's division of labor, now over real
//! sockets instead of shared-memory queues:
//!   parent  = "the loader" — builds the socket topology for the chosen
//!             policy (`loader::setup`), and for `hermes` loads/attaches
//!             the real eBPF program; then forks one worker per slot and
//!             supervises them until told to stop.
//!   children = one forked worker per slot, each running the real
//!             instrumented epoll loop (`worker::worker_loop`) and, under
//!             Hermes, the Algorithm-1 scheduler that writes the M_Sel
//!             eBPF map.
//!
//! The WST (`wst::mmap_wst`) is the one piece of phase-1-style shared
//! memory that survives unchanged: it was always meant to be real
//! userspace shared memory, not something eBPF touches (§4.1).
//!
//! Build with `cargo build --release -p hermes`, then run the resulting
//! binary with `sudo -E target/release/hermes --policy hermes` — loading
//! and attaching eBPF needs privilege (CAP_BPF + CAP_NET_ADMIN, or root).
//! `sudo cargo run` also works but re-resolves/rebuilds as root, which is
//! slower and litters `target/` with root-owned files; see README for the
//! full Ubuntu setup. `reuseport`/`lifo` don't strictly need root, but
//! running them the same way keeps the three policies directly comparable.

mod loader;
mod metrics;
mod scheduler;
mod worker;
mod wst;

use anyhow::{Context, Result};
use clap::Parser;
use hermes_common::NUM_WORKERS;
use loader::Policy;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use worker::HangSpec;

#[derive(Parser, Debug)]
#[command(about = "Micro-Hermes phase 2 load balancer")]
struct Cli {
    /// Dispatch policy under test: hermes | reuseport | lifo
    #[arg(long, default_value = "hermes")]
    policy: String,

    /// TCP port the worker group listens on
    #[arg(long, default_value_t = hermes_common::DEFAULT_PORT)]
    port: u16,

    /// Directory tick CSVs are written to (one file per worker)
    #[arg(long, default_value = "./metrics")]
    metrics_dir: PathBuf,

    /// Print one line per worker loop iteration
    #[arg(long)]
    verbose: bool,
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();
    let policy = Policy::parse(&cli.policy)
        .with_context(|| format!("unknown --policy '{}' (want hermes|reuseport|lifo)", cli.policy))?;

    std::fs::create_dir_all(&cli.metrics_dir)
        .with_context(|| format!("creating metrics dir {}", cli.metrics_dir.display()))?;

    let hang = parse_hang_inject();

    // Registered once, before fork(): signal_hook::flag::register installs
    // a raw sigaction handler (no background thread), and both signal
    // dispositions and the process's memory (including this Arc's backing
    // allocation, COW) are inherited by fork(2). Each forked child ends up
    // with its own independent copy of `shutdown` at the same virtual
    // address the handler was registered against, so a signal delivered to
    // any one process (parent or a specific child) only ever flips that
    // process's own flag — exactly the per-process shutdown flag each
    // worker_loop checks. Must happen before fork(): forking a
    // multi-threaded process is unsafe, and this process stays
    // single-threaded up to that point specifically so this is safe.
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, shutdown.clone())
        .context("registering SIGINT handler")?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, shutdown.clone())
        .context("registering SIGTERM handler")?;

    let wst = wst::mmap_wst();

    log::info!("policy={} port={} workers={NUM_WORKERS}", policy.as_str(), cli.port);
    let setup = loader::setup(policy, cli.port).context("socket/eBPF setup")?;

    let mut child_pids = Vec::with_capacity(NUM_WORKERS);
    for worker_id in 0..NUM_WORKERS {
        let pid = unsafe { libc::fork() };
        match pid {
            -1 => anyhow::bail!("fork failed: {}", std::io::Error::last_os_error()),
            0 => {
                // Child: drop the fds for every other worker's listener
                // (hermes/reuseport only — under lifo every entry is the
                // same fd, so this is a no-op there). Purely hygiene: the
                // kernel-side eBPF dispatch and the shared lifo listener
                // don't depend on which processes still hold which fds.
                for (i, &fd) in setup.listen_fds.iter().enumerate() {
                    if i != worker_id && fd != setup.listen_fds[worker_id] {
                        unsafe { libc::close(fd) };
                    }
                }
                let tick_path = cli.metrics_dir.join(format!("w{worker_id}_ticks.csv"));
                let hang_for_this_worker = hang.filter(|h| h.0 == worker_id).map(|h| h.1);
                let result = worker::worker_loop(
                    worker_id,
                    setup.listen_fds[worker_id],
                    policy,
                    wst,
                    hang_for_this_worker,
                    shutdown.clone(),
                    &tick_path,
                    cli.verbose,
                );
                if let Err(e) = result {
                    eprintln!("[w{worker_id}] fatal: {e:#}");
                    std::process::exit(1);
                }
                std::process::exit(0);
            }
            pid => child_pids.push(pid),
        }
    }

    log::info!("{} workers running, Ctrl-C to stop", child_pids.len());
    while !shutdown.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
    }

    log::info!("shutting down");
    // Forward explicitly in case the signal only reached this process
    // (e.g. `kill <pid>` rather than a terminal Ctrl-C, which the kernel
    // already delivers to the whole foreground process group including
    // every child).
    for &pid in &child_pids {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    for &pid in &child_pids {
        let mut status = 0i32;
        unsafe { libc::waitpid(pid, &mut status, 0) };
    }

    if policy == Policy::Hermes {
        loader::cleanup_pins();
    }
    Ok(())
}

/// Parse `HERMES_HANG_INJECT=<worker_id>:<at_ms>:<duration_ms>`, e.g.
/// `HERMES_HANG_INJECT=0:1500:400` to reproduce phase 1's Case-2 hang
/// injection (worker 0 stalls 400ms starting 1.5s after it comes up). Only
/// one worker can be targeted per run; unset by default.
fn parse_hang_inject() -> Option<(usize, HangSpec)> {
    let raw = std::env::var("HERMES_HANG_INJECT").ok()?;
    let parts: Vec<&str> = raw.split(':').collect();
    let [w, at, dur] = parts.as_slice() else {
        log::warn!("HERMES_HANG_INJECT='{raw}' malformed, expected worker_id:at_ms:duration_ms, ignoring");
        return None;
    };
    let worker_id: usize = w.parse().ok()?;
    let at_ms: u64 = at.parse().ok()?;
    let dur_ms: u64 = dur.parse().ok()?;
    Some((worker_id, HangSpec { at: Duration::from_millis(at_ms), duration: Duration::from_millis(dur_ms) }))
}
