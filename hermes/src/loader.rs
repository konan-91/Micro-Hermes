//! Kernel-facing setup: the real counterpart of phase 1's `dispatcher.rs`.
//!
//! Phase 1's `Policy` enum picked between three *simulated* dispatch
//! mechanisms in userspace. In phase 2 only one of those three is code we
//! write at all — `reuseport` and `lifo` are the kernel's own
//! `SO_REUSEPORT` hash and `EPOLLEXCLUSIVE` wait-queue behaviour, selected
//! here purely by which *socket topology* we hand to the workers. `hermes`
//! additionally loads and attaches the real eBPF program from
//! `hermes-ebpf`.
//!
//! ## Socket topology per policy
//!
//! - `hermes` / `reuseport`: N distinct sockets, each with `SO_REUSEPORT`,
//!   all bound to the same port. Every worker owns exactly one (its own
//!   column in `M_socket`). The kernel's stateless 4-tuple hash picks among
//!   them by default; under `hermes` the attached eBPF program overrides
//!   that pick using Algorithm 2 (`hermes-ebpf/src/main.rs`).
//! - `lifo`: **one** socket, no `SO_REUSEPORT`. Every worker adds it to its
//!   own epoll instance with `EPOLLEXCLUSIVE`. This *is* the real
//!   `EPOLLEXCLUSIVE` mechanism the paper's baseline describes (§2.2): the
//!   kernel's wait-queue is genuinely shared and genuinely LIFO (insertion
//!   at the head, wakeup stops at the first idle waiter) — nothing here
//!   simulates that, it falls out of the kernel doing what it always does.
//!
//! All sockets are created here, in the loader, **before** `fork()`. Each
//! forked child inherits the whole fd table, so worker *i* already has
//! `listen_fds[i]` open in its own process with no fd-passing required —
//! see `main.rs`.

use anyhow::{Context, Result};
use aya::maps::ReusePortSockArray;
use aya::programs::SkReuseport;
use aya::EbpfLoader;
use hermes_common::{BPFFS_DIR, NUM_WORKERS, PIN_M_SOCKET, PIN_PROGRAM};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Full Hermes: Algorithm 1 in workers (`worker::schedule_and_sync`) +
    /// Algorithm 2 in the attached eBPF program.
    Hermes,
    /// Baseline: real `EPOLLEXCLUSIVE` wait-queue wakeup.
    Lifo,
    /// Baseline: real `SO_REUSEPORT` stateless 4-tuple hash, no eBPF
    /// program attached at all.
    Reuseport,
}

impl Policy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Policy::Hermes => "hermes",
            Policy::Lifo => "lifo",
            Policy::Reuseport => "reuseport",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hermes" => Some(Policy::Hermes),
            "lifo" => Some(Policy::Lifo),
            "reuseport" => Some(Policy::Reuseport),
            _ => None,
        }
    }

    /// Whether workers under this policy run Algorithm 1 and publish M_Sel.
    pub fn runs_scheduler(&self) -> bool {
        matches!(self, Policy::Hermes)
    }
}

/// Socket setup, ready to fork workers over. `listen_fds[i]` is the fd
/// worker `i` should `epoll_ctl`-register and `accept4` from. Under `lifo`
/// every entry is the *same* fd (see module docs); under `hermes` /
/// `reuseport` each is distinct.
pub struct Setup {
    pub listen_fds: Vec<RawFd>,
    /// Kept open for the loader process's lifetime so the fds stay valid
    /// until every worker has forked and inherited its own copy. Dropped
    /// (closed) once the parent itself exits; harmless either way since
    /// each worker's inherited copy keeps the underlying socket alive
    /// independently (see module docs on fork semantics).
    _owned: Vec<OwnedFd>,
}

/// Build the socket topology for `policy` and, for `Policy::Hermes`, load
/// and attach the eBPF program. Must be called before `fork()`.
pub fn setup(policy: Policy, port: u16) -> Result<Setup> {
    let setup = match policy {
        Policy::Hermes | Policy::Reuseport => {
            let mut owned = Vec::with_capacity(NUM_WORKERS);
            for _ in 0..NUM_WORKERS {
                owned.push(create_listener(port, true).context("creating SO_REUSEPORT listener")?);
            }
            let listen_fds: Vec<RawFd> = owned.iter().map(|fd| fd.as_raw_fd()).collect();
            if policy == Policy::Hermes {
                load_and_attach_ebpf(&listen_fds)?;
            }
            Setup { listen_fds, _owned: owned }
        }
        Policy::Lifo => {
            let owned = create_listener(port, false).context("creating shared listener")?;
            let fd = owned.as_raw_fd();
            Setup { listen_fds: vec![fd; NUM_WORKERS], _owned: vec![owned] }
        }
    };
    Ok(setup)
}

