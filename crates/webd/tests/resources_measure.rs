//! B6 18.3 measurement procedure over synthetic fixtures (no real Chrome).
//!
//! Runs the reproducible procedure shape from architecture 18.3: blank
//! instance, static local page and ten heavy ephemeral synthetic tabs, with
//! level A/B/C reads and PSS grouped by process type, exactly 3 repetitions
//! with median/min/max plus Chrome, kernel and flag metadata. Synthetic
//! `/proc`-shaped fixtures drive levels B and C; one live self-read of
//! `/proc/self/status` anchors level B to the real machine. No browser is
//! launched, no accounts are used and no session numbers are claimed: logged
//! in WhatsApp/Teams figures stay pending-user (18.4).
//!
//! The 30 s settle is recorded as the procedure constant (`SETTLE_SECS`):
//! the test asserts it without wall-clock sleeping so the suite stays fast.
//! A real VPS run waits the full 30 s before each reading.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use excalibur_browser_resources::budgets::MIB;
use excalibur_browser_resources::measure::{
    HEAVY_TABS_PER_CASE, LevelReadings, MeasureEnvironment, MeasuredCase, REPETITIONS, SETTLE_SECS,
    median_of_three, summarize_triplet,
};
use excalibur_browser_resources::sampling::{
    ProcessType, enumerate_proc_root, parse_smaps_pss, parse_status_working_set, working_set_a,
};

fn evidence_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-evidence/browser/resources")
}

fn chrome_version_label() -> String {
    // Best-effort label only: never launches Chrome.
    for candidate in ["/usr/bin/google-chrome-stable", "/opt/google/chrome/chrome"] {
        if let Ok(output) = std::process::Command::new(candidate)
            .arg("--version")
            .output()
            && output.status.success()
        {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !text.is_empty() {
                return text;
            }
        }
    }
    "synthetic-no-chrome-launched".to_string()
}

fn kernel_label() -> String {
    fs::read_to_string("/proc/version")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown-kernel".to_string())
}

/// Write one synthetic process entry under a `/proc`-shaped root.
fn write_proc(root: &Path, pid: u32, kind: &str, rss_anon_kb: u64, vmswap_kb: u64, pss_kb: u64) {
    let dir = root.join(pid.to_string());
    fs::create_dir_all(&dir).unwrap();
    let status = format!(
        "Name:\tchrome\nVmRSS:\t{total} kB\nRssAnon:\t{rss_anon_kb} kB\nVmSwap:\t{vmswap_kb} kB\n",
        total = rss_anon_kb + 100,
    );
    fs::write(dir.join("status"), status).unwrap();
    fs::write(
        dir.join("cmdline"),
        format!("/opt/google/chrome/chrome --type={kind} --lang=en-US"),
    )
    .unwrap();
    fs::write(
        dir.join("smaps_rollup"),
        format!("Pss:                  {pss_kb} kB\n"),
    )
    .unwrap();
}

