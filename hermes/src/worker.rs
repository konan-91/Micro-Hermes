//! Worker event loop: the real counterpart of phase 1's instrumented epoll
//! loop (Fig. 9, `phase1/src/worker.rs`). One process per worker, run to
//! completion, real `epoll`/`accept4`/`read`/`write` on real TCP sockets.
//!
//! Mapping of Fig. 9's hooks onto this file (compare to the phase-1 doc
//! comment they were ported from — the mapping is unchanged, only the
//! implementation of each hook is now real syscalls instead of a
//! shared-memory ring buffer):
//!   line 12: shm_avail_update        -> stamp last_loop_entry at loop entry
//!   line 13: epoll_wait(timer=5ms)   -> real epoll_wait, timeout=5ms
//!   line 14: shm_busy_count(+N)      -> pending_events += (fds returned)
//!   line 25: accept + conn_count(+1) -> accept4() drained to EAGAIN (ET)
//!   handler: process the request     -> parse wire request, sleep(service_us)
//!   line 18: shm_busy_count(-1)      -> pending_events -= 1 per handled fd
//!   line 37: close + conn_count(-1)  -> EOF/EPOLLHUP/EPOLLERR -> close()
//!   line 20: schedule_and_sync()     -> Algorithm 1 + M_Sel eBPF map write
//!
//! A connection's cost is a property of *the connection* (§3), sampled by
//! the client (`hermes-bench`) and carried in each request header; the
//! worker just executes it. This mirrors the paper's framing precisely and
//! keeps the traffic-case controls (`hermes-bench/src/workload.rs`)
//! identical in spirit to phase 1's, just driven by real bytes instead of a
//! synthetic `ConnDesc`.

use crate::loader::Policy;
use crate::metrics::{TickRow, TickWriter};
use crate::scheduler::{schedule, HANG_THRESHOLD_NS};
use crate::wst::{now_monotonic_ns, Wst};
use aya::maps::{Array, Map, MapData};
use hermes_common::{
    decode_request, encode_response, BPFFS_DIR, PIN_M_SEL, REQUEST_HEADER_LEN, RESPONSE_LEN,
};
use std::collections::HashMap;
use std::os::fd::RawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// epoll_wait timeout, applied under every policy: the paper's LB runs a
/// 5 ms timer in all epoll modes (Fig. 5b). Guarantees the loop — and thus
/// hang detection and (under Hermes) the scheduler — runs at least every
/// 5 ms even with zero traffic (§5.3.2).
const EPOLL_TIMEOUT_MS: libc::c_int = 5;
/// Max events per `epoll_wait` call.
const MAX_EVENTS: usize = 64;

/// A hang injected into this worker, to exercise Stage-1 hang detection —
/// unchanged in spirit from phase 1's `HangSpec`, now driven by an env var
/// at LB startup (`HERMES_HANG_INJECT=<worker_id>:<at_ms>:<duration_ms>`,
/// parsed in `main.rs`) since the workload case now lives in the separate
/// `hermes-bench` process and can't reach into a worker directly.
#[derive(Debug, Clone, Copy)]
pub struct HangSpec {
    pub at: Duration,
    pub duration: Duration,
}

struct ConnState {
    /// Bytes read but not yet forming a complete `REQUEST_HEADER_LEN`
    /// request. TCP is a byte stream: one `read()` can return a partial
    /// header, several pipelined requests, or both.
    buf: Vec<u8>,
}