/// Create a TCP listener with `SO_REUSEADDR` (always, for fast restarts)
/// and, if `reuseport`, `SO_REUSEPORT` — which must be set **before**
/// `bind(2)`: the kernel only admits a socket into a reuseport group at
/// bind time, and setting the option afterward is silently ignored. This
/// is exactly why `std::net::TcpListener` can't be used here: it offers no
/// hook between `socket(2)` and `bind(2)`.
fn create_listener(port: u16, reuseport: bool) -> Result<OwnedFd> {
    use std::os::fd::FromRawFd;

    unsafe {
        let raw = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_NONBLOCK, 0);
        if raw < 0 {
            return Err(std::io::Error::last_os_error()).context("socket(2)");
        }
        let fd = OwnedFd::from_raw_fd(raw);

        set_sockopt_bool(&fd, libc::SO_REUSEADDR)?;
        if reuseport {
            set_sockopt_bool(&fd, libc::SO_REUSEPORT)?;
        }

        let addr = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr { s_addr: libc::INADDR_ANY.to_be() },
            sin_zero: [0; 8],
        };
        let ret = libc::bind(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        if ret != 0 {
            return Err(std::io::Error::last_os_error()).context(format!("bind(2) to port {port}"));
        }

        // Backlog: kernel accept queue depth (analogue of phase 1's
        // QUEUE_CAP). 1024 is generous for the CPS levels in workload.rs.
        if libc::listen(fd.as_raw_fd(), 1024) != 0 {
            return Err(std::io::Error::last_os_error()).context("listen(2)");
        }

        Ok(fd)
    }
}

fn set_sockopt_bool(fd: &OwnedFd, opt: libc::c_int) -> Result<()> {
    let one: libc::c_int = 1;
    let ret = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            opt,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error()).context(format!("setsockopt({opt})"));
    }
    Ok(())
}

/// Load `hermes-ebpf`, pin `M_Sel`/`M_socket` under `BPFFS_DIR` (so workers
/// can reopen them after `fork()` with no fd-passing, see `worker.rs`),
/// populate `M_socket` with every worker's listening socket, load the
/// `SK_REUSEPORT` program, and attach it to the reuseport group via any one
/// of the sockets (attaching through one socket in a group attaches to the
/// whole group — see `SkReuseport::attach` docs).
fn load_and_attach_ebpf(listen_fds: &[RawFd]) -> Result<()> {
    bump_memlock_rlimit()?;
    std::fs::create_dir_all(BPFFS_DIR)
        .with_context(|| format!("creating {BPFFS_DIR} (is /sys/fs/bpf mounted as bpffs?)"))?;

    let mut ebpf = EbpfLoader::new()
        .default_map_pin_directory(BPFFS_DIR)
        .load(aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/hermes")))
        .context("loading hermes-ebpf object")?;

    let mut socket_array: ReusePortSockArray<_> = ebpf
        .take_map(PIN_M_SOCKET)
        .with_context(|| format!("map {PIN_M_SOCKET} not found in eBPF object"))?
        .try_into()
        .context("M_socket is not a BPF_MAP_TYPE_REUSEPORT_SOCKARRAY")?;

    for (worker_id, &fd) in listen_fds.iter().enumerate() {
        // SAFETY: `fd` is one of our own listener fds, guaranteed open for
        // the duration of this call (owned by `Setup` in the caller).
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        socket_array
            .set(worker_id as u32, &borrowed, 0)
            .with_context(|| format!("M_socket[{worker_id}] = fd {fd}"))?;
    }

    let program: &mut SkReuseport = ebpf
        .program_mut(PIN_PROGRAM)
        .with_context(|| format!("program {PIN_PROGRAM} not found in eBPF object"))?
        .try_into()
        .context("program is not SK_REUSEPORT")?;
    program.load().context("loading SK_REUSEPORT program into the kernel")?;

    let first = unsafe { BorrowedFd::borrow_raw(listen_fds[0]) };
    program
        .attach(first)
        .context("SO_ATTACH_REUSEPORT_EBPF (needs CAP_NET_ADMIN or root)")?;

    log::info!(
        "eBPF loaded: program pinned+attached, M_socket populated with {} workers, M_Sel pinned at {BPFFS_DIR}/{}",
        listen_fds.len(),
        hermes_common::PIN_M_SEL,
    );

    // `ebpf` (and with it the loader's own fds for the program/maps) is
    // dropped here. That's safe: the program stays attached to the
    // reuseport group as long as its sockets exist (owned by our caller,
    // then inherited by every forked worker), and the maps stay reachable
    // because they're pinned to bpffs, not because this handle is alive.
    Ok(())
}

/// Bump `RLIMIT_MEMLOCK` to unlimited. Needed on kernels that still charge
/// eBPF map memory against the old memlock rlimit instead of memcg
/// (pre-5.11-ish; see https://lwn.net/Articles/837122/). Harmless no-op on
/// newer kernels.
fn bump_memlock_rlimit() -> Result<()> {
    let rlim = libc::rlimit { rlim_cur: libc::RLIM_INFINITY, rlim_max: libc::RLIM_INFINITY };
    let ret = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rlim) };
    if ret != 0 {
        log::debug!(
            "setrlimit(RLIMIT_MEMLOCK) failed (ret={ret}): {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Best-effort cleanup of pinned eBPF state, called by the loader on a
/// clean shutdown so repeated runs don't see stale pins. Not called on a
/// crash/kill -9, in which case `rm -rf /sys/fs/bpf/hermes` by hand (or
/// just rebooting / a fresh `bpffs` mount) is the manual recovery step —
/// the pins are inert once no process reopens them, they don't keep the
/// program attached (that's tied to the sockets, which die with the
/// workers) and don't leak kernel memory once removed.
pub fn cleanup_pins() {
    if let Err(e) = std::fs::remove_dir_all(BPFFS_DIR) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!("failed to clean up {BPFFS_DIR}: {e}");
        }
    }
}