/// Build a synthetic tree for one repetition: browser + gpu + utility plus
/// `renderers` renderer processes with per-rep growth.
fn synthetic_sample(seed: u64, renderers: u32) -> (u64, Option<u64>, BTreeMap<ProcessType, u64>) {
    let root = std::env::temp_dir().join(format!(
        "webd-resources-measure-{}-{}-{}",
        std::process::id(),
        seed,
        renderers
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    // Browser, GPU and utility backplane (small, stable across reps).
    write_proc(&root, 100, "browser", 40_000, 0, 30_000);
    write_proc(&root, 101, "gpu-process", 20_000, 0, 15_000);
    write_proc(&root, 102, "utility", 10_000, 0, 8_000);
    // Renderers: heavy synthetic tabs grow slightly per repetition.
    for i in 0..renderers {
        let pid = 200 + i;
        let rss = 60_000 + seed * 1_000 + u64::from(i) * 500;
        let pss = 45_000 + seed * 800 + u64::from(i) * 400;
        write_proc(&root, pid, "renderer", rss, 0, pss);
    }
    let mut pids: Vec<u32> = (0..renderers).map(|i| 200 + i).collect();
    pids.extend([100, 101, 102]);
    let sample = enumerate_proc_root(&root, &pids);
    let _ = fs::remove_dir_all(&root);
    let pss_by_type = sample.pss_by_type();
    (sample.working_set_bytes, sample.pss_bytes, pss_by_type)
}

fn level_a_synthetic(seed: u64) -> u64 {
    // Synthetic cgroup readings around 900 MiB growing per repetition.
    let current = 950 * MIB + seed * 10 * MIB;
    let inactive_file = 50 * MIB;
    working_set_a(current, inactive_file)
}

#[test]
fn measurement_procedure_three_reps_with_report() {
    // Procedure constants are part of the contract.
    assert_eq!(REPETITIONS, 3);
    assert_eq!(SETTLE_SECS, 30);
    assert_eq!(HEAVY_TABS_PER_CASE, 10);

    // One live anchor: our own level-B working set from the real machine.
    let live_self = excalibur_browser_resources::sampling::current_process_working_set();
    assert!(live_self.is_some(), "live /proc/self/status must parse");

    let environment = MeasureEnvironment {
        chrome_version: chrome_version_label(),
        kernel_version: kernel_label(),
        flags: vec!["--headless=new".to_string(), "--no-first-run".to_string()],
    };

    // Cases mirror 18.3: blank, static local page, ten heavy ephemeral tabs.
    let case_specs = [
        ("blank", 1u32),
        ("static-site", 2u32),
        ("ten-heavy-tabs", 10u32),
    ];
    let mut cases: Vec<MeasuredCase> = Vec::new();
    for (name, renderers) in case_specs {
        let mut repetitions = Vec::new();
        for rep in 0..3u64 {
            let (level_b, level_c, pss_by_type) = synthetic_sample(rep, renderers);
            // PSS grouping by type is exercised on every repetition.
            assert!(pss_by_type.contains_key(&ProcessType::Renderer));
            let level_c_pss_bytes = level_c.expect("all synthetic entries carry PSS");
            // Cross-check: PSS parses independently from the fixture text.
            let reparsed =
                parse_smaps_pss("Pss:                  45000 kB\n").expect("fixture parses");
            assert_eq!(reparsed, 45_000 * 1024);
            // Cross-check: level-B parses independently too.
            let status_probe = "Name:\tchrome\nRssAnon:\t60000 kB\nVmSwap:\t0 kB\n";
            assert_eq!(parse_status_working_set(status_probe), Some(60_000 * 1024));
            repetitions.push(LevelReadings {
                level_a_bytes: Some(level_a_synthetic(rep)),
                level_b_bytes: level_b,
                level_c_pss_bytes,
            });
        }
        assert_eq!(repetitions.len(), REPETITIONS);
        cases.push(MeasuredCase {
            name: name.to_string(),
            environment: environment.clone(),
            repetitions,
        });
    }

    // Summaries use exactly median/min/max over the three repetitions.
    assert_eq!(median_of_three([3, 1, 2]), 2);
    let mut table = serde_json::Map::new();
    for case in &cases {
        let b = [
            case.repetitions[0].level_b_bytes,
            case.repetitions[1].level_b_bytes,
            case.repetitions[2].level_b_bytes,
        ];
        let c = [
            case.repetitions[0].level_c_pss_bytes,
            case.repetitions[1].level_c_pss_bytes,
            case.repetitions[2].level_c_pss_bytes,
        ];
        let (b_min, b_med, b_max) = summarize_triplet(b);
        let (c_min, c_med, c_max) = summarize_triplet(c);
        assert_eq!(case.median_level_b(), Some(b_med));
        assert_eq!(case.median_level_c(), Some(c_med));
        assert!(b_min <= b_med && b_med <= b_max);
        assert!(c_min <= c_med && c_med <= c_max);
        // Ten heavy tabs must dwarf the blank case on both levels.
        if case.name == "ten-heavy-tabs" {
            let blank = cases.iter().find(|k| k.name == "blank").unwrap();
            assert!(b_med > blank.median_level_b().unwrap());
            assert!(c_med > blank.median_level_c().unwrap());
        }
        table.insert(
            case.name.clone(),
            serde_json::json!({
                "level_b_bytes": {"min": b_min, "median": b_med, "max": b_max},
                "level_c_pss_bytes": {"min": c_min, "median": c_med, "max": c_max},
                "level_a_bytes": [
                    case.repetitions[0].level_a_bytes,
                    case.repetitions[1].level_a_bytes,
                    case.repetitions[2].level_a_bytes,
                ],
                "repetitions": case.repetitions.len(),
            }),
        );
    }

    let report = serde_json::json!({
        "procedure": "18.3",
        "settle_secs": SETTLE_SECS,
        "repetitions": REPETITIONS,
        "heavy_tabs_per_case": HEAVY_TABS_PER_CASE,
        "cases": ["blank", "static-site", "ten-heavy-tabs"],
        "environment": {
            "chrome_version": environment.chrome_version,
            "kernel_version": environment.kernel_version,
            "flags": environment.flags,
        },
        "live_self_level_b_bytes": live_self,
        "synthetic_readings": table,
        "notes": [
            "Synthetic /proc-shaped fixtures only; no browser launched.",
            "Level A uses synthetic cgroup current/inactive_file arithmetic; level B is enumerated from synthetic status files plus one live self-read anchor above.",
            "WhatsApp/Teams 1h/24h session figures (18.4) stay pending-user and are not claimed here.",
        ],
    });
    let dir = evidence_dir();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("measure.json");
    fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    assert!(path.is_file(), "report must exist at {}", path.display());
}
