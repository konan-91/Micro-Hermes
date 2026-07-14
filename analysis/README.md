# Benchmark analysis

Everything needed to produce the dissertation's figures and tables from the
micro-hermes simulator.

## Contents

| Path | What it is |
|---|---|
| `run_benchmarks.sh` | Runs the full matrix: 3 policies × 5 cases × `TRIALS` trials (each trial gets a distinct `SEED`). Skips result files that already exist. |
| `hermes_analysis.ipynb` | **The one thing to run.** Annotated notebook: generates data if missing, then produces all figures and tables. Every figure section includes a suggested caption and a design-justification paragraph written for direct adaptation into the dissertation. |
| `results/` | Raw per-run CSVs (`*_ticks.csv`, `*_conns.csv`). Git-ignored; regenerate any time. |
| `figures/` | Output figures, PNG (300 dpi, drafts) + PDF (vector, for LaTeX). |
| `tables/` | Output tables, CSV + LaTeX (`summary_stats.tex` is a ready `table` environment). |

## Usage

```bash
jupyter lab hermes_analysis.ipynb   # then: Kernel → Restart & Run All
```

or headless:

```bash
jupyter nbconvert --to notebook --execute --inplace hermes_analysis.ipynb
```

Requires Python with `pandas`, `numpy`, `matplotlib` (a stock JupyterLab
install has all three) and a Rust toolchain on `PATH` for the data-generation
step. End-to-end runtime from empty `results/`: ≈ 3 minutes.

To collect more trials for tighter error bars, edit `TRIALS` in the
notebook's first code cell (or run `TRIALS=5 ./run_benchmarks.sh` first — the
notebook will pick up whatever is present, but keep the two values in sync).

## Figure inventory

| File | Shows | Dissertation role |
|---|---|---|
| `fig1_latency_cdf` | Latency CDFs, one panel per case | Full distributional comparison |
| `fig2_p99_bars` | P99 ± trial sd per case | Headline tail-latency result |
| `fig3_balance_over_time` | Cross-worker open-conn SD trajectory (Case 3) | The paper's Fig. 13 balance metric |
| `fig4_concentration_profile` | Per-worker totals ranked busiest→least (Case 3) | Shape of imbalance (LIFO starvation) |
| `fig5_hang_detection` | Stage-1 filter reacting to an injected stall | Mechanism demonstration |
| `fig6_cascade_stages` | Survivors per Algorithm-1 stage per case | Filter behaviour vs. load |
| `fig7_burst` | Follow-up latency CDF when all open connections fire at once (Case 5) | Measured cost of LIFO's connection concentration |
