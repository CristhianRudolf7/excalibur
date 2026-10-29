#![cfg(unix)]
use excalibur_tools::terminal::{TerminalConfig, TerminalManager};
use serde_json::json;
use std::{path::Path, time::Duration};
fn manager(dir: &Path) -> TerminalManager {
    TerminalManager::new(TerminalConfig {
        spool_dir: dir.join("spool"),
        ..Default::default()
    })
    .unwrap()
}
fn session(result: &excalibur_types::runtime::ToolResult) -> String {
    assert!(result.success, "{result:?}");
    result.output["session_id"].as_str().unwrap().to_string()
}
#[tokio::test]
async fn pipe_sessions_accept_input_and_wait_for_patterns_without_polling() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let first=manager.exec("run-a",json!({"cmd":"printf 'ready\\n'; IFS= read -r line; printf 'got:%s\\n' \"$line\"","cwd":dir.path(),"yield_ms":250})).await;
    let id = session(&first);
    assert!(!first.output["exited"].as_bool().unwrap());
    let ready = manager
        .io(
            "run-a",
            json!({"action":"wait_for","session_id":id,"pattern":"ready","timeout_ms":1000}),
        )
        .await;
    assert_eq!(ready.output["matched"], true);
    let own = manager
        .io("run-b", json!({"action":"read","session_id":id}))
        .await;
    assert!(!own.success);
    let written = manager
        .io(
            "run-a",
            json!({"action":"write","session_id":id,"text":"hello\n"}),
        )
        .await;
    assert!(written.success);
    let finished = manager
        .io(
            "run-a",
            json!({"action":"wait_for","session_id":id,"timeout_ms":5000}),
        )
        .await;
    assert_eq!(finished.output["exited"], true);
    assert_eq!(finished.output["exit_code"], 0);
    assert!(
        finished.output["output"]
            .as_str()
            .unwrap()
            .contains("got:hello")
    );
    let end = finished.output["cursor"].as_u64().unwrap();
    let empty = manager
        .io(
            "run-a",
            json!({"action":"read","session_id":id,"cursor":end}),
        )
        .await;
    assert_eq!(empty.output["output"], "");
    manager.shutdown().await;
}
#[tokio::test]
async fn pty_supports_named_keys_and_group_signals() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let first=manager.exec("pty",json!({"cmd":"test -t 0 && printf 'PTY_READY\\n'; IFS= read -r line; printf 'received:%s\\n' \"$line\"","cwd":dir.path(),"tty":true,"yield_ms":250})).await;
    let id = session(&first);
    assert!(
        first.output["output"]
            .as_str()
            .unwrap()
            .contains("PTY_READY")
    );
    assert!(
        manager
            .io(
                "pty",
                json!({"action":"write","session_id":id,"text":"interactive"})
            )
            .await
            .success
    );
    assert!(
        manager
            .io(
                "pty",
                json!({"action":"keys","session_id":id,"keys":["enter"]})
            )
            .await
            .success
    );
    let final_result = manager
        .io(
            "pty",
            json!({"action":"wait_for","session_id":id,"timeout_ms":5000}),
        )
        .await;
    assert_eq!(final_result.output["exit_code"], 0);
    assert!(
        final_result.output["output"]
            .as_str()
            .unwrap()
            .contains("received:interactive")
    );
    let running = manager
        .exec(
            "pty",
            json!({"cmd":"sleep 30","cwd":dir.path(),"tty":true,"yield_ms":250}),
        )
        .await;
    let id = session(&running);
    let signal = manager
        .io(
            "pty",
            json!({"action":"signal","session_id":id,"signal":"interrupt"}),
        )
        .await;
    assert!(signal.success);
    let done = manager
        .io(
            "pty",
            json!({"action":"wait_for","session_id":id,"timeout_ms":5000}),
        )
        .await;
    assert_eq!(done.output["exited"], true);
    assert_ne!(done.output["exit_code"], 0);
    manager.shutdown().await;
}
#[tokio::test]
async fn output_spooling_is_complete_while_results_and_live_status_are_bounded_and_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let manager = TerminalManager::new(TerminalConfig {
        spool_dir: dir.path().join("spool"),
        redacted_secrets: vec!["synthetic-secret".into()],
        ..Default::default()
    })
    .unwrap();
    let result=manager.exec("bounded",json!({"cmd":"printf 'synthetic-'; sleep 0.05; printf 'secret\\n'; head -c 2100000 /dev/zero | tr '\\0' x; printf '\\nFINISHED\\n'","cwd":dir.path(),"yield_ms":30000,"output_bytes":4096})).await;
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["exited"], true);
    assert_eq!(result.output["truncated"], true);
    let text = result.output["output"].as_str().unwrap();
    assert!(text.len() < 4300);
    assert!(text.contains("[REDACTED]"));
    assert!(text.contains("FINISHED"));
    assert!(!text.contains("synthetic-secret"));
    let full = std::fs::read_to_string(result.output["spool_path"].as_str().unwrap()).unwrap();
    assert!(full.len() > 2100000);
    assert!(!full.contains("synthetic-secret"));
    assert!(full.contains("[REDACTED]"));
    for status in manager.live_status("bounded") {
        let tail = status["tail"].as_str().unwrap();
        assert!(tail.len() <= 2048);
        assert!(tail.lines().count() <= 20);
        assert!(!tail.contains("synthetic-secret"));
    }
    manager.shutdown().await;
}
#[tokio::test]
async fn session_limit_evicts_oldest_unprotected_and_notifies_its_owner() {
    let dir = tempfile::tempdir().unwrap();
    let manager = TerminalManager::new(TerminalConfig {
        spool_dir: dir.path().join("spool"),
        max_per_run: 3,
        max_global: 3,
        protected_recent: 2,
        ..Default::default()
    })
    .unwrap();
    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(session(
            &manager
                .exec(
                    "owner",
                    json!({"cmd":"sleep 30","cwd":dir.path(),"yield_ms":250}),
                )
                .await,
        ));
    }
    assert_eq!(manager.live_status("owner").len(), 3);
    let notices = manager.take_notices("owner");
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["session_id"], ids[0]);
    assert!(
        !manager
            .io("owner", json!({"action":"read","session_id":ids[0]}))
            .await
            .success
    );
    assert!(
        manager
            .io("owner", json!({"action":"read","session_id":ids[3]}))
            .await
            .success
    );
    manager.shutdown().await;
}
#[tokio::test]
async fn process_deadline_escalates_and_environment_disables_pagers() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let env=manager.exec("env",json!({"cmd":"printf '%s,%s,%s,%s' \"$PAGER\" \"$GIT_PAGER\" \"$TERM\" \"$NO_COLOR\"","cwd":dir.path(),"yield_ms":1000})).await;
    assert_eq!(env.output["output"], "cat,cat,dumb,1");
    let result=manager.exec("timeout",json!({"cmd":"trap '' TERM; sleep 30","cwd":dir.path(),"yield_ms":250,"timeout_ms":250})).await;
    let id = session(&result);
    let done = manager
        .io(
            "timeout",
            json!({"action":"wait_for","session_id":id,"timeout_ms":6000}),
        )
        .await;
    assert_eq!(done.output["timed_out"], true);
    assert_eq!(done.output["exited"], true);
    assert_eq!(manager.environment()["os"], std::env::consts::OS);
    manager.shutdown().await;
}
#[cfg(target_os = "linux")]
#[test]
fn daemon_sigkill_does_not_leave_the_command_or_its_child_running() {
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("ready");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "daemon_process_helper",
            "--nocapture",
        ])
        .env("EXCALIBUR_TERMINAL_CRASH_ROOT", dir.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !ready.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "helper did not become ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let ids = std::fs::read_to_string(&ready)
        .unwrap()
        .split_whitespace()
        .map(|s| s.parse::<i32>().unwrap())
        .collect::<Vec<_>>();
    child.kill().unwrap();
    child.wait().unwrap();
    for pid in ids {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let live = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .is_some_and(|s| s.split(") ").nth(1).is_some_and(|s| !s.starts_with('Z')));
            if !live {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "process {pid} survived daemon kill"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
#[cfg(target_os = "linux")]
#[test]
#[ignore = "subprocess helper invoked by daemon_sigkill_does_not_leave_the_command_or_its_child_running"]
fn daemon_process_helper() {
    let root = std::path::PathBuf::from(std::env::var_os("EXCALIBUR_TERMINAL_CRASH_ROOT").unwrap());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async{
        let manager=manager(&root);
        let result=manager.exec("crash",json!({"cmd":"sleep 60 & child=$!; printf '%s %s' \"$$\" \"$child\" > child-pids; wait","cwd":root,"yield_ms":250})).await;
        assert!(result.success,"{result:?}");let pids=std::fs::read(root.join("child-pids")).unwrap();std::fs::write(root.join("ready"),pids).unwrap();
        std::future::pending::<()>().await;
    });
}

#[tokio::test]
async fn run_session_ceiling_and_eviction_are_shared_across_manager_instances() {
    let dir = tempfile::tempdir().unwrap();
    let a = manager(&dir.path().join("a"));
    let b = manager(&dir.path().join("b"));
    let mut first = String::new();
    for n in 0..17 {
        let mgr = if n < 8 { &a } else { &b };
        let result = mgr
            .exec(
                "same-run-across-managers",
                json!({"cmd":"sleep 30","cwd":dir.path(),"yield_ms":250}),
            )
            .await;
        if n == 0 {
            first = session(&result);
        } else {
            session(&result);
        }
    }
    assert_eq!(
        a.live_status("same-run-across-managers").len()
            + b.live_status("same-run-across-managers").len(),
        16
    );
    let notices = a.take_notices("same-run-across-managers");
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["session_id"], first);
    assert!(b.take_notices("same-run-across-managers").is_empty());
    tokio::join!(a.shutdown(), b.shutdown());
}

#[tokio::test]
async fn wait_regex_searches_the_complete_spool_across_chunks_and_resident_gap() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let result=manager.exec("regex-spool",json!({"cmd":"head -c 65534 /dev/zero | tr '\\0' a; printf 'CROSS_BOUNDARY_END'; head -c 600000 /dev/zero | tr '\\0' a; printf 'HIDDEN_MAGIC'; head -c 600000 /dev/zero | tr '\\0' b; IFS= read -r line; printf DONE","cwd":dir.path(),"yield_ms":1000,"output_bytes":256})).await;
    let id = session(&result);
    assert_eq!(result.output["exited"], false);
    assert!(result.output["cursor"].as_u64().unwrap() > 1200000);
    for pattern in ["CROSS_BOUNDARY_END", "HIDDEN_MAGIC"] {
        let found=manager.io("regex-spool",json!({"action":"wait_for","session_id":id,"pattern":pattern,"timeout_ms":0,"output_bytes":256})).await;
        assert!(found.success, "{found:?}");
        assert_eq!(found.output["matched"], true);
        assert_eq!(found.output["exited"], false);
    }
    manager
        .io(
            "regex-spool",
            json!({"action":"write","session_id":id,"text":"finish\n"}),
        )
        .await;
    let done = manager
        .io(
            "regex-spool",
            json!({"action":"wait_for","session_id":id,"timeout_ms":5000}),
        )
        .await;
    assert_eq!(done.output["exit_code"], 0);
    assert!(
        std::fs::read_to_string(done.output["spool_path"].as_str().unwrap())
            .unwrap()
            .ends_with("DONE")
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn archived_output_releases_named_spool_but_cursor_and_pattern_reads_keep_working() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let result = manager
        .exec(
            "archived",
            json!({"cmd":"printf 'durable-output'","cwd":dir.path(),"yield_ms":1000}),
        )
        .await;
    let id = session(&result);
    let path = std::path::PathBuf::from(result.output["spool_path"].as_str().unwrap());
    assert!(path.exists());
    let hash = "a".repeat(64);
    manager.ack_output("archived", &id, &hash).unwrap();
    assert!(!path.exists());
    let reread = manager
        .io("archived", json!({"action":"read","session_id":id}))
        .await;
    assert_eq!(reread.output["output"], "durable-output");
    assert_eq!(reread.full_output_blob.as_deref(), Some(hash.as_str()));
    assert!(reread.output["spool_path"].is_null());
    let found = manager
        .io(
            "archived",
            json!({"action":"wait_for","session_id":id,"pattern":"durable-output","timeout_ms":0}),
        )
        .await;
    assert_eq!(found.output["matched"], true);
    manager.shutdown().await;
}

#[tokio::test]
async fn unclaimed_output_cleanup_respects_active_leases_and_minimum_retention() {
    for _ in 0..8 {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let result = manager
            .exec(
                "leased",
                json!({"cmd":"printf active; sleep 30","cwd":dir.path(),"yield_ms":250}),
            )
            .await;
        let ready=manager.io("leased",json!({"action":"wait_for","session_id":session(&result),"pattern":"active","timeout_ms":5000})).await;
        assert_eq!(ready.output["matched"], true);
        let active = std::path::PathBuf::from(result.output["spool_path"].as_str().unwrap());
        let old = manager.spool_dir().join("unclaimed.output");
        let young = manager.spool_dir().join("recent.output");
        std::fs::write(&old, b"unclaimed").unwrap();
        std::fs::write(&young, b"young").unwrap();
        let modified = std::time::SystemTime::now() - Duration::from_secs(90000);
        for path in [&old, &active] {
            std::fs::File::open(path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
        }
        assert!(manager.cleanup_released(1).is_err());
        assert_eq!(manager.cleanup_released(86400).unwrap(), 1);
        assert!(!old.exists());
        assert!(active.exists());
        assert!(young.exists());
        manager.shutdown().await;
        // Closing can emit final output and update mtime. Age the immutable
        // completed spool explicitly rather than treating new output as old.
        std::fs::File::open(&active)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        {
            let released = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&active)
                .unwrap();
            assert!(
                fs2::FileExt::try_lock_exclusive(&released).is_ok(),
                "shutdown must release the spool lease"
            );
        }
        assert_eq!(manager.cleanup_released(86400).unwrap(), 1);
        assert!(!active.exists());
        assert!(young.exists());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn process_survives_retirement_of_temporary_tokio_blocking_threads() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_keep_alive(Duration::from_millis(1))
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let manager = manager(dir.path());
        let result = manager
            .exec(
                "thread-lifetime",
                json!({"cmd":"sleep 0.5; printf STILL_RUNNING","cwd":dir.path(),"yield_ms":250}),
            )
            .await;
        let id = session(&result);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let done = manager
            .io(
                "thread-lifetime",
                json!({"action":"wait_for","session_id":id,"timeout_ms":5000}),
            )
            .await;
        assert_eq!(done.output["exit_code"], 0);
        assert_eq!(done.output["output"], "STILL_RUNNING");
        manager.shutdown().await;
    });
}

