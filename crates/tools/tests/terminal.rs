#![cfg(unix)]
use excalibur_tools::terminal::{
    SandboxConfig, TerminalConfig, TerminalManager, prepare_project_boundary,
};
use serde_json::json;
use std::{path::Path, time::Duration};
fn manager_with_spool(root: &Path, spool: &Path) -> TerminalManager {
    std::fs::create_dir_all(root).unwrap();
    prepare_project_boundary(root).unwrap();
    TerminalManager::new(TerminalConfig {
        spool_dir: spool.into(),
        sandbox: Some(SandboxConfig::project(root)),
        ..Default::default()
    })
    .unwrap()
}
fn manager(dir: &Path) -> TerminalManager {
    manager_with_spool(dir, &dir.join("spool"))
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
    assert!(!first.output["exited"].as_bool().unwrap(), "{first:?}");
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
    prepare_project_boundary(dir.path()).unwrap();
    let manager = TerminalManager::new(TerminalConfig {
        spool_dir: dir.path().join("spool"),
        sandbox: Some(SandboxConfig::project(dir.path())),
        redacted_secrets: vec!["synthetic-secret".into()],
        ..Default::default()
    })
    .unwrap();
    let result=manager.exec("bounded",json!({"cmd":"printf 'synthetic-'; sleep 0.05; printf 'secret\\n'; head -c 2100000 /dev/zero | tr '\\0' x; printf '\\nFINISHED\\n'","cwd":dir.path(),"yield_ms":30000,"output_bytes":4096})).await;
    assert!(result.success, "{result:?}");
    assert_eq!(result.output["exited"], true);
    assert_eq!(result.output["truncated"], true);
    let text = result.output["output"].as_str().unwrap();
    assert!(text.len() <= 4096);
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
    prepare_project_boundary(dir.path()).unwrap();
    let manager = TerminalManager::new(TerminalConfig {
        spool_dir: dir.path().join("spool"),
        sandbox: Some(SandboxConfig::project(dir.path())),
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
    let id = result.output["session_id"]
        .as_str()
        .expect("an admitted timeout retains its session and real failure details")
        .to_owned();
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
        assert!(result.success,"{result:?}");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !root.join("child-pids").is_file() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("sandbox shell must fork its child and publish the marker before daemon interruption");
        let pid=result.output["pid"].as_u64().unwrap()as u32;let mut pids=vec![pid];let mut index=0;
        while index<pids.len(){let process=pids[index];index+=1;if let Ok(children)=std::fs::read_to_string(format!("/proc/{process}/task/{process}/children")){pids.extend(children.split_whitespace().filter_map(|p|p.parse::<u32>().ok()));}}
        assert!(pids.len()>=2,"sandbox must contain real child processes");
        std::fs::write(root.join("ready"),pids.iter().map(u32::to_string).collect::<Vec<_>>().join(" ")).unwrap();
        std::future::pending::<()>().await;
    });
}

