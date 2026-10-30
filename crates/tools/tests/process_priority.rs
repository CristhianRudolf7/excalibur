#![cfg(target_os = "linux")]
use excalibur_tools::terminal::{
    SandboxConfig, TerminalConfig, TerminalManager, prepare_project_boundary,
};
use serde_json::json;
#[tokio::test]
async fn confined_tool_processes_have_lower_scheduler_and_io_priority() {
    let project = tempfile::tempdir().unwrap();
    let spool = tempfile::tempdir().unwrap();
    prepare_project_boundary(project.path()).unwrap();
    let manager = TerminalManager::new(TerminalConfig {
        spool_dir: spool.path().into(),
        sandbox: Some(SandboxConfig::project(project.path())),
        ..Default::default()
    })
    .unwrap();
    let first = manager
        .exec(
            "priority",
            json!({"cmd":"ps -o ni= -p $$; ionice -p $$","cwd":project.path(),"yield_ms":1000}),
        )
        .await;
    assert!(first.success, "{first:?}");
    let output = if first.output["exited"] == true {
        first.output.clone()
    } else {
        manager.io("priority",json!({"action":"wait_for","session_id":first.output["session_id"],"timeout_ms":5000})).await.output
    };
    assert_eq!(output["exit_code"], 0, "{output}");
    let text = output["output"].as_str().unwrap();
    assert!(
        text.lines()
            .any(|line| line.trim().parse::<i32>().is_ok_and(|n| n >= 10)),
        "{text}"
    );
    assert!(text.contains("best-effort: prio 7"), "{text}");
    manager.shutdown().await;
}
#[tokio::test]
async fn pause_between_effects_preserves_declared_background_sessions_but_cancel_reaps_them() {
    use excalibur_store::{Database, DatabaseKind};
    use excalibur_tools::{LocalTools, ToolPolicy};
    use excalibur_types::runtime::ToolRuntime;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    prepare_project_boundary(&root).unwrap();
    let db = Database::open(
        temp.path().join("data/main.db"),
        DatabaseKind::Main,
        Default::default(),
    )
    .unwrap();
    let tools = LocalTools::new(
        db.clone(),
        root.clone(),
        ToolPolicy {
            sandbox: Some(SandboxConfig::project(&root)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let manager = tools.terminals();
    let started = manager
        .exec(
            "background",
            json!({"cmd":"sleep 30","cwd":root,"yield_ms":250}),
        )
        .await;
    assert!(started.success, "{started:?}");
    assert_eq!(started.output["exited"], false);
    assert!(
        !tools
            .interrupt_execution("background", false, None)
            .await
            .unwrap()
    );
    assert!(manager.execution_resources("background").processes > 0);
    assert!(
        tools
            .interrupt_execution("background", true, None)
            .await
            .unwrap()
    );
    assert_eq!(manager.execution_resources("background").processes, 0);
    manager.shutdown().await;
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn memory_pressure_terminates_only_the_largest_owned_process_group() {
    let project = tempfile::tempdir().unwrap();
    let spool = tempfile::tempdir().unwrap();
    prepare_project_boundary(project.path()).unwrap();
    let manager = TerminalManager::new(TerminalConfig {
        spool_dir: spool.path().into(),
        sandbox: Some(SandboxConfig::project(project.path())),
        ..Default::default()
    })
    .unwrap();
    let large=manager.exec("memory",json!({"cmd":"python3 -c 'import time; data=bytearray(32*1024*1024); print(\"allocated\", flush=True); time.sleep(30)'","cwd":project.path(),"yield_ms":250})).await;
    assert!(large.success, "{large:?}");
    let large_id = large.output["session_id"].as_str().unwrap();
    let ready=manager.io("memory",json!({"action":"wait_for","session_id":large_id,"pattern":"allocated","timeout_ms":5000})).await;
    assert_eq!(ready.output["matched"], true, "{ready:?}");
    let small = manager
        .exec(
            "memory",
            json!({"cmd":"sleep 30","cwd":project.path(),"yield_ms":250}),
        )
        .await;
    assert!(small.success, "{small:?}");
    assert_eq!(
        manager
            .terminate_largest("memory")
            .await
            .unwrap()
            .as_deref(),
        Some(large_id)
    );
    let killed = manager
        .io("memory", json!({"action":"read","session_id":large_id}))
        .await;
    assert!(!killed.success, "{killed:?}");
    assert_eq!(killed.output["exited"], true);
    assert_eq!(killed.output["termination_reason"], "memory_exhaustion");
    assert_eq!(
        killed.output["error"],
        "terminated due to memory exhaustion"
    );
    let live = manager
        .io(
            "memory",
            json!({"action":"read","session_id":small.output["session_id"]}),
        )
        .await;
    assert_eq!(live.output["exited"], false, "{live:?}");
    manager.shutdown().await;
}

#[test]
fn an_already_background_host_never_raises_tool_priority() {
    let output = std::process::Command::new("nice")
        .args(["-n", "15"])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "background_host_priority_helper",
            "--ignored",
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
#[ignore = "subprocess fixture with inherited nice 15"]
fn background_host_priority_helper() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let project = tempfile::tempdir().unwrap();
        let spool = tempfile::tempdir().unwrap();
        prepare_project_boundary(project.path()).unwrap();
        let manager = TerminalManager::new(TerminalConfig {
            spool_dir: spool.path().into(),
            sandbox: Some(SandboxConfig::project(project.path())),
            ..Default::default()
        })
        .unwrap();
        let result = manager
            .exec(
                "inherited",
                json!({"cmd":"ps -o ni= -p $$","cwd":project.path(),"yield_ms":1000}),
            )
            .await;
        assert!(result.success, "{result:?}");
        assert!(
            result.output["output"]
                .as_str()
                .unwrap()
                .lines()
                .any(|line| line.trim().parse::<i32>().is_ok_and(|n| n >= 15)),
            "{result:?}"
        );
        manager.shutdown().await;
    });
}

#[tokio::test]
async fn a_real_delegated_cgroup_applies_a_lower_cpu_weight_when_available() {
    let parent = std::env::var_os("EXCALIBUR_TEST_CGROUP")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            let current = std::fs::read_to_string("/proc/self/cgroup").ok()?;
            let relative = current.lines().find_map(|line| line.strip_prefix("0::"))?;
            Some(std::path::Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/')))
        })
        .unwrap();
    let group = parent.join(format!("excalibur-priority-test-{}", std::process::id()));
    if let Err(error) = std::fs::create_dir(&group) {
        eprintln!("cgroup verification unavailable: no writable delegation: {error}");
        return;
    }
    if !group.join("cpu.weight").exists() {
        std::fs::remove_dir(&group).unwrap();
        eprintln!("cgroup verification unavailable: CPU controller not delegated");
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let spool = tempfile::tempdir().unwrap();
    prepare_project_boundary(project.path()).unwrap();
    let mut sandbox = SandboxConfig::project(project.path());
    sandbox.cgroup_directory = Some(group.clone());
    let manager = TerminalManager::new(TerminalConfig {
        spool_dir: spool.path().into(),
        sandbox: Some(sandbox),
        ..Default::default()
    })
    .unwrap();
    let result=manager.exec("cgroup-priority",json!({"cmd":"cat /proc/self/cgroup; ps -o ni= -p $$; sleep 10","cwd":project.path(),"yield_ms":250})).await;
    assert!(result.success, "{result:?}");
    let parent_weight: u32 = std::fs::read_to_string(parent.join("cpu.weight"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let weight: u32 = std::fs::read_to_string(group.join("cpu.weight"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(weight < parent_weight);
    assert_eq!(weight, 10.min(parent_weight - 1));
    // The sandbox has its own cgroup namespace, so verify membership from
    // the host descriptor rather than trusting /proc output inside it.
    let pid = result.output["pid"].as_u64().unwrap().to_string();
    let members = std::fs::read_to_string(group.join("cgroup.procs")).unwrap();
    assert!(
        members.lines().any(|member| member == pid),
        "host PID {pid} not in {members}"
    );
    eprintln!("real cgroup cpu.weight={weight} parent={parent_weight}");
    manager.shutdown().await;
    std::fs::remove_dir(&group).unwrap();
}
