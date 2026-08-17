//! The paper's four traffic profiles (§10) plus Case 5, described from
//! the traffic side, connections per second and per-connection processing
//! cost at the LB.
//!
//!   Case 1  high CPS, low cost   (stress/spike)
//!   Case 2  high CPS, high cost  (overload, plus an injected hang)
//!   Case 3  low CPS, low cost, long-lived connections
//!   Case 4  low CPS, high cost   (SSL-heavy services)
//!   Case 5  Case 3 plus a synchronized burst on all open connections

use std::time::Duration;

/// Per-connection processing time distribution
#[derive(Debug, Clone, Copy)]
pub enum ProcessingTime {
    /// Fixed duration (uniform cheap requests)
    Fixed(Duration),
    /// Bimodal, most connections fast, some slow, modelling variable
    /// per-connection L7 cost (SSL handshakes, compression)
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

/// Offered load level within a case (paper Table 3). The level scales the
/// arrival rate only, everything else about the case stays fixed
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
    /// Case 3 accumulation followed by a synchronized burst of follow-up
    /// requests on every open connection, the failure mode the paper's
    /// conn metric guards against (§3)
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

/// At time `at` every open connection generates one ready event on the
/// worker that owns it. Connection affinity means those events cannot
/// migrate, whoever holds the connections must serialize the backlog
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
    pub lifetime: Duration,
    /// Optional injected hang
    pub hang: Option<HangSpec>,
    /// Optional synchronized burst on open connections
    pub burst: Option<BurstSpec>,
}

impl WorkloadConfig {
    /// Profiles are sized so each run finishes in a few seconds on a
    /// laptop while keeping the paper's qualitative CPS/cost
    /// relationships. Returns each case's characteristic load level
    /// (Case 2 is inherently an overload scenario so it gets heavy)
    pub fn for_case(case: WorkloadCase) -> Self {
        let level = match case {
            WorkloadCase::Case1 => LoadLevel::Light,
            WorkloadCase::Case2 => LoadLevel::Heavy,
            _ => LoadLevel::Medium,
        };
        Self::for_case_with_load(case, level)
    }

    /// Per-case CPS at each load level (Table 3 sweep). Capacity is about
    /// 4 worker-seconds of processing per second
    pub fn for_case_with_load(case: WorkloadCase, load: LoadLevel) -> Self {
        let mut config = Self::base_case(case);
        config.cps = match case {
            // mean cost 1ms so capacity ~4000 conn/s. Light 10%, medium
            // 40%, heavy 75%
            WorkloadCase::Case1 => match load {
                LoadLevel::Light => 400,
                LoadLevel::Medium => 1600,
                LoadLevel::Heavy => 3000,
            },
            // mean cost 45ms so capacity ~89 conn/s. Light 45%, medium
            // 84%, heavy 112% (sustained overload)
            WorkloadCase::Case2 => match load {
                LoadLevel::Light => 40,
                LoadLevel::Medium => 75,
                LoadLevel::Heavy => 100,
            },
            // cost is trivial, load here is the accumulation rate of
            // long-lived connections (120/240/600 over the 4s run)
            WorkloadCase::Case3 => match load {
                LoadLevel::Light => 30,
                LoadLevel::Medium => 60,
                LoadLevel::Heavy => 150,
            },
            // mean cost 74ms so capacity ~54 conn/s. Light 28%, medium
            // 74%, heavy 93%
            WorkloadCase::Case4 => match load {
                LoadLevel::Light => 15,
                LoadLevel::Medium => 40,
                LoadLevel::Heavy => 50,
            },
            // Case 5 is a fixed burst scenario, one level only
            WorkloadCase::Case5 => config.cps,
        };
        config
    }

    /// Per-case base profile. CPS here is the characteristic level,
    /// for_case_with_load overrides it for the sweep
    fn base_case(case: WorkloadCase) -> Self {
        match case {
            // high CPS, cheap requests, ~10% utilization
            WorkloadCase::Case1 => WorkloadConfig {
                cps: 400,
                duration: Duration::from_secs(3),
                service: ProcessingTime::Fixed(Duration::from_millis(1)),
                lifetime: Duration::from_millis(150),
                hang: None,
                burst: None,
            },
            // high CPS and expensive requests, sustained overload.
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
            // low CPS, cheap, long-lived. Connections never close within
            // the run, so final open-connection balance is the metric
            WorkloadCase::Case3 => WorkloadConfig {
                cps: 60,
                duration: Duration::from_secs(4),
                service: ProcessingTime::Fixed(Duration::from_millis(2)),
                lifetime: Duration::from_secs(60),
                hang: None,
                burst: None,
            },
            // low CPS, expensive, ~75% utilization. Occasional very slow
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
            // Case 3 accumulation, then at 2.5s every open connection
            // (~150 by then) fires one 5ms follow-up. LIFO has
            // concentrated the connections on the head worker, which must
            // serialize the whole backlog
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

/// SplitMix64-style scramble to a float in [0, 1)
fn lcg_float(seed: u64) -> f64 {
    let x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (x >> 11) as f64 / (1u64 << 53) as f64
}