#[tokio::test]
async fn run_session_ceiling_and_eviction_are_shared_across_manager_instances() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let a = manager_with_spool(&workspace, &dir.path().join("a/spool"));
    let b = manager_with_spool(&workspace, &dir.path().join("b/spool"));
    let mut first = String::new();
    for n in 0..17 {
        let mgr = if n < 8 { &a } else { &b };
        let result = mgr
            .exec(
                "same-run-across-managers",
                json!({"cmd":"sleep 30","cwd":workspace,"yield_ms":250}),
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
    let result=manager.exec("regex-spool",json!({"cmd":"head -c 65534 /dev/zero | tr '\\0' a; printf 'CROSS_BOUNDARY_END'; head -c 600000 /dev/zero | tr '\\0' a; printf 'HIDDEN_MAGIC'; head -c 600000 /dev/zero | tr '\\0' b; printf '\\nREADY_FOR_INPUT\\n'; IFS= read -r line; printf DONE","cwd":dir.path(),"yield_ms":1000,"output_bytes":256})).await;
    let id = session(&result);
    assert_eq!(result.output["exited"], false);
    let ready=manager.io("regex-spool",json!({"action":"wait_for","session_id":id,"pattern":"READY_FOR_INPUT","timeout_ms":10000,"output_bytes":256})).await;
    assert_eq!(ready.output["matched"], true, "{ready:?}");
    assert_eq!(ready.output["exited"], false, "{ready:?}");
    assert!(
        ready.output["cursor"].as_u64().unwrap() > 1200000,
        "{ready:?}"
    );
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
        let workspace = root.path().join("workspace");
        let a = manager_with_spool(&workspace, &root.path().join("a/spool"));
        let b = manager_with_spool(&workspace, &root.path().join("b/spool"));
        let mut first = String::new();
        for n in 0..65 {
            let mgr = if n % 2 == 0 { &a } else { &b };
            let run = format!("global-owner-{n}");
            let result = mgr
                .exec(
                    &run,
                    json!({"cmd":"printf complete","cwd":workspace,"yield_ms":250}),
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

#[tokio::test]
async fn execution_limits_are_admitted_atomically_and_can_be_expanded() {
    use excalibur_types::runtime::ExecutionLimits;
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    manager
        .set_execution_limits(
            "limited",
            ExecutionLimits {
                max_processes: Some(1),
                max_terminal_sessions: Some(2),
            },
        )
        .unwrap();
    let command = json!({"cmd":"IFS= read -r line", "yield_ms":250});
    let (left, right) = tokio::join!(
        manager.exec("limited", command.clone()),
        manager.exec("limited", command.clone())
    );
    assert_eq!(usize::from(left.success) + usize::from(right.success), 1);
    let rejected = if left.success { &right } else { &left };
    assert!(rejected.output.to_string().contains("budget"));
    assert_eq!(manager.execution_resources("limited").processes, 1);
    assert_eq!(manager.execution_resources("limited").terminal_sessions, 1);
    manager
        .set_execution_limits(
            "limited",
            ExecutionLimits {
                max_processes: Some(2),
                max_terminal_sessions: Some(2),
            },
        )
        .unwrap();
    assert!(manager.exec("limited", command).await.success);
    assert_eq!(manager.execution_resources("limited").processes, 2);
    assert!(
        !manager
            .exec("limited", json!({"cmd":"touch forbidden"}))
            .await
            .success
    );
    assert!(!dir.path().join("forbidden").exists());
    assert_eq!(manager.execution_resources("other").processes, 0);
    manager.cancel_run("limited").await;
    assert_eq!(manager.execution_resources("limited").processes, 0);
    manager.shutdown().await;
}
#[tokio::test]
async fn live_status_and_reads_recheck_secrets_registered_after_capture() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    prepare_project_boundary(&root).unwrap();
    let redactor = excalibur_vault::Redactor::default();
    let manager = TerminalManager::new(TerminalConfig {
        spool_dir: temp.path().join("spool"),
        sandbox: Some(SandboxConfig::project(&root)),
        redactor: redactor.clone(),
        ..Default::default()
    })
    .unwrap();
    let secret = format!("SYNTHETIC_BEGIN_{}_END_SYNTHETIC_SECRET", "x".repeat(5000));
    let cmd = format!(
        "i=0; while [ $i -lt 25 ]; do printf 'line-%s\\n' \"$i\"; i=$((i+1)); done; printf '%s\\n' '{secret}'"
    );
    let result = manager
        .exec("late-redaction", json!({"cmd":cmd,"yield_ms":30000}))
        .await;
    let id = session(&result);
    assert_eq!(result.output["exited"], true);
    redactor
        .register_secret(&excalibur_vault::SecretValue::new(secret.as_bytes().to_vec()).unwrap());
    let status = manager.live_status("late-redaction");
    let tail = status[0]["tail"].as_str().unwrap();
    assert!(tail.contains("[REDACTED]"));
    assert!(!tail.contains("END_SYNTHETIC_SECRET"));
    assert!(tail.len() <= 2048);
    assert!(tail.lines().count() <= 20);
    let read = manager
        .io("late-redaction", json!({"action":"read","session_id":id}))
        .await;
    assert!(!read.output["output"].as_str().unwrap().contains(&secret));
    assert!(
        read.output["output"]
            .as_str()
            .unwrap()
            .contains("[REDACTED]")
    );
    manager.shutdown().await;
}
#[tokio::test]
async fn run_resource_budgets_count_commands_in_other_manager_instances() {
    use excalibur_types::runtime::ExecutionLimits;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let first = manager_with_spool(&root, &temp.path().join("spool-a"));
    let second = manager_with_spool(&root, &temp.path().join("spool-b"));
    let run = format!("global-resource-budget-{}", temp.path().display());
    let limits = ExecutionLimits {
        max_processes: Some(1),
        max_terminal_sessions: None,
    };
    first.set_execution_limits(&run, limits.clone()).unwrap();
    second.set_execution_limits(&run, limits).unwrap();
    let started = first
        .exec(
            &run,
            json!({"cmd":"printf 'READY\\n'; sleep 30","yield_ms":250}),
        )
        .await;
    session(&started);
    assert_eq!(second.execution_resources(&run).processes, 1);
    let denied = second
        .exec(&run, json!({"cmd":"touch must-not-start","yield_ms":250}))
        .await;
    assert!(!denied.success);
    assert!(!root.join("must-not-start").exists());
    first.shutdown().await;
    second.shutdown().await;
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn dropping_the_last_host_manager_terminates_its_owned_process_group() {
    struct Cleanup(i32);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let manager = manager(temp.path());
    let peer = manager.clone();
    let first = manager
        .exec(
            "drop-owner",
            json!({"cmd":"printf 'READY\\n'; sleep 60","yield_ms":250}),
        )
        .await;
    session(&first);
    let pid = first.output["pid"].as_u64().unwrap() as i32;
    let _cleanup = Cleanup(pid);
    drop(manager);
    assert_eq!(peer.live_status("drop-owner")[0]["exited"], false);
    drop(peer);
    let dead = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let live = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .is_some_and(|s| s.split(") ").nth(1).is_some_and(|s| !s.starts_with('Z')));
            if !live {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        dead.is_ok(),
        "owned command group survived dropping all host handles"
    );
}
#[tokio::test]
async fn output_budget_includes_marker_and_lossy_utf8_while_full_bytes_remain_spooled() {
    let temp = tempfile::tempdir().unwrap();
    let manager = manager(temp.path());
    let result=manager.exec("binary-budget",json!({"cmd":"printf HEAD; head -c 16384 /dev/zero | tr '\\0' '\\377'; printf TAIL","yield_ms":30000,"output_bytes":256})).await;
    session(&result);
    let output = result.output["output"].as_str().unwrap();
    assert!(output.len() <= 256);
    assert!(output.starts_with("HEAD"));
    assert!(output.ends_with("TAIL"));
    assert_eq!(result.output["truncated"], true);
    let raw = std::fs::read(result.output["spool_path"].as_str().unwrap()).unwrap();
    assert_eq!(raw.len(), 16392);
    assert_eq!(&raw[..4], b"HEAD");
    assert_eq!(&raw[raw.len() - 4..], b"TAIL");
    manager.shutdown().await;
}
