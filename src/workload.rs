//! Workload definitions: the four traffic profiles from the paper (§10).
//!
//! A workload is described from the *traffic* side, matching how the paper
//! characterizes its cases: connections per second (CPS) and per-connection
//! processing cost at the LB. Processing cost is a property of the
//! connection (SSL, compression, payload size), not of the worker; the
//! generator samples it per connection and carries it in the ConnDesc.
//!
//!   Case 1: High CPS, low processing time   (stress/spike scenario)
//!   Case 2: High CPS, high processing time  (compression-heavy; overload)
//!   Case 3: Low CPS,  low processing time   (long-lived conns: finance/chat)
//!   Case 4: Low CPS,  high processing time  (SSL/regex-heavy web services)
//!   Case 5: Case 3 + synchronized burst on all open connections (beyond
//!            the paper's four: evidence for the concentration failure mode)
//!
//! Case 2 also injects a worker hang so Stage 1 of Algorithm 1 is exercised.
//!
//! Phase 2: this module is replaced by a real traffic generator (e.g. wrk)
//! hitting real sockets; nothing here needs porting

use std::time::Duration;

/// Per-connection processing time distribution
#[derive(Debug, Clone, Copy)]
pub enum ProcessingTime {
    /// Fixed duration (uniform cheap requests)
    Fixed(Duration),
    /// Bimodal: most connections fast, some slow, models the paper's highly
    /// variable per-connection L7 cost (SSL handshakes, compression)
    Bimodal {
        fast: Duration,
        slow: Duration,
        /// Probability [0,1] of drawing the slow cost
        slow_probability: f64,
    },
}

impl ProcessingTime {
    /// Draw a sample. Deterministic in `seed` (no rand crate needed)
    pub fn sample(&self, seed: u64) -> Duration {
        match self {
            ProcessingTime::Fixed(d) => *d,
            ProcessingTime::Bimodal { fast, slow, slow_probability } => {
                if lcg_float(seed) < *slow_probability { *slow } else { *fast }
            }
        }
    }
}

/// Offered-load level within a case, mirroring the paper's Table 3 sweep
/// (light / medium / heavy workload per traffic model). The level scales the
/// case's connection arrival rate; the per-connection cost distribution and
/// all other properties of the case stay fixed, so the sweep varies *how
/// much* traffic arrives, not *what kind*
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadLevel {
    Light,
    Medium,
    Heavy,
}

impl LoadLevel {
    pub fn from_env_str(s: &str) -> Option<Self> {
        match s {
            "light" => Some(LoadLevel::Light),
            "medium" => Some(LoadLevel::Medium),
            "heavy" => Some(LoadLevel::Heavy),
            _ => None,
        }
    }
}

/// One of the four paper scenarios, plus the burst-evidence scenario
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadCase {
    Case1,
    Case2,
    Case3, // most common in production (~56%)
    Case4,
    /// Case 5 (beyond the paper's four): Case-3-style accumulation of
    /// long-lived connections followed by a synchronized burst of follow-up
    /// requests on every open connection; the failure mode the paper's
    /// `conn` metric exists to guard against (§3)
    Case5,
}

/// A hang injected into one worker, to exercise Stage-1 hang detection
#[derive(Debug, Clone, Copy)]
pub struct HangSpec {
    pub worker_id: usize,
    /// When the hang starts, relative to worker start
    pub at: Duration,
    /// How long the worker stays stuck (must exceed HANG_THRESHOLD_NS to
    /// actually trip the time filter)
    pub duration: Duration,
}

/// A synchronized burst of follow-up requests: at time `at`, every open
/// connection generates one ready event on the worker that owns it.
/// Connection affinity means those events cannot migrate: whoever holds
/// the connections must serialize the whole backlog through its own loop
#[derive(Debug, Clone, Copy)]
pub struct BurstSpec {
    /// When the burst fires, relative to worker start
    pub at: Duration,
    /// Processing cost of each follow-up request
    pub service: Duration,
}

/// Full traffic profile for one run
#[derive(Debug, Clone, Copy)]
pub struct WorkloadConfig {
    /// New connections per second
    pub cps: u32,
    /// How long the generator produces traffic
    pub duration: Duration,
    /// Per-connection processing cost distribution
    pub service: ProcessingTime,
    /// How long each connection stays open after its request is processed
    /// (drives `conn -= 1`; large values model long-lived connections)
    pub lifetime: Duration,
    /// Optional injected hang
    pub hang: Option<HangSpec>,
    /// Optional synchronized burst on open connections
    pub burst: Option<BurstSpec>,
}

