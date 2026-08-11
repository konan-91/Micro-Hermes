//! Socket setup and eBPF loading for each policy.
//!
//! hermes and reuseport use N distinct SO_REUSEPORT sockets bound to the
//! same port, one per worker. The kernel's 4-tuple hash picks among them
//! by default, and under hermes the attached eBPF program overrides that
//! pick with Algorithm 2. lifo uses a single shared socket which every
//! worker adds to its own epoll with EPOLLEXCLUSIVE, giving the real
//! wait-queue behaviour the baseline describes (§2.2).
//!
//! All sockets are created before fork(), so each worker inherits its
//! listen fd with no fd-passing needed

use anyhow::{Context, Result};
use aya::maps::ReusePortSockArray;
use aya::programs::SkReuseport;
use aya::EbpfLoader;
use hermes_common::{BPFFS_DIR, NUM_WORKERS, PIN_M_SOCKET, PIN_PROGRAM};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Algorithm 1 in workers plus Algorithm 2 in the eBPF program
    Hermes,
    /// Baseline, real EPOLLEXCLUSIVE wait-queue wakeup
    Lifo,
    /// Baseline, the default SO_REUSEPORT hash with no eBPF attached
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

    /// Whether workers run Algorithm 1 and publish M_Sel
    pub fn runs_scheduler(&self) -> bool {
        matches!(self, Policy::Hermes)
    }
}

/// Socket setup ready to fork workers over. listen_fds[i] is the fd worker
/// i registers and accepts from. Under lifo every entry is the same fd
pub struct Setup {
    pub listen_fds: Vec<RawFd>,
    /// Kept open in the parent so the fds stay valid until every worker
    /// has forked and inherited its own copy
    _owned: Vec<OwnedFd>,
}

/// Build the sockets for `policy` and, under hermes, load and attach the
/// eBPF program. Must be called before fork()
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

/// Create a nonblocking TCP listener. SO_REUSEPORT must be set before
/// bind(2), the kernel only admits a socket into a reuseport group at bind
/// time. That is also why std's TcpListener can't be used here, it offers
/// no hook between socket(2) and bind(2)
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

        // 1024 backlog is generous for the CPS levels in workload.rs
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

/// Load hermes-ebpf, pin its maps under BPFFS_DIR so workers can reopen
/// them after fork(), populate M_socket with every worker's listener, then
/// load and attach the SK_REUSEPORT program. Attaching through one socket
/// attaches to the whole reuseport group
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
        // fd is one of our own listeners, kept open by the caller
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

    // Dropping ebpf here is fine. The program stays attached as long as
    // the sockets exist and the maps stay reachable through their pins
    Ok(())
}

/// Bump RLIMIT_MEMLOCK to unlimited. Kernels before ~5.11 charge eBPF map
/// memory against it. Harmless no-op on newer kernels
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

/// Remove pinned eBPF state on clean shutdown so repeated runs don't see
/// stale pins. Not called after a crash, in which case remove
/// /sys/fs/bpf/hermes by hand
pub fn cleanup_pins() {
    if let Err(e) = std::fs::remove_dir_all(BPFFS_DIR) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!("failed to clean up {BPFFS_DIR}: {e}");
        }
    }
}
