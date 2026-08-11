//! Types and constants shared by hermes, hermes-ebpf and hermes-bench.
//! no_std because the eBPF crate links against it directly

#![no_std]

/// Number of worker processes, one per core in the real system (§2.1).
/// Fixed at compile time so the M_socket map size and the WST/bitmap
/// width agree without a runtime handshake
pub const NUM_WORKERS: usize = 4;

/// Default TCP port, override with --port on both binaries
pub const DEFAULT_PORT: u16 = 7878;

/// bpffs directory the program and maps are pinned under. Pinning lets
/// each worker reopen M_Sel after fork() without any fd-passing
pub const BPFFS_DIR: &str = "/sys/fs/bpf/hermes";

pub const PIN_PROGRAM: &str = "hermes_select";
pub const PIN_M_SEL: &str = "m_sel";
pub const PIN_M_SOCKET: &str = "m_socket";

/// Wire protocol between hermes-bench and the workers. Deliberately tiny,
/// this project validates dispatch rather than reimplementing HTTP/TLS.
/// The request carries the per-connection processing cost and a client
/// timestamp, the response echoes enough back for the client to compute
/// end-to-end latency. Fields are raw little-endian bytes, both sides
/// share this crate so no serialization framework is needed
pub const REQUEST_MAGIC: u32 = 0x4845524d; // "HERM"
pub const REQUEST_HEADER_LEN: usize = 24; // magic,seq,service_us,_reserved (u32 x4) + send_ns (i64)

/// send_ns is the client's own monotonic timestamp, echoed back so
/// latency is measured on the client's clock rather than trusted from the
/// server
pub fn encode_request(seq: u32, service_us: u32, send_ns: i64, buf: &mut [u8; REQUEST_HEADER_LEN]) {
    buf[0..4].copy_from_slice(&REQUEST_MAGIC.to_le_bytes());
    buf[4..8].copy_from_slice(&seq.to_le_bytes());
    buf[8..12].copy_from_slice(&service_us.to_le_bytes());
    buf[12..16].copy_from_slice(&0u32.to_le_bytes());
    buf[16..24].copy_from_slice(&send_ns.to_le_bytes());
}

/// Returns None if the magic doesn't match
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

/// Response, the echoed seq and send_ns plus the serving worker id. The
/// worker id is informative only, balance is measured from the tick CSVs
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
