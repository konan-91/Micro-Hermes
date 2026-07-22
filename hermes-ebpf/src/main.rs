//! Stage 3 (§5.4, Algorithm 2): kernel-side fine-grained connection dispatch.
//!
//! Attached to the worker group's `SO_REUSEPORT` sockets via
//! `SO_ATTACH_REUSEPORT_EBPF` (see `hermes::loader`). Runs once per inbound
//! SYN that completes the handshake, in place of the kernel's default
//! stateless-hash reuseport socket pick.
//!
//! This is a direct, mechanical port of `dispatch_hermes` /
//! `reciprocal_scale` / `find_nth_set_bit` from the phase-1 simulator
//! (`phase1/src/dispatcher.rs`) — that code was deliberately written
//! heap-free and loop-bounded so this port would be exactly that.
//!
//! The one thing that changes shape, not substance: the paper's explicit
//! "if n <= 1, fall back to the default reuseport hash" rule (§5.4) is
//! realised here simply by returning `SK_PASS` without calling
//! `select_reuseport` at all — that is *literally* what `SK_PASS` means for
//! an `SK_REUSEPORT` program that hasn't picked a socket: the kernel runs
//! its normal hash-based selection. No fallback hash needs to be
//! reimplemented in eBPF.

#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::sk_action::SK_PASS,
    macros::{map, sk_reuseport},
    maps::{Array, ReusePortSockArray},
    programs::SkReuseportContext,
};
use hermes_common::NUM_WORKERS;

const IPPROTO_TCP: u32 = 6;

/// M_Sel (§5.4 table): the userspace-computed candidate bitmap. A single
/// u64 element — bit i set means worker i survived Algorithm 1's cascade.
/// Every worker's `schedule_and_sync()` overwrites this on its own cadence;
/// concurrent writers are safe because array-map element writes are atomic
/// and the paper only ever needs the most recent value (§5.3.1).
#[map(name = "m_sel")]
static M_SEL: Array<u64> = Array::pinned(1, 0);

/// M_socket (§5.4 table): worker ID -> underlying listening socket.
/// Populated once by the loader during startup (`hermes::loader`), before
/// any worker is forked, and never written again.
#[map(name = "m_socket")]
static M_SOCKET: ReusePortSockArray = ReusePortSockArray::pinned(NUM_WORKERS as u32, 0);

#[sk_reuseport]
pub fn hermes_select(ctx: SkReuseportContext) -> u32 {
    match try_select(&ctx) {
        Some(worker_id) => match M_SOCKET.select_reuseport(&ctx, worker_id) {
            Ok(()) => SK_PASS,
            // The chosen worker's socket isn't in the map (e.g. a race
            // during startup/shutdown before M_socket is fully populated).
            // Fail open to the kernel's default hash rather than dropping
            // a connection we could still have served.
            Err(_) => SK_PASS,
        },
        // Either n <= 1 candidates (the paper's fallback rule, §5.4) or
        // non-TCP traffic reached this reuseport group somehow: let the
        // kernel's default SO_REUSEPORT hash pick instead.
        None => SK_PASS,
    }
}

/// Algorithm 2 minus the final `select_reuseport` call (that needs the raw
/// context, so the caller does it). Returns the chosen worker ID, or `None`
/// to mean "fall back to the kernel's default hash".
#[inline(always)]
fn try_select(ctx: &SkReuseportContext) -> Option<u32> {
    if ctx.ip_protocol() != IPPROTO_TCP {
        return None;
    }
    let bitmap = *M_SEL.get(0)?;
    let n = bitmap.count_ones();
    if n <= 1 {
        return None;
    }
    let nth = reciprocal_scale(ctx.hash(), n);
    find_nth_set_bit(bitmap, nth)
}

/// The kernel's `reciprocal_scale()` (include/linux/reciprocal_div.h): maps
/// a 32-bit hash uniformly into `[0, n)` with one 64-bit multiply and one
/// shift — no modulo, no division by a non-constant (both forbidden by the
/// verifier for arbitrary runtime divisors).
#[inline(always)]
fn reciprocal_scale(hash: u32, n: u32) -> u32 {
    (((hash as u64) * n as u64) >> 32) as u32
}

/// Position of the Nth (0-based) set bit. `NUM_WORKERS` is a small
/// compile-time constant (4), so LLVM fully unrolls this loop — no
/// `bpf_loop()` helper or explicit bound-check is needed to satisfy the
/// verifier, unlike a loop over a runtime-sized collection.
#[inline(always)]
fn find_nth_set_bit(bitmap: u64, nth: u32) -> Option<u32> {
    let mut seen = 0u32;
    for i in 0..NUM_WORKERS as u32 {
        if bitmap & (1u64 << i) != 0 {
            if seen == nth {
                return Some(i);
            }
            seen += 1;
        }
    }
    None
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[unsafe(link_section = "license")]
#[unsafe(no_mangle)]
static LICENSE: [u8; 13] = *b"Dual MIT/GPL\0";
