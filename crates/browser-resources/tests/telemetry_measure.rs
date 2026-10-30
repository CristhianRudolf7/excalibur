//! Telemetry peaks and reproducible measurement summaries.

use excalibur_browser_resources::budgets::MIB;
use excalibur_browser_resources::measure::{
    HEAVY_TABS_PER_CASE, LevelReadings, MeasureEnvironment, MeasuredCase, REPETITIONS, SETTLE_SECS,
    median_of_three, summarize_triplet,
};
use excalibur_browser_resources::telemetry::BrowserResourceSample;

fn env() -> MeasureEnvironment {
    MeasureEnvironment {
        chrome_version: "155.0.8059.84".to_string(),
        kernel_version: "6.8.0-test".to_string(),
        flags: vec!["--headless=new".to_string()],
    }
}

#[test]
fn telemetry_tracks_peaks_separately() {
    let mut sample = BrowserResourceSample::new(500 * MIB, Some(400 * MIB), 2, 3);
    sample.observe(700 * MIB, Some(300 * MIB), 1, 5);
    assert_eq!(sample.browser_working_set_bytes, 700 * MIB);
    assert_eq!(sample.browser_pss_bytes, Some(300 * MIB));
    assert_eq!(sample.peak_working_set_bytes, 700 * MIB);
    // Peaks never fall back.
    assert_eq!(sample.peak_pss_bytes, Some(400 * MIB));
    assert_eq!(sample.peak_instances, 2);
    assert_eq!(sample.peak_tabs, 5);
}

#[test]
fn triplet_median_min_max() {
    assert_eq!(median_of_three([3, 1, 2]), 2);
    assert_eq!(summarize_triplet([300, 100, 200]), (100, 200, 300));
}

#[test]
fn measured_case_requires_three_repetitions() {
    let case = MeasuredCase {
        name: "ten-heavy-tabs".to_string(),
        environment: env(),
        repetitions: vec![
            LevelReadings {
                level_a_bytes: None,
                level_b_bytes: 900 * MIB,
                level_c_pss_bytes: 500 * MIB,
            },
            LevelReadings {
                level_a_bytes: None,
                level_b_bytes: 1000 * MIB,
                level_c_pss_bytes: 520 * MIB,
            },
            LevelReadings {
                level_a_bytes: None,
                level_b_bytes: 950 * MIB,
                level_c_pss_bytes: 510 * MIB,
            },
        ],
    };
    assert_eq!(case.median_level_b(), Some(950 * MIB));
    assert_eq!(case.median_level_c(), Some(510 * MIB));
    assert_eq!(REPETITIONS, 3);
    assert_eq!(SETTLE_SECS, 30);
    assert_eq!(HEAVY_TABS_PER_CASE, 10);

    let short = MeasuredCase {
        name: "blank".to_string(),
        environment: env(),
        repetitions: vec![],
    };
    assert_eq!(short.median_level_b(), None);
}
