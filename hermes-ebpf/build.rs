use which::which;

/// Rebuild when the bpf-linker binary changes. Cargo can't express this
/// dependency properly (rust-lang/cargo#12385), so use its mtime instead
fn main() {
    let bpf_linker = which("bpf-linker").unwrap();
    println!("cargo:rerun-if-changed={}", bpf_linker.to_str().unwrap());
}
