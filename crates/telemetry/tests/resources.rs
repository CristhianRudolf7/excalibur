use excalibur_telemetry::{
    RunMetadata, Telemetry, TelemetryConfig,
    resources::{self, ResourceConfig, ResourceScope, ResourceSummary},
};
use excalibur_vault::Redactor;
#[cfg(target_os = "linux")]
#[test]
fn synthetic_cgroup_v2_preserves_units_peaks_and_oom_deltas() {
    let dir = tempfile::tempdir().unwrap();
    let proc = dir.path().join("proc/42");
    std::fs::create_dir_all(&proc).unwrap();
    std::fs::write(
        proc.join("status"),
        "Name:\tsynthetic\nVmRSS:\t1024 kB\nVmHWM:\t2048 kB\n",
    )
    .unwrap();
    let mut fields = vec!["0"; 30];
    fields[0] = "R";
    fields[11] = "10";
    fields[12] = "20";
    std::fs::write(
        proc.join("stat"),
        format!("42 (name with ) and spaces) {}", fields.join(" ")),
    )
    .unwrap();
    let group = dir.path().join("group");
    std::fs::create_dir(&group).unwrap();
    for (name, value) in [
        ("memory.current", "3000"),
        ("memory.peak", "5000"),
        ("pids.current", "2"),
        ("pids.peak", "4"),
        ("cpu.stat", "usage_usec 8000\nuser_usec 5000"),
        ("memory.events", "oom 2\noom_kill 1"),
    ] {
        std::fs::write(group.join(name), value).unwrap();
    }
    let config = ResourceConfig {
        process_id: 42,
        proc_root: dir.path().join("proc"),
        cgroup: Some(group.clone()),
        cgroup_scope: ResourceScope::TaskChildren,
    };
    let first = resources::sample(&config, 1000);
    assert_eq!(first.daemon_rss_bytes, Some(1024 * 1024));
    assert_eq!(first.daemon_lifetime_peak_bytes, Some(2048 * 1024));
    let rate = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    assert_eq!(first.daemon_cpu_ns, Some(30 * 1_000_000_000 / rate));
    assert_eq!(first.group_cpu_ns, Some(8_000_000));
    assert_eq!(first.group_pids_peak, Some(4));
    std::fs::write(proc.join("status"), "VmRSS: 1536 kB\nVmHWM: 2048 kB").unwrap();
    std::fs::write(group.join("memory.peak"), "9000").unwrap();
    std::fs::write(group.join("cpu.stat"), "usage_usec 12000").unwrap();
    std::fs::write(group.join("memory.events"), "oom 5\noom_kill 3").unwrap();
    let second = resources::sample(&config, 2000);
    let total = ResourceSummary::from_samples(&[first, second]);
    assert_eq!(total.daemon_rss_delta_peak_bytes, Some(512 * 1024));
    assert_eq!(total.group_memory_peak_bytes, Some(9000));
    assert_eq!(total.group_cpu_elapsed_ns, Some(4_000_000));
    assert_eq!(total.group_oom_events_delta, Some(3));
    assert_eq!(total.group_oom_kills_delta, Some(2));
    assert_eq!(total.group_scope, ResourceScope::TaskChildren);
    std::fs::remove_file(group.join("memory.peak")).unwrap();
    let missing = resources::sample(&config, 3000);
    assert_eq!(missing.group_memory_peak_bytes, None);
    assert!(missing.unavailable.contains(&"cgroup.memory.peak".into()));
}
#[cfg(target_os = "linux")]
#[test]
fn native_rss_matches_external_procfs_for_a_stopped_memory_child() {
    use std::{
        io::{BufRead, BufReader},
        process::{Command, Stdio},
    };
    let mut child=Command::new("python3").args(["-c","import os,signal; a=bytearray(24*1024*1024); print('ready',flush=True); os.kill(os.getpid(),signal.SIGSTOP); signal.pause()"] ).stdout(Stdio::piped()).spawn().unwrap();
    let mut ready = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready.trim(), "ready");
    // waitpid establishes the OS stop boundary, not a timing-based sleep.
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(child.id() as i32, &mut status, libc::WUNTRACED) },
        child.id() as i32
    );
    assert!(libc::WIFSTOPPED(status));
    let process = std::fs::read_to_string(format!("/proc/{}/status", child.id())).unwrap();
    let external = |name: &str| {
        process
            .lines()
            .find(|l| l.starts_with(name))
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u64>()
            .unwrap()
            * 1024
    };
    let measured = resources::sample(
        &ResourceConfig {
            process_id: child.id(),
            ..Default::default()
        },
        1000,
    );
    assert_eq!(measured.daemon_rss_bytes, Some(external("VmRSS:")));
    assert_eq!(
        measured.daemon_lifetime_peak_bytes,
        Some(external("VmHWM:"))
    );
    assert!(measured.daemon_rss_bytes.unwrap() >= 24 * 1024 * 1024);
    child.kill().unwrap();
    child.wait().unwrap();
}
#[tokio::test]
async fn one_second_sampler_persists_baseline_and_final_samples() {
    let dir = tempfile::tempdir().unwrap();
    let t = Telemetry::open(TelemetryConfig::new(
        dir.path(),
        RunMetadata::default(),
        Redactor::default(),
    ))
    .await
    .unwrap();
    let sampler = t.start_sampler("synthetic").await.unwrap();
    // Wait for the durable periodic sample, with a test timeout solely as a hung-test guard.
    let mut subscriber = t.subscribe(1);
    let periodic = tokio::time::timeout(std::time::Duration::from_secs(10), subscriber.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(periodic.source, "resource");
    let summary = sampler.stop().await.unwrap();
    assert!(summary.sample_count >= 3);
    assert!(summary.daemon_rss_observed_peak_bytes.is_some());
    t.shutdown().await.unwrap();
}
