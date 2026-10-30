//! Harness telemetry fields (architecture 13.1).
//!
//! The adapter reports browser RAM separately from the harness and the
//! container by adding these fields to every execution `ResourceSample`,
//! with peaks alongside the instantaneous values.

/// Browser memory fields attached to a harness resource sample.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BrowserResourceSample {
    /// Working set in bytes (level A or B).
    pub browser_working_set_bytes: u64,
    /// PSS in bytes when level C was measured.
    pub browser_pss_bytes: Option<u64>,
    /// Live browser instances.
    pub browser_instances: u32,
    /// Open browser tabs.
    pub browser_tabs: u32,
    /// Peak working set in bytes.
    pub peak_working_set_bytes: u64,
    /// Peak PSS in bytes.
    pub peak_pss_bytes: Option<u64>,
    /// Peak instances.
    pub peak_instances: u32,
    /// Peak tabs.
    pub peak_tabs: u32,
}

impl BrowserResourceSample {
    /// Create a sample; peaks start at the instantaneous values.
    #[must_use]
    pub fn new(working_set_bytes: u64, pss_bytes: Option<u64>, instances: u32, tabs: u32) -> Self {
        Self {
            browser_working_set_bytes: working_set_bytes,
            browser_pss_bytes: pss_bytes,
            browser_instances: instances,
            browser_tabs: tabs,
            peak_working_set_bytes: working_set_bytes,
            peak_pss_bytes: pss_bytes,
            peak_instances: instances,
            peak_tabs: tabs,
        }
    }

    /// Fold a new observation in, raising the peaks.
    pub fn observe(
        &mut self,
        working_set_bytes: u64,
        pss_bytes: Option<u64>,
        instances: u32,
        tabs: u32,
    ) {
        self.browser_working_set_bytes = working_set_bytes;
        self.browser_pss_bytes = pss_bytes;
        self.browser_instances = instances;
        self.browser_tabs = tabs;
        self.peak_working_set_bytes = self.peak_working_set_bytes.max(working_set_bytes);
        match (self.peak_pss_bytes, pss_bytes) {
            (Some(peak), Some(now)) => self.peak_pss_bytes = Some(peak.max(now)),
            (None, Some(now)) => self.peak_pss_bytes = Some(now),
            _ => {}
        }
        self.peak_instances = self.peak_instances.max(instances);
        self.peak_tabs = self.peak_tabs.max(tabs);
    }
}
