//! Workload definitions — the four traffic profiles from the paper (§10).
//!
//! A workload is described from the *traffic* side, matching how the paper
//! characterizes its cases: connections per second (CPS) and per-connection
//! processing cost at the LB. Processing cost is a property of the
//! connection (SSL, compression, payload size), not of the worker — the
//! generator samples it per connection and carries it in the ConnDesc.
//!
//!   Case 1 — High CPS, low processing time   (stress/spike scenario)
//!   Case 2 — High CPS, high processing time  (compression-heavy; overload)
//!   Case 3 — Low CPS,  low processing time   (long-lived conns: finance/chat)
//!   Case 4 — Low CPS,  high processing time  (SSL/regex-heavy web services)
//!
//! Case 2 also injects a worker hang so Stage 1 of Algorithm 1 is exercised.
//!
//! Phase 2: this module is replaced by a real traffic generator (e.g. wrk)
//! hitting real sockets — nothing here needs porting.

use std::time::Duration;

/// Per-connection processing time distribution.
#[derive(Debug, Clone, Copy)]
pub enum ProcessingTime {
    /// Fixed duration (uniform cheap requests).
    Fixed(Duration),
    /// Bimodal: most connections fast, some slow — models the paper's highly
    /// variable per-connection L7 cost (SSL handshakes, compression).
    Bimodal {
        fast: Duration,
        slow: Duration,
        /// Probability [0,1] of drawing the slow cost.
        slow_probability: f64,
    },
}

impl ProcessingTime {
    /// Draw a sample. Deterministic in `seed` (no rand crate needed).
    pub fn sample(&self, seed: u64) -> Duration {
        match self {
            ProcessingTime::Fixed(d) => *d,
            ProcessingTime::Bimodal { fast, slow, slow_probability } => {
                if lcg_float(seed) < *slow_probability { *slow } else { *fast }
            }
        }
    }
}

/// One of the four paper scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadCase {
    Case1,
    Case2,
    Case3, // most common in production (~56%)
    Case4,
}

/// A hang injected into one worker, to exercise Stage-1 hang detection.
#[derive(Debug, Clone, Copy)]
pub struct HangSpec {
    pub worker_id: usize,
    /// When the hang starts, relative to worker start.
    pub at: Duration,
    /// How long the worker stays stuck (must exceed HANG_THRESHOLD_NS to
    /// actually trip the time filter).
    pub duration: Duration,
}

/// Full traffic profile for one run.
#[derive(Debug, Clone, Copy)]
pub struct WorkloadConfig {
    /// New connections per second.
    pub cps: u32,
    /// How long the generator produces traffic.
    pub duration: Duration,
    /// Per-connection processing cost distribution.
    pub service: ProcessingTime,
    /// How long each connection stays open after its request is processed
    /// (drives `conn -= 1`; large values model long-lived connections).
    pub lifetime: Duration,
    /// Optional injected hang.
    pub hang: Option<HangSpec>,
}

impl WorkloadConfig {
    /// Traffic profiles sized so each run finishes in a few seconds on a
    /// laptop while keeping the paper's qualitative CPS/cost relationships
    /// (capacity here is NUM_WORKERS ≈ 4 worker-seconds of processing per
    /// wall-clock second).
    pub fn for_case(case: WorkloadCase) -> Self {
        match case {
            // High CPS, cheap requests: ~10% utilization, dispatch-rate bound.
            WorkloadCase::Case1 => WorkloadConfig {
                cps: 400,
                duration: Duration::from_secs(3),
                service: ProcessingTime::Fixed(Duration::from_millis(1)),
                lifetime: Duration::from_millis(150),
                hang: None,
            },
            // High CPS *and* expensive requests: offered load ≈ 4.5
            // worker-sec/sec vs capacity 4 → sustained overload, queues grow.
            // Worker 0 hangs partway through to exercise Stage 1.
            WorkloadCase::Case2 => WorkloadConfig {
                cps: 100,
                duration: Duration::from_secs(4),
                service: ProcessingTime::Bimodal {
                    fast: Duration::from_millis(10),
                    slow: Duration::from_millis(150),
                    slow_probability: 0.25,
                },
                lifetime: Duration::from_millis(200),
                hang: Some(HangSpec {
                    worker_id: 0,
                    at: Duration::from_millis(1500),
                    duration: Duration::from_millis(400),
                }),
            },
            // Low CPS, cheap, long-lived: connections never close within the
            // run — final open-connection balance is the headline metric.
            WorkloadCase::Case3 => WorkloadConfig {
                cps: 60,
                duration: Duration::from_secs(4),
                service: ProcessingTime::Fixed(Duration::from_millis(2)),
                lifetime: Duration::from_secs(60),
                hang: None,
            },
            // Low CPS, expensive: ~75% utilization, occasional very slow
            // connections pin workers for long stretches.
            WorkloadCase::Case4 => WorkloadConfig {
                cps: 40,
                duration: Duration::from_secs(4),
                service: ProcessingTime::Bimodal {
                    fast: Duration::from_millis(20),
                    slow: Duration::from_millis(200),
                    slow_probability: 0.3,
                },
                lifetime: Duration::from_millis(400),
                hang: None,
            },
        }
    }

    /// Quick mixed profile for manual smoke testing (not paper validation).
    pub fn default_case() -> Self {
        WorkloadConfig {
            cps: 150,
            duration: Duration::from_millis(2500),
            service: ProcessingTime::Bimodal {
                fast: Duration::from_millis(2),
                slow: Duration::from_millis(40),
                slow_probability: 0.1,
            },
            lifetime: Duration::from_millis(250),
            hang: None,
        }
    }
}

/// SplitMix64-style scramble → float in [0, 1). Good enough for simulation.
fn lcg_float(seed: u64) -> f64 {
    let x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (x >> 11) as f64 / (1u64 << 53) as f64
}
