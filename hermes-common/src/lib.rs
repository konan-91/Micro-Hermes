//! Types and constants shared across every phase-2 crate: the eBPF program
//! (`hermes-ebpf`), the loader/worker binary (`hermes`), and the load
//! generator (`hermes-bench`). `#![no_std]` because `hermes-ebpf` links
//! against it directly; every consumer that also wants `std` gets it for
//! free (there is nothing here that needs an allocator or the standard
//! library).

#![no_std]

/// Number of worker processes, one pinned per CPU core in the real system
/// (§2.1 of the design doc). Kept small and fixed at compile time so the
/// eBPF side's `M_socket` map size and the userspace WST/bitmap width all
/// agree without a runtime handshake. Bump this and rebuild everything if
/// you want to test with more cores.
pub const NUM_WORKERS: usize = 4;

/// Default TCP port the worker group listens on. Overridable with `--port`
/// on both `hermes` and `hermes-bench`.
pub const DEFAULT_PORT: u16 = 7878;

/// bpffs directory the loader pins the eBPF program and both maps under.
/// Pinning (rather than keeping the only reference in the loader process's
/// fd table) is what lets the loader exit independently of the long-running
/// worker processes and lets each worker open `M_Sel` fresh after `fork()`
/// without any fd-passing: see `hermes::ebpf_maps`.
pub const BPFFS_DIR: &str = "/sys/fs/bpf/hermes";

pub const PIN_PROGRAM: &str = "hermes_select";
pub const PIN_M_SEL: &str = "m_sel";
pub const PIN_M_SOCKET: &str = "m_socket";

/// Wire protocol between `hermes-bench` (client) and the `hermes` worker
/// (server). Deliberately tiny: this project is validating *dispatch*, not
/// reimplementing HTTP/TLS, so the request just carries what the paper's
/// traffic model needs — a per-connection processing cost sampled by the
/// generator (§10) — and the response echoes back enough for the client to
/// compute true end-to-end latency itself.
///
/// Both sides know the wire layout at compile time (shared crate), so
/// requests are encoded/decoded as raw little-endian bytes below — no
/// serialization framework needed, and the format doesn't depend on host
/// endianness. Fields: magic(u32), seq(u32), service_us(u32), reserved(u32),
/// send_ns(i64) — see `REQUEST_HEADER_LEN`.
pub const REQUEST_MAGIC: u32 = 0x4845524d; // "HERM"
pub const REQUEST_HEADER_LEN: usize = 24; // magic,seq,service_us,_reserved (u32 x4) + send_ns (i64)

/// Encode a request onto the wire. `send_ns` is the client's own
/// `CLOCK_MONOTONIC` timestamp, carried through so latency is computed from
/// the client's perspective (what a real benchmarking tool measures) rather
/// than trusted from the server.
pub fn encode_request(seq: u32, service_us: u32, send_ns: i64, buf: &mut [u8; REQUEST_HEADER_LEN]) {
    buf[0..4].copy_from_slice(&REQUEST_MAGIC.to_le_bytes());
    buf[4..8].copy_from_slice(&seq.to_le_bytes());
    buf[8..12].copy_from_slice(&service_us.to_le_bytes());
    buf[12..16].copy_from_slice(&0u32.to_le_bytes());
    buf[16..24].copy_from_slice(&send_ns.to_le_bytes());
}

/// Decode a request header. Returns `None` if the magic doesn't match.
pub fn decode_request(buf: &[u8; REQUEST_HEADER_LEN]) -> Option<(u32, u32, i64)> {
    let magic = u32::from_le_bytes(buf[0..4].try_into().ok()?);
    if magic != REQUEST_MAGIC {
        return None;
    }
    let seq = u32::from_le_bytes(buf[4..8].try_into().ok()?);
    let service_us = u32::from_le_bytes(buf[8..12].try_into().ok()?);
    let send_ns = i64::from_le_bytes(buf[16..24].try_into().ok()?);
    Some((seq, service_us, send_ns))
}

/// Response: just the echoed seq + send_ns (so the client can identify which
/// request completed) plus the worker id that served it (purely informative;
/// balance is measured from the WST tick CSV, not from client-observed
/// worker ids, but it's useful for debugging dispatch by hand).
pub const RESPONSE_LEN: usize = 16; // seq(u32) + worker_id(u32) + send_ns(i64)

pub fn encode_response(seq: u32, worker_id: u32, send_ns: i64, buf: &mut [u8; RESPONSE_LEN]) {
    buf[0..4].copy_from_slice(&seq.to_le_bytes());
    buf[4..8].copy_from_slice(&worker_id.to_le_bytes());
    buf[8..16].copy_from_slice(&send_ns.to_le_bytes());
}

pub fn decode_response(buf: &[u8; RESPONSE_LEN]) -> (u32, u32, i64) {
    let seq = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let worker_id = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    let send_ns = i64::from_le_bytes(buf[8..16].try_into().unwrap());
    (seq, worker_id, send_ns)
}
