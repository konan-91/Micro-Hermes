# Micro-Hermes

The two versions are built and run separately: the simulation runs on any Unix-like system, while the eBPF version requires Linux, because it loads a program into the kernel.

## Part 1: The Simulation

Requirements. A stable Rust toolchain; the analysis pipeline additionally needs Python 3 with pandas, numpy, matplotlib and Jupyter.

Building and running a single simulation, from the phase1/ directory:

    cargo run --release

Configuration is via environment variables:

    POLICY         hermes | lifo | reuseport      (default: hermes)
    WORKLOAD_CASE  1 | 2 | 3 | 4 | 5 | default    (default: default)
    LOAD           light | medium | heavy         (default: the
                   case's characteristic level)
    SEED           integer; varies the connection stream between
                   trials	              (default: 0)
    METRICS_PATH   per-iteration tick CSV   (default: metrics.csv)
    CONNS_PATH     per-connection CSV       (default: conns.csv)
    VERBOSE        1 to print every worker loop iteration

For example, the overloaded compression-heavy case under the Hermes policy:

    POLICY=hermes WORKLOAD_CASE=2 LOAD=heavy cargo run --release

Each run prints a summary and writes the two CSVs. Unit tests run with `cargo test`.

The full benchmark matrix (3 policies × {4 cases × 3 load levels + Case 5} × 3 trials, 8 minutes) is driven by `analysis/run_benchmarks.sh`, which writes per-run CSVs into `analysis/results/`, skipping existing files so an interrupted run can be resumed.

## Part 2: The eBPF Version

Requirements. Linux (developed on current Ubuntu). Beyond the Rust toolchain, compiling the kernel program needs the nightly toolchain with `rust-src` and the `bpf-linker` tool:

    rustup toolchain install nightly --component rust-src
    cargo install cargo-binstall && cargo binstall bpf-linker

Loading a program into the kernel requires administrator privileges, so the benchmark scripts start the load balancer under `sudo` for every policy.

Building both binaries from the repository root:

    cargo build --release -p hermes -p hermes-bench

Running one benchmark point end to end (starts the load balancer, runs the load generator against it, shuts it down):

    benchmark/run_case.sh <policy> <case> <load> <trial>

The full matrix, matching the simulation's:

    benchmark/run_all.sh          # 3 trials
    TRIALS=5 benchmark/run_all.sh # more trials for tighter error bars

Results are written to `benchmark/results/`: per-connection records from the load generator plus per-iteration records from each worker.

## Part 3: Regenerating Figures and Tables

    cd analysis && jupyter lab hermes_analysis.ipynb
    # then: Kernel → Restart & Run All

The notebook regenerates all figures (`analysis/figures/`) and tables (`analysis/tables/`) from whichever results directory its `RESULTS_DIR` points at: `analysis/results/` for the simulation, `benchmark/results/` for the eBPF version. The output paths are shared, so regenerating one version's artefacts overwrites the other's unless the output is redirected.