pub fn worker_loop(
    worker_id: usize,
    listen_fd: RawFd,
    policy: Policy,
    wst: &'static Wst,
    hang: Option<HangSpec>,
    shutdown: Arc<AtomicBool>,
    tick_path: &Path,
    verbose: bool,
) -> anyhow::Result<()> {
    let slot = wst.slot(worker_id);
    let epfd = epoll_create()?;
    add_listener(epfd, listen_fd, policy)?;

    // Hermes only: reopen the pinned M_Sel map fresh in this process. No
    // fd-passing needed — `default_map_pin_directory` in loader.rs already
    // pinned it under BPFFS_DIR, so any process can reopen it by path.
    let mut m_sel: Option<Array<MapData, u64>> = if policy.runs_scheduler() {
        let data = MapData::from_pin(format!("{BPFFS_DIR}/{PIN_M_SEL}"))
            .map_err(|e| anyhow::anyhow!("worker {worker_id}: opening pinned M_Sel: {e}"))?;
        // `from_pin` gives a type-erased `MapData`; `from_map_data` reads the
        // map's kernel-reported type back into the tagged `Map` enum so we
        // can `TryFrom` it into the concrete `Array<MapData, u64>` wrapper —
        // the documented pattern for reopening a pinned map in a fresh
        // process (no fd-passing from the loader needed, see loader.rs).
        let map = Map::from_map_data(data)
            .map_err(|e| anyhow::anyhow!("worker {worker_id}: M_Sel has unsupported map type: {e}"))?;
        Some(Array::try_from(map).map_err(|e| anyhow::anyhow!("M_Sel is not an Array<u64>: {e}"))?)
    } else {
        None
    };

    let mut conns: HashMap<RawFd, ConnState> = HashMap::new();
    let mut tick_writer = TickWriter::create(tick_path)?;
    let mut hang_pending = hang;
    let start = Instant::now();
    let mut events = vec![empty_event(); MAX_EVENTS];
    let mut iter: u32 = 0;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Fig. 9 line 12: shm_avail_update
        slot.last_loop_entry.store(now_monotonic_ns(), Ordering::Relaxed);

        // Hang injection: block before epoll_wait without re-stamping,
        // exactly what a worker stuck in a handler looks like to Stage 1.
        if let Some(h) = hang_pending {
            if start.elapsed() >= h.at {
                eprintln!("[w{worker_id}] injecting {:?} hang at t={:?}", h.duration, start.elapsed());
                std::thread::sleep(h.duration);
                hang_pending = None;
                continue;
            }
        }

        // Fig. 9 line 13: epoll_wait(timer = 5ms)
        let n = epoll_wait(epfd, &mut events, EPOLL_TIMEOUT_MS)?;

        // Fig. 9 line 14: shm_busy_count(+event_num). "event_num" here is
        // the number of ready *fds* epoll_wait returned, matching Fig. 9's
        // granularity exactly (one list entry per ready fd, regardless of
        // how many accepts/requests that fd yields once drained).
        if n > 0 {
            slot.pending_events.fetch_add(n as i64, Ordering::Relaxed);
        }

        for ev in &events[..n] {
            let ev = *ev; // copy out of the (packed) array before reading fields
            let fd = ev.u64 as RawFd;
            let mask = ev.events;

            if fd == listen_fd {
                accept_all(epfd, listen_fd, slot, &mut conns, verbose, worker_id);
            } else {
                handle_conn_event(fd, mask, epfd, &mut conns, slot, worker_id, verbose);
            }

            // Fig. 9 line 18: shm_busy_count(-1), once this list entry is
            // fully handled (all pending accepts/requests on that fd drained).
            slot.pending_events.fetch_sub(1, Ordering::Relaxed);
        }

        // Fig. 9 line 20: schedule_and_sync(), END of loop. Hermes only —
        // the baselines have no userspace scheduler (§ Stage 3 table).
        let result = if let Some(m_sel) = m_sel.as_mut() {
            let r = schedule(wst, now_monotonic_ns(), HANG_THRESHOLD_NS);
            if let Err(e) = m_sel.set(0, r.bitmap, 0) {
                log::warn!("worker {worker_id}: M_Sel write failed: {e}");
            }
            Some(r)
        } else {
            None
        };

        let tick = TickRow {
            timestamp_ns: now_monotonic_ns(),
            worker_id,
            iter,
            snapshots: wst.snapshot_all(),
            open_conns: conns.len(),
            result,
            policy,
        };
        if verbose {
            tick.print();
        }
        tick_writer.write(&tick)?;

        iter = iter.wrapping_add(1);
    }

    tick_writer.flush()?;
    for &fd in conns.keys() {
        unsafe { libc::close(fd) };
    }
    Ok(())
}

/// Drain every pending connection on the listener (edge-triggered: must
/// `accept4` until `EAGAIN`, or a burst arriving in one wakeup is missed).
fn accept_all(
    epfd: RawFd,
    listen_fd: RawFd,
    slot: &crate::wst::WorkerSlot,
    conns: &mut HashMap<RawFd, ConnState>,
    verbose: bool,
    worker_id: usize,
) {
    loop {
        let fd = unsafe { libc::accept4(listen_fd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_NONBLOCK) };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EAGAIN) && err.raw_os_error() != Some(libc::EWOULDBLOCK) {
                log::warn!("worker {worker_id}: accept4 failed: {err}");
            }
            return;
        }

        // accept_handler (Fig. 9 line 25): shm_conn_count(+1)
        slot.accumulated_conns.fetch_add(1, Ordering::Relaxed);

        if let Err(e) = epoll_add_conn(epfd, fd) {
            log::warn!("worker {worker_id}: epoll_ctl(ADD) for accepted fd failed: {e}");
            unsafe { libc::close(fd) };
            slot.accumulated_conns.fetch_sub(1, Ordering::Relaxed);
            continue;
        }
        if verbose {
            eprintln!("[w{worker_id}] accepted fd={fd}");
        }
        conns.insert(fd, ConnState { buf: Vec::with_capacity(REQUEST_HEADER_LEN) });
    }
}