impl WorkloadConfig {
    /// Traffic profiles sized so each run finishes in a few seconds on a
    /// laptop while keeping the paper's qualitative CPS/cost relationships
    /// (capacity here is NUM_WORKERS ~ 4 worker-seconds of processing per
    /// wall-clock second).
    ///
    /// `for_case` returns each case's *characteristic* load level, the one
    /// matching how the paper describes the scenario (Case 2 is inherently an
    /// overload scenario, so its characteristic level is heavy; Cases 3/4 sit
    /// at medium; Case 1's base profile is light). `for_case_with_load`
    /// selects a specific level of the Table-3-style sweep
    pub fn for_case(case: WorkloadCase) -> Self {
        let level = match case {
            WorkloadCase::Case1 => LoadLevel::Light,
            WorkloadCase::Case2 => LoadLevel::Heavy,
            _ => LoadLevel::Medium,
        };
        Self::for_case_with_load(case, level)
    }

    /// The Table-3 sweep: per-case CPS at each load level. Utilization figures
    /// are offered load / capacity, with capacity ~ 4 worker-sec/sec
    pub fn for_case_with_load(case: WorkloadCase, load: LoadLevel) -> Self {
        let mut config = Self::base_case(case);
        config.cps = match case {
            // Mean cost 1 ms -> capacity ~ 4000 conn/s (batch-limit bound
            // slightly below that). Light 10%, medium 40%, heavy 75%
            WorkloadCase::Case1 => match load {
                LoadLevel::Light => 400,
                LoadLevel::Medium => 1600,
                LoadLevel::Heavy => 3000,
            },
            // Mean cost 45 ms -> capacity ~ 89 conn/s. Light 45%, medium 84%,
            // heavy 112% (sustained overload; the case's characteristic level)
            WorkloadCase::Case2 => match load {
                LoadLevel::Light => 40,
                LoadLevel::Medium => 75,
                LoadLevel::Heavy => 100,
            },
            // Cost is trivial; "load" here is the accumulation rate of
            // long-lived connections (120 / 240 / 600 over the 4 s run)
            WorkloadCase::Case3 => match load {
                LoadLevel::Light => 30,
                LoadLevel::Medium => 60,
                LoadLevel::Heavy => 150,
            },
            // Mean cost 74 ms -> capacity ~ 54 conn/s. Light 28%, medium 74%,
            // heavy 93%
            WorkloadCase::Case4 => match load {
                LoadLevel::Light => 15,
                LoadLevel::Medium => 40,
                LoadLevel::Heavy => 50,
            },
            // Case 5 is our fixed burst-evidence scenario, not part of the
            // paper's sweep, one level only
            WorkloadCase::Case5 => config.cps,
        };
        config
    }

    /// Per-case base profile (service distribution, duration, lifetime,
    /// hang/burst injection). CPS here is each case's characteristic level;
    /// `for_case_with_load` overrides it for the sweep
    fn base_case(case: WorkloadCase) -> Self {
        match case {
            // High CPS, cheap requests: ~10% utilization, dispatch-rate bound
            WorkloadCase::Case1 => WorkloadConfig {
                cps: 400,
                duration: Duration::from_secs(3),
                service: ProcessingTime::Fixed(Duration::from_millis(1)),
                lifetime: Duration::from_millis(150),
                hang: None,
                burst: None,
            },
            // High CPS *and* expensive requests: offered load ~ 4.5
            // worker-sec/sec vs capacity 4 -> sustained overload, queues grow.
            // Worker 0 hangs partway through to exercise Stage 1
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
                burst: None,
            },
            // Low CPS, cheap, long-lived: connections never close within the
            // run, final open-connection balance is the headline metric
            WorkloadCase::Case3 => WorkloadConfig {
                cps: 60,
                duration: Duration::from_secs(4),
                service: ProcessingTime::Fixed(Duration::from_millis(2)),
                lifetime: Duration::from_secs(60),
                hang: None,
                burst: None,
            },
            // Low CPS, expensive: ~75% utilization, occasional very slow
            // connections pin workers for long stretches
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
                burst: None,
            },
            // Case-3 accumulation, then at 2.5 s every open connection
            // (~150 by then) fires one 5 ms follow-up request. LIFO has
            // concentrated every connection on the head worker, which must
            // serialize the whole backlog; balanced policies split it 4 ways
            WorkloadCase::Case5 => WorkloadConfig {
                cps: 60,
                duration: Duration::from_secs(4),
                service: ProcessingTime::Fixed(Duration::from_millis(2)),
                lifetime: Duration::from_secs(60),
                hang: None,
                burst: Some(BurstSpec {
                    at: Duration::from_millis(2500),
                    service: Duration::from_millis(5),
                }),
            },
        }
    }

    /// Quick mixed profile for manual smoke testing (not paper validation)
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
            burst: None,
        }
    }
}

/// SplitMix64-style scramble -> float in [0, 1). Good enough for simulation
fn lcg_float(seed: u64) -> f64 {
    let x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (x >> 11) as f64 / (1u64 << 53) as f64
}
