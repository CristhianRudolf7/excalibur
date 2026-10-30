//! Reproducible memory measurement (architecture 18.3).
//!
//! Procedure with the binary pinned: blank instance, one local static site,
//! ten ephemeral tabs with the synthetic heavy page. Per case: 30 s settle,
//! level A/B/C reads, PSS grouped by process type. Repeat 3 times and report
//! the median with the minimum and maximum, plus the Chrome version, kernel
//! and flags. Logged-in WhatsApp/Teams figures after 1 h and 24 h come from
//! the manual 18.4 walkthrough; no logged-in consumption is claimed without
//! them.

/// Required repetitions per case.
pub const REPETITIONS: usize = 3;
/// Settle delay before each reading, in seconds.
pub const SETTLE_SECS: u64 = 30;
/// Synthetic heavy ephemeral tabs per case.
pub const HEAVY_TABS_PER_CASE: u32 = 10;

/// One level reading triple for a case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelReadings {
    /// Level-A working set when a private cgroup exists.
    pub level_a_bytes: Option<u64>,
    /// Level-B working set (`RssAnon + VmSwap` total).
    pub level_b_bytes: u64,
    /// Level-C PSS total.
    pub level_c_pss_bytes: u64,
}

/// Median of three values.
#[must_use]
pub fn median_of_three(mut values: [u64; 3]) -> u64 {
    values.sort_unstable();
    values[1]
}

/// Summarize three repetitions as `(min, median, max)`.
#[must_use]
pub fn summarize_triplet(values: [u64; 3]) -> (u64, u64, u64) {
    let mut sorted = values;
    sorted.sort_unstable();
    (sorted[0], sorted[1], sorted[2])
}

/// Environment metadata recorded with every measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasureEnvironment {
    /// Chrome version string, e.g. `155.0.8059.84`.
    pub chrome_version: String,
    /// Kernel release, e.g. `6.8.0-147-generic`.
    pub kernel_version: String,
    /// Chrome flags used for the case.
    pub flags: Vec<String>,
}

/// One measured case with its three repetitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasuredCase {
    /// Case name (`blank`, `static-site`, `ten-heavy-tabs`...).
    pub name: String,
    /// Environment of the run.
    pub environment: MeasureEnvironment,
    /// One entry per repetition.
    pub repetitions: Vec<LevelReadings>,
}

impl MeasuredCase {
    /// Median level-B working set across repetitions.
    #[must_use]
    pub fn median_level_b(&self) -> Option<u64> {
        if self.repetitions.len() != REPETITIONS {
            return None;
        }
        Some(median_of_three([
            self.repetitions[0].level_b_bytes,
            self.repetitions[1].level_b_bytes,
            self.repetitions[2].level_b_bytes,
        ]))
    }

    /// Median level-C PSS across repetitions.
    #[must_use]
    pub fn median_level_c(&self) -> Option<u64> {
        if self.repetitions.len() != REPETITIONS {
            return None;
        }
        Some(median_of_three([
            self.repetitions[0].level_c_pss_bytes,
            self.repetitions[1].level_c_pss_bytes,
            self.repetitions[2].level_c_pss_bytes,
        ]))
    }
}
