//! The paper's four traffic profiles (§10) plus Case 5. The traffic-side
//! numbers match the phase 1 simulator. Case 2's hang is injected on the
//! LB side via HERMES_HANG_INJECT since this process can't reach into a
//! worker, and lifetime bounds how long this client keeps a connection
//! open (capped in client.rs so a 60s profile doesn't stall the run)

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
    /// Draw a sample, deterministic in seed so runs are reproducible
    pub fn sample(&self, seed: u64) -> Duration {
        match self {
            ProcessingTime::Fixed(d) => *d,
            ProcessingTime::Bimodal { fast, slow, slow_probability } => {
                if lcg_float(seed) < *slow_probability { *slow } else { *fast }
            }
        }
    }
}

/// Offered load level within a case (paper Table 3)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadLevel {
    Light,
    Medium,
    Heavy,
}

impl LoadLevel {
    pub fn from_str(s: &str) -> Option<Self> {
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
    Case3,
    Case4,
    Case5,
}

impl WorkloadCase {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "1" => Some(WorkloadCase::Case1),
            "2" => Some(WorkloadCase::Case2),
            "3" => Some(WorkloadCase::Case3),
            "4" => Some(WorkloadCase::Case4),
            "5" => Some(WorkloadCase::Case5),
            _ => None,
        }
    }
}

/// At time `at` (relative to generator start) every connection still held
/// open fires one follow-up request over its existing socket
#[derive(Debug, Clone, Copy)]
pub struct BurstSpec {
    pub at: Duration,
    pub service: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct WorkloadConfig {
    pub cps: u32,
    pub duration: Duration,
    pub service: ProcessingTime,
    pub lifetime: Duration,
    pub burst: Option<BurstSpec>,
}

impl WorkloadConfig {
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
            WorkloadCase::Case1 => match load {
                LoadLevel::Light => 400,
                LoadLevel::Medium => 1600,
                LoadLevel::Heavy => 3000,
            },
            WorkloadCase::Case2 => match load {
                LoadLevel::Light => 40,
                LoadLevel::Medium => 75,
                LoadLevel::Heavy => 100,
            },
            WorkloadCase::Case3 => match load {
                LoadLevel::Light => 30,
                LoadLevel::Medium => 60,
                LoadLevel::Heavy => 150,
            },
            WorkloadCase::Case4 => match load {
                LoadLevel::Light => 15,
                LoadLevel::Medium => 40,
                LoadLevel::Heavy => 50,
            },
            WorkloadCase::Case5 => config.cps,
        };
        config
    }

    fn base_case(case: WorkloadCase) -> Self {
        match case {
            WorkloadCase::Case1 => WorkloadConfig {
                cps: 400,
                duration: Duration::from_secs(3),
                service: ProcessingTime::Fixed(Duration::from_millis(1)),
                lifetime: Duration::from_millis(150),
                burst: None,
            },
            // Case 2 also injects a worker hang, applied on the LB side
            // via HERMES_HANG_INJECT rather than here
            WorkloadCase::Case2 => WorkloadConfig {
                cps: 100,
                duration: Duration::from_secs(4),
                service: ProcessingTime::Bimodal {
                    fast: Duration::from_millis(10),
                    slow: Duration::from_millis(150),
                    slow_probability: 0.25,
                },
                lifetime: Duration::from_millis(200),
                burst: None,
            },
            WorkloadCase::Case3 => WorkloadConfig {
                cps: 60,
                duration: Duration::from_secs(4),
                service: ProcessingTime::Fixed(Duration::from_millis(2)),
                lifetime: Duration::from_secs(60),
                burst: None,
            },
            WorkloadCase::Case4 => WorkloadConfig {
                cps: 40,
                duration: Duration::from_secs(4),
                service: ProcessingTime::Bimodal {
                    fast: Duration::from_millis(20),
                    slow: Duration::from_millis(200),
                    slow_probability: 0.3,
                },
                lifetime: Duration::from_millis(400),
                burst: None,
            },
            WorkloadCase::Case5 => WorkloadConfig {
                cps: 60,
                duration: Duration::from_secs(4),
                service: ProcessingTime::Fixed(Duration::from_millis(2)),
                lifetime: Duration::from_secs(60),
                burst: Some(BurstSpec {
                    at: Duration::from_millis(2500),
                    service: Duration::from_millis(5),
                }),
            },
        }
    }

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
            burst: None,
        }
    }
}

fn lcg_float(seed: u64) -> f64 {
    let x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (x >> 11) as f64 / (1u64 << 53) as f64
}
