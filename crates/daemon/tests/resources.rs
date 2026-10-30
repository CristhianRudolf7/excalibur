#[cfg(target_os = "linux")]
#[test]
fn rss_is_native_and_includes_an_actual_child() {
    let mut child = std::process::Command::new("sleep")
        .arg("2")
        .spawn()
        .unwrap();
    let sample = excalibur_daemon::sample_memory().unwrap();
    assert_eq!(sample.daemon.pid, std::process::id());
    assert!(sample.daemon.rss_bytes > 0);
    assert!(sample.descendants.iter().any(|p| p.pid == child.id()));
    assert!(sample.total_rss_bytes >= sample.daemon.rss_bytes);
    let _ = child.kill();
    child.wait().unwrap();
}