/// Handle a readiness event on an accepted connection: drain readable
/// bytes (ET), process every complete request found, write its response,
/// and close on EOF/error/hangup (Fig. 9 line 37: `shm_conn_count(-1)`).
fn handle_conn_event(
    fd: RawFd,
    mask: u32,
    epfd: RawFd,
    conns: &mut HashMap<RawFd, ConnState>,
    slot: &crate::wst::WorkerSlot,
    worker_id: usize,
    verbose: bool,
) {
    let mut close = mask & (libc::EPOLLHUP as u32 | libc::EPOLLERR as u32) != 0;

    if !close {
        let mut readbuf = [0u8; 4096];
        loop {
            let ret = unsafe { libc::read(fd, readbuf.as_mut_ptr() as *mut libc::c_void, readbuf.len()) };
            if ret > 0 {
                if let Some(state) = conns.get_mut(&fd) {
                    state.buf.extend_from_slice(&readbuf[..ret as usize]);
                }
            } else if ret == 0 {
                close = true; // peer closed (Fig. 9: "fin")
                break;
            } else {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EAGAIN) || err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                    break; // drained (ET: must reach EAGAIN before returning)
                }
                close = true; // real error (Fig. 9: "err")
                break;
            }
        }
    }

    if !close {
        // Process every complete request currently buffered.
        while let Some(state) = conns.get_mut(&fd) {
            if state.buf.len() < REQUEST_HEADER_LEN {
                break;
            }
            let mut header = [0u8; REQUEST_HEADER_LEN];
            header.copy_from_slice(&state.buf[..REQUEST_HEADER_LEN]);
            state.buf.drain(..REQUEST_HEADER_LEN);

            let Some((seq, service_us, send_ns)) = decode_request(&header) else {
                log::warn!("worker {worker_id}: bad magic on fd={fd}, dropping connection");
                close = true;
                break;
            };

            // The L7 work itself (SSL, compression, ...), simulated: this
            // is the same controlled-cost model phase 1 used, now driven
            // by the client's request instead of a synthetic ConnDesc —
            // see the module doc comment for why that boundary is drawn
            // here rather than implementing a real protocol stack.
            std::thread::sleep(Duration::from_micros(service_us as u64));

            let mut resp = [0u8; RESPONSE_LEN];
            encode_response(seq, worker_id as u32, send_ns, &mut resp);
            if let Err(e) = write_all(fd, &resp) {
                log::debug!("worker {worker_id}: write to fd={fd} failed: {e}");
                close = true;
                break;
            }
        }
    }

    if close {
        unsafe {
            libc::epoll_ctl(epfd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut());
            libc::close(fd);
        }
        conns.remove(&fd);
        // other_handler (Fig. 9 line 37): shm_conn_count(-1)
        slot.accumulated_conns.fetch_sub(1, Ordering::Relaxed);
        if verbose {
            eprintln!("[w{worker_id}] closed fd={fd}");
        }
    }
}

/// Best-effort blocking write for the tiny (16-byte) response. A
/// production implementation would arm `EPOLLOUT` and finish the write
/// asynchronously if the socket buffer is ever full; for responses this
/// small over localhost that essentially never happens, so a short
/// retry-on-EAGAIN spin keeps the worker loop simple. Flagged here as the
/// one deliberately-simplified corner of the real I/O path.
fn write_all(fd: RawFd, buf: &[u8]) -> std::io::Result<()> {
    let mut off = 0;
    let mut spins = 0;
    while off < buf.len() {
        let ret = unsafe {
            libc::write(fd, buf[off..].as_ptr() as *const libc::c_void, buf.len() - off)
        };
        if ret > 0 {
            off += ret as usize;
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAGAIN) && spins < 1000 {
                spins += 1;
                std::thread::yield_now();
                continue;
            }
            return Err(err);
        }
    }
    Ok(())
}

fn epoll_create() -> anyhow::Result<RawFd> {
    let fd = unsafe { libc::epoll_create1(0) };
    if fd < 0 {
        anyhow::bail!("epoll_create1: {}", std::io::Error::last_os_error());
    }
    Ok(fd)
}

fn empty_event() -> libc::epoll_event {
    libc::epoll_event { events: 0, u64: 0 }
}

fn add_listener(epfd: RawFd, listen_fd: RawFd, policy: Policy) -> anyhow::Result<()> {
    let mut events = libc::EPOLLIN as u32 | libc::EPOLLET as u32;
    if policy == Policy::Lifo {
        // The real EPOLLEXCLUSIVE mechanism (§2.2): every worker adds the
        // *same* shared listener with this flag. The kernel's wait-queue
        // wakeup then genuinely is LIFO-by-registration-order and stops at
        // the first idle waiter — nothing here simulates that.
        events |= libc::EPOLLEXCLUSIVE as u32;
    }
    let mut ev = libc::epoll_event { events, u64: listen_fd as u64 };
    let ret = unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, listen_fd, &mut ev) };
    if ret != 0 {
        anyhow::bail!("epoll_ctl(ADD, listener): {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn epoll_add_conn(epfd: RawFd, fd: RawFd) -> anyhow::Result<()> {
    let mut ev = libc::epoll_event { events: libc::EPOLLIN as u32 | libc::EPOLLET as u32, u64: fd as u64 };
    let ret = unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, fd, &mut ev) };
    if ret != 0 {
        anyhow::bail!("epoll_ctl(ADD, conn): {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn epoll_wait(epfd: RawFd, events: &mut [libc::epoll_event], timeout_ms: libc::c_int) -> anyhow::Result<usize> {
    let ret = unsafe { libc::epoll_wait(epfd, events.as_mut_ptr(), events.len() as libc::c_int, timeout_ms) };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            return Ok(0); // interrupted by a signal (e.g. our own shutdown handler)
        }
        anyhow::bail!("epoll_wait: {err}");
    }
    Ok(ret as usize)
}
