//! Sampling fixtures: synthetic /proc trees plus one live self-read.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use excalibur_browser_resources::sampling::{
    InstanceClass, ProcessType, SampleLevel, classify_cmdline, current_process_working_set,
    desired_oom_score_adj, enumerate_proc_root, oom_raise_allowed, parse_byte_counter,
    parse_memory_events, parse_memory_stat_inactive_file, parse_psi_some_avg10, parse_smaps_pss,
    parse_status_working_set, psi_triggers_eviction, working_set_a,
};

fn fixture_root() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "browser-resources-proc-{}-{}",
        std::process::id(),
        "sampling"
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_proc(root: &std::path::Path, pid: u32, status: &str, rollup: &str, cmdline: &str) {
    let dir = root.join(pid.to_string());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("status"), status).unwrap();
    fs::write(dir.join("smaps_rollup"), rollup).unwrap();
    fs::write(dir.join("cmdline"), cmdline).unwrap();
}

fn status_for(anon_kb: u64, swap_kb: u64) -> String {
    format!("Name:\tchrome\nRssAnon:\t{anon_kb} kB\nVmSwap:\t{swap_kb} kB\n")
}

fn rollup_for(pss_kb: u64) -> String {
    format!("Rss: 1000 kB\nPss: {pss_kb} kB\n")
}

#[test]
fn classify_all_chrome_types() {
    assert_eq!(
        classify_cmdline("/opt/chrome/chrome --headless"),
        ProcessType::Browser
    );
    assert_eq!(
        classify_cmdline("chrome --type=zygote --no-sandbox"),
        ProcessType::Zygote
    );
    assert_eq!(
        classify_cmdline("chrome --type=gpu-process --field-trial-handle"),
        ProcessType::Gpu
    );
    assert_eq!(
        classify_cmdline("chrome --type=utility --utility-sub-type=network.mojom.NetworkService"),
        ProcessType::Utility
    );
    assert_eq!(
        classify_cmdline("chrome --type=renderer --lang=en"),
        ProcessType::Renderer
    );
    assert_eq!(
        classify_cmdline("chrome --type=crashpad-handler --no-rate-limit"),
        ProcessType::Crashpad
    );
}

#[test]
fn level_b_parses_rss_anon_plus_swap() {
    let status = status_for(1000, 250);
    assert_eq!(parse_status_working_set(&status), Some(1250 * 1024));
    assert_eq!(parse_status_working_set("Name:\tx\n"), None);
}

#[test]
fn level_c_parses_pss_rollup() {
    assert_eq!(parse_smaps_pss(&rollup_for(257_000)), Some(257_000 * 1024));
    assert_eq!(parse_smaps_pss("Rss: 1 kB\n"), None);
}

#[test]
fn level_a_subtracts_inactive_file() {
    assert_eq!(working_set_a(2_000_000_000, 500_000_000), 1_500_000_000);
    assert_eq!(working_set_a(100, 200), 0);
    let stat = "total_inactive_file 0\ninactive_file 4096\nactive_file 8\n";
    assert_eq!(parse_memory_stat_inactive_file(stat), Some(4096));
    assert_eq!(parse_byte_counter("1610612736\n"), Some(1_610_612_736));
}

#[test]
fn events_and_psi_thresholds() {
    let events = parse_memory_events("low 0\nhigh 3\nmax 0\noom 0\noom_kill 1\n");
    assert_eq!(events.high, 3);
    assert_eq!(events.oom_kill, 1);
    assert_eq!(
        parse_psi_some_avg10("some avg10=12.50 avg60=1.00 total=0\n"),
        Some(12.5)
    );
    assert!(psi_triggers_eviction(10.01));
    assert!(!psi_triggers_eviction(10.0));
    assert_eq!(SampleLevel::A.interval_secs(), 2);
    assert_eq!(SampleLevel::B.interval_secs(), 10);
    assert_eq!(SampleLevel::C.interval_secs(), 60);
    assert_eq!(SampleLevel::D.interval_secs(), 0);
}

#[test]
fn enumerate_full_instance_tree_matches_independent_total() {
    let root = fixture_root();
    // Browser + zygote + gpu + utility + renderer + crashpad = 6 processes.
    let procs: Vec<(u32, u64, u64, &str)> = vec![
        (101, 50_000, 40_000, "/opt/chrome/chrome --headless"),
        (102, 5_000, 4_000, "chrome --type=zygote"),
        (103, 30_000, 20_000, "chrome --type=gpu-process"),
        (
            104,
            20_000,
            15_000,
            "chrome --type=utility --utility-sub-type=network.mojom.NetworkService",
        ),
        (105, 120_000, 90_000, "chrome --type=renderer --lang=en"),
        (106, 2_000, 1_500, "chrome --type=crashpad-handler"),
    ];
    for (pid, anon, pss, cmd) in &procs {
        write_proc(&root, *pid, &status_for(*anon, 0), &rollup_for(*pss), cmd);
    }
    let pids: Vec<u32> = procs.iter().map(|(p, _, _, _)| *p).collect();
    let sample = enumerate_proc_root(&root, &pids);
    assert_eq!(sample.entries.len(), 6);
    // Independent totals from the same instant agree.
    let expected_b: u64 = procs.iter().map(|(_, anon, _, _)| anon * 1024).sum();
    let expected_c: u64 = procs.iter().map(|(_, _, pss, _)| pss * 1024).sum();
    assert_eq!(sample.working_set_bytes, expected_b);
    assert_eq!(sample.pss_bytes, Some(expected_c));
    // Every class present.
    let kinds: Vec<ProcessType> = sample
        .entries
        .iter()
        .map(|e| e.process_type.clone())
        .collect();
    assert!(kinds.contains(&ProcessType::Browser));
    assert!(kinds.contains(&ProcessType::Zygote));
    assert!(kinds.contains(&ProcessType::Gpu));
    assert!(kinds.contains(&ProcessType::Utility));
    assert!(kinds.contains(&ProcessType::Renderer));
    assert!(kinds.contains(&ProcessType::Crashpad));
    // PSS grouped by type is exact.
    let by_type: BTreeMap<ProcessType, u64> = sample.pss_by_type();
    assert_eq!(by_type[&ProcessType::Renderer], 90_000 * 1024);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn oom_policy_prefers_ephemeral_renderers() {
    assert_eq!(
        desired_oom_score_adj(&ProcessType::Renderer, InstanceClass::Ephemeral),
        1000
    );
    assert_eq!(
        desired_oom_score_adj(&ProcessType::Renderer, InstanceClass::Persistent),
        300
    );
    assert_eq!(
        desired_oom_score_adj(&ProcessType::Gpu, InstanceClass::Ephemeral),
        200
    );
    assert_eq!(
        desired_oom_score_adj(&ProcessType::Browser, InstanceClass::Ephemeral),
        0
    );
    // Same-user raise is allowed; lowering needs privilege.
    assert!(oom_raise_allowed(300, 1000, true));
    assert!(!oom_raise_allowed(300, 1000, false));
    assert!(!oom_raise_allowed(300, 0, true));
}

#[test]
fn live_proc_self_read_is_sane() {
    let working = current_process_working_set();
    assert!(working.is_some());
}
