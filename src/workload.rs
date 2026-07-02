/// Workload generator — configurable synthetic connection arrivals.
///
/// Replaces the hardcoded "worker 0 is always slow" approach with a proper
/// workload model covering the four Hermes paper cases (Table 3):
///
///   Case 1 — High CPS, Low processing time   (stress/spike scenario)
///   Case 2 — High CPS, High processing time  (compression-heavy)
///   Case 3 — Low CPS,  Low processing time   (finance/chat, long-lived conns)
///   Case 4 — Low CPS,  High processing time  (SSL/regex-heavy web services)
///
/// Also supports hang injection so Stage 1 of Algorithm 1 is exercised.

use std::time::Duration;

/// Processing time distribution for a workload case.
#[derive(Debug, Clone, Copy)]
pub enum ProcessingTime {
    /// Fixed duration (simple testing).
    Fixed(Duration),
    /// Bimodal: most events fast, occasional slow events (simulates varied L7).
    Bimodal {
        fast_ms: u64,
        slow_ms: u64,
        /// Probability [0,1] of a slow event.
        slow_probability: f64,
    },
}

impl ProcessingTime {
    /// Draw a processing time sample.  Uses a simple LCG so no rand crate needed.
    pub fn sample(&self, seed: u64) -> Duration {
        match self {
            ProcessingTime::Fixed(d) => *d,
            ProcessingTime::Bimodal { fast_ms, slow_ms, slow_probability } => {
                // Simple LCG random float in [0, 1).
                let r = lcg_float(seed);
                if r < *slow_probability {
                    Duration::from_millis(*slow_ms)
                } else {
                    Duration::from_millis(*fast_ms)
                }
            }
        }
    }
}

/// One of the four paper workload scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadCase {
    Case1, // High CPS, Low processing time
    Case2, // High CPS, High processing time
    Case3, // Low CPS,  Low processing time  (most common — 56% of production)
    Case4, // Low CPS,  High processing time
}

/// Per-worker workload configuration.
#[derive(Debug, Clone)]
pub struct WorkloadConfig {
    /// How many event-loop iterations to run.
    pub iterations: u32,
    /// Processing time per event for this worker.
    pub processing_time: ProcessingTime,
    /// If Some(iter), the worker simulates a hang starting at that iteration
    /// by sleeping longer than HANG_THRESHOLD_NS.  This exercises Stage 1.
    pub hang_at_iter: Option<u32>,
    /// Duration of the injected hang (should exceed HANG_THRESHOLD_NS = 200ms).
    pub hang_duration: Duration,
    /// Events returned per simulated epoll_wait call.
    pub events_per_batch: i64,
}

impl WorkloadConfig {
    /// Build a configuration for one of the four paper cases.
    pub fn for_case(case: WorkloadCase, worker_id: usize) -> Self {
        match case {
            WorkloadCase::Case1 => WorkloadConfig {
                iterations: 40,
                processing_time: ProcessingTime::Fixed(Duration::from_millis(2)),
                hang_at_iter: None,
                hang_duration: Duration::ZERO,
                events_per_batch: 2 + (worker_id as i64 % 3),
            },
            WorkloadCase::Case2 => WorkloadConfig {
                iterations: 20,
                processing_time: ProcessingTime::Bimodal {
                    fast_ms: 10,
                    slow_ms: 200,
                    slow_probability: 0.2,
                },
                // Worker 0 hangs partway through — exercises Stage 1.
                hang_at_iter: if worker_id == 0 { Some(5) } else { None },
                hang_duration: Duration::from_millis(350),
                events_per_batch: 3,
            },
            WorkloadCase::Case3 => WorkloadConfig {
                iterations: 30,
                processing_time: ProcessingTime::Fixed(Duration::from_millis(3)),
                hang_at_iter: None,
                hang_duration: Duration::ZERO,
                events_per_batch: 2,
            },
            WorkloadCase::Case4 => WorkloadConfig {
                iterations: 15,
                processing_time: ProcessingTime::Bimodal {
                    fast_ms: 20,
                    slow_ms: 150,
                    slow_probability: 0.4,
                },
                hang_at_iter: None,
                hang_duration: Duration::ZERO,
                events_per_batch: 1 + (worker_id as i64 % 2),
            },
        }
    }

    /// Default config used when no case is specified — matches the original
    /// behaviour (worker 0 slow, others fast) for backward compatibility.
    pub fn default_for_worker(worker_id: usize) -> Self {
        WorkloadConfig {
            iterations: 25,
            processing_time: if worker_id == 0 {
                ProcessingTime::Fixed(Duration::from_millis(40))
            } else {
                ProcessingTime::Fixed(Duration::from_millis(2))
            },
            hang_at_iter: None,
            hang_duration: Duration::ZERO,
            events_per_batch: 2 + (worker_id as i64 % 3),
        }
    }
}

/// Simple LCG pseudo-random float in [0, 1).
fn lcg_float(seed: u64) -> f64 {
    // Park-Miller-esque constants, good enough for simulation.
    let x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (x >> 11) as f64 / (1u64 << 53) as f64
}
