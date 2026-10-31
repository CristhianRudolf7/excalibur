#![cfg(target_os = "linux")]
//! A daemon started under `nohup`, as a background shell job or by a supervisor
//! that ignores signals inherits ignored SIGINT/SIGQUIT/SIGHUP. Tool processes
//! must still start with default dispositions so interrupts reach them. This
//! binary deliberately ignores those signals process-wide, so it holds only the
//! tests that need that state.
use excalibur_tools::terminal::{
    SandboxConfig, TerminalConfig, TerminalManager, prepare_project_boundary,
};
use serde_json::json;
use std::path::Path;

fn ignore_job_control_signals() {
    for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGHUP] {
        let previous = unsafe { libc::signal(signal, libc::SIG_IGN) };
        assert_ne!(previous, libc::SIG_ERR);
    }
}

fn manager(dir: &Path) -> TerminalManager {
    prepare_project_boundary(dir).unwrap();
    TerminalManager::new(TerminalConfig {
        spool_dir: dir.join("spool"),
        sandbox: Some(SandboxConfig::project(dir)),
        ..Default::default()
    })
    .unwrap()
}

fn session(result: &excalibur_types::runtime::ToolResult) -> String {
    assert!(result.success, "{result:?}");
    result.output["session_id"].as_str().unwrap().to_string()
}

async fn interrupt_terminates(manager: &TerminalManager, dir: &Path, tty: bool) {
    let running = manager
        .exec(
            "signals",
            json!({"cmd":"sleep 30","cwd":dir,"tty":tty,"yield_ms":250}),
        )
        .await;
    let id = session(&running);
    assert_eq!(running.output["exited"], false, "{running:?}");
    let signal = manager
        .io(
            "signals",
            json!({"action":"signal","session_id":id,"signal":"interrupt"}),
        )
        .await;
    assert!(signal.success, "{signal:?}");
    let done = manager
        .io(
            "signals",
            json!({"action":"wait_for","session_id":id,"timeout_ms":5000}),
        )
        .await;
    assert_eq!(
        done.output["exited"], true,
        "interrupt was ignored (tty={tty}): {done:?}"
    );
    assert_ne!(done.output["exit_code"], 0);
}

#[tokio::test]
async fn children_start_with_default_dispositions_even_when_the_host_ignores_signals() {
    ignore_job_control_signals();
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());

    // The kernel's own view of the confined tool process: nothing ignored, nothing blocked.
    let status = manager
        .exec(
            "signals",
            json!({"cmd":"grep -E '^Sig(Ign|Blk):' /proc/self/status","cwd":dir.path(),"yield_ms":10000}),
        )
        .await;
    assert!(status.success, "{status:?}");
    assert_eq!(status.output["exit_code"], 0, "{status:?}");
    let text = status.output["output"].as_str().unwrap();
    for line in text.lines() {
        let (name, mask) = line.split_once(':').unwrap();
        assert_eq!(
            u64::from_str_radix(mask.trim(), 16).unwrap(),
            0,
            "{name} not clear in tool process: {text}"
        );
    }
    assert_eq!(text.lines().count(), 2, "{text}");

    interrupt_terminates(&manager, dir.path(), true).await;
    interrupt_terminates(&manager, dir.path(), false).await;
    manager.shutdown().await;
}
