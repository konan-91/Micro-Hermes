#![no_std]

// This file exists to enable the library target (needed for `cargo test`
// on the host target; the `#[sk_reuseport]` binary itself only ever builds
// for `bpfel-unknown-none`). See src/main.rs for the actual program.
