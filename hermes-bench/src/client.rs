//! Per-connection client task: the real counterpart of phase 1's role for
//! each dispatched connection (`phase1/src/worker.rs`'s per-event handling,
//! seen from the *client* side of a real socket instead of a shared-memory
//! queue).
//!
//! One `tokio` task per connection. Each task: connects, sends the initial
//! request, times the round trip, then — if this is a Case-5-style
//! workload — waits for either its own close deadline or the synchronized
//! burst deadline, whichever comes first, exactly once.

use hermes_common::{decode_response, encode_request, REQUEST_HEADER_LEN, RESPONSE_LEN};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant as StdInstant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant as TokioInstant;

use crate::workload::BurstSpec;

/// One row of the conns CSV: a completed (or failed) request, timed
/// end-to-end from this client's own clock. This is what phase 1 could
/// never measure directly (it had no real client) and is a strictly better
/// latency signal: it's what a real caller of the load balancer would see.
#[derive(Debug, Clone)]
pub struct ConnRow {
    pub conn_id: u64,
    pub seq: u32,
    /// "accept" (the connection's first request) | "burst" (Case-5
    /// follow-up) | "error" (connect/write/read failed) | "drop" (connect
    /// itself failed, e.g. backlog full — the real analogue of phase 1's
    /// simulated accept-queue overflow).
    pub kind: &'static str,
    pub worker_id: Option<u32>,
    pub send_ns: i64,
    pub recv_ns: i64,
    pub service_us: u32,
    pub label: Arc<str>,
}

impl ConnRow {
    fn ok(conn_id: u64, seq: u32, kind: &'static str, worker_id: u32, send_ns: i64, recv_ns: i64, service_us: u32, label: &Arc<str>) -> Self {
        Self { conn_id, seq, kind, worker_id: Some(worker_id), send_ns, recv_ns, service_us, label: label.clone() }
    }

    fn failed(conn_id: u64, seq: u32, kind: &'static str, label: &Arc<str>) -> Self {
        Self { conn_id, seq, kind, worker_id: None, send_ns: 0, recv_ns: 0, service_us: 0, label: label.clone() }
    }

    pub fn latency_us(&self) -> Option<i64> {
        if self.worker_id.is_some() {
            Some((self.recv_ns - self.send_ns) / 1_000)
        } else {
            None
        }
    }

    pub fn to_csv_line(&self) -> String {
        let worker = self.worker_id.map(|w| w.to_string()).unwrap_or_default();
        let latency = self.latency_us().map(|l| l.to_string()).unwrap_or_default();
        format!(
            "{},{},{},{},{},{},{},{},{}",
            self.conn_id, self.seq, self.kind, worker, self.send_ns, self.recv_ns, latency, self.service_us, self.label,
        )
    }
}

pub fn csv_header() -> &'static str {
    "conn_id,seq,kind,worker_id,send_ns,recv_ns,latency_us,service_us,label"
}

#[allow(clippy::too_many_arguments)]
pub async fn run_connection(
    addr: SocketAddr,
    conn_id: u64,
    initial_service_us: u32,
    close_at: TokioInstant,
    burst: Option<BurstSpec>,
    clock_start: StdInstant,
    gen_start: TokioInstant,
    label: Arc<str>,
    tx: UnboundedSender<ConnRow>,
) {
    let mut stream = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(_) => {
            let _ = tx.send(ConnRow::failed(conn_id, 0, "drop", &label));
            return;
        }
    };
    let _ = stream.set_nodelay(true);

    if !fire_request(&mut stream, 0, initial_service_us, clock_start, conn_id, "accept", &label, &tx).await {
        return;
    }

    let mut seq = 1u32;
    let mut burst_fired = false;
    loop {
        // tokio::select!'s `if` guard short-circuits: when false, the
        // branch's future expression is never evaluated, so `.unwrap()` on
        // a `None` burst deadline is never reached (idiomatic pattern for
        // an "optional timer" branch — see the tokio::select! docs).
        let burst_active = !burst_fired && burst.is_some();
        tokio::select! {
            _ = tokio::time::sleep_until(close_at) => break,
            _ = tokio::time::sleep_until(gen_start + burst.map(|b| b.at).unwrap_or_default()), if burst_active => {
                burst_fired = true;
                let service_us = burst.unwrap().service.as_micros() as u32;
                if !fire_request(&mut stream, seq, service_us, clock_start, conn_id, "burst", &label, &tx).await {
                    break;
                }
                seq += 1;
            }
        }
    }
}

async fn fire_request(
    stream: &mut TcpStream,
    seq: u32,
    service_us: u32,
    clock_start: StdInstant,
    conn_id: u64,
    kind: &'static str,
    label: &Arc<str>,
    tx: &UnboundedSender<ConnRow>,
) -> bool {
    let send_ns = now_ns(clock_start);
    let mut req = [0u8; REQUEST_HEADER_LEN];
    encode_request(seq, service_us, send_ns, &mut req);
    if stream.write_all(&req).await.is_err() {
        let _ = tx.send(ConnRow::failed(conn_id, seq, "error", label));
        return false;
    }

    let mut resp = [0u8; RESPONSE_LEN];
    if stream.read_exact(&mut resp).await.is_err() {
        let _ = tx.send(ConnRow::failed(conn_id, seq, "error", label));
        return false;
    }
    let recv_ns = now_ns(clock_start);
    let (rseq, worker_id, echoed_send_ns) = decode_response(&resp);
    let _ = tx.send(ConnRow::ok(conn_id, rseq, kind, worker_id, echoed_send_ns, recv_ns, service_us, label));
    true
}

/// Nanoseconds since `clock_start`. `send_ns`/`recv_ns` only ever get
/// compared against each other on *this* client's own clock (the server
/// just echoes the bits back unchanged), so a plain `Instant` delta is all
/// that's needed — no wall-clock or cross-process clock sync required.
fn now_ns(clock_start: StdInstant) -> i64 {
    clock_start.elapsed().as_nanos() as i64
}