#[test]
fn global_session_ceiling_is_enforced_across_multiple_managers() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "global_session_ceiling_helper",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "isolated subprocess invoked by global_session_ceiling_is_enforced_across_multiple_managers"]
fn global_session_ceiling_helper() {
    let root = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let a = manager(&root.path().join("a"));
        let b = manager(&root.path().join("b"));
        let mut first = String::new();
        for n in 0..65 {
            let mgr = if n % 2 == 0 { &a } else { &b };
            let run = format!("global-owner-{n}");
            let result = mgr
                .exec(
                    &run,
                    json!({"cmd":"printf complete","cwd":root.path(),"yield_ms":250}),
                )
                .await;
            let id = session(&result);
            if n == 0 {
                first = id;
            }
        }
        let retained = (0..65)
            .map(|n| {
                let mgr = if n % 2 == 0 { &a } else { &b };
                mgr.live_status(&format!("global-owner-{n}")).len()
            })
            .sum::<usize>();
        assert_eq!(retained, 64);
        let notices = a.take_notices("global-owner-0");
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["session_id"], first);
        assert!(
            !a.io(
                "global-owner-0",
                json!({"action":"read","session_id":first})
            )
            .await
            .success
        );
        tokio::join!(a.shutdown(), b.shutdown());
    });
}
