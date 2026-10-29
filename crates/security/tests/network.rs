#![cfg(target_os = "linux")]
use excalibur_journal::Journal;
use excalibur_sandbox::{
    SandboxConfig,
    network::{NetworkPolicy, ProxyConfig, Resolver},
};
use excalibur_security::{
    Action, ApprovalScope, Autonomy, CapabilityToken, Decision, Network, Redactor, Root, Rule,
    RuleEffect, RunSecurity, SecuredTools, Security,
};
use excalibur_store::{Database, DatabaseKind, ReadLane, StorageConfig};
use excalibur_tools::{Access, LocalTools, ToolPolicy, terminal::prepare_project_boundary};
use excalibur_types::runtime::{ToolCall, ToolRuntime};
use serde_json::json;
use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};
struct Origin {
    port: u16,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Origin {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        let mut request = Vec::new();
                        let mut byte = [0];
                        while request.len() < 32768 {
                            if stream.read_exact(&mut byte).is_err() {
                                break;
                            }
                            request.push(byte[0]);
                            if request.ends_with(b"\r\n\r\n") {
                                counter.fetch_add(1, Ordering::SeqCst);
                                let _=stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nfixture");
                                break;
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            port,
            hits,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Origin {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
struct FixtureDns(Arc<AtomicUsize>);
impl Resolver for FixtureDns {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        assert!(host == "first.test" || host == "second.test");
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
    }
}
async fn approve(security: &Security, run: &str, action: Action, scope: ApprovalScope) {
    let id = match security
        .authorize(run, &action, excalibur_store::unix_seconds())
        .await
        .unwrap()
    {
        Decision::Ask { approval_id, .. } => approval_id,
        other => panic!("expected exact approval, got {other:?}"),
    };
    assert!(
        security
            .answer_approval(
                &id,
                true,
                scope,
                "authenticated-synthetic-response",
                excalibur_store::unix_seconds()
            )
            .await
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_tool_proxy_rechecks_taint_expiry_denial_and_run_control_before_dns_or_origin() {
    for scenario in 0..5 {
        let taint = scenario == 0;
        let expired = scenario == 2;
        let origin = Origin::new();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        assert!(
            std::process::Command::new("/usr/bin/mkfifo")
                .arg(root.join("release"))
                .status()
                .unwrap()
                .success()
        );
        prepare_project_boundary(&root).unwrap();
        let bridge = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("excalibur-network-bridge");
        assert!(
            bridge.is_file(),
            "cargo build -p excalibur-sandbox --bin excalibur-network-bridge must run before this test"
        );
        let dns_count = Arc::new(AtomicUsize::new(0));
        let mut proxy = ProxyConfig::new(NetworkPolicy::Domains(
            ["first.test".into(), "second.test".into()].into(),
        ));
        proxy.allowed_ports = [origin.port].into();
        proxy.permitted_private = [IpAddr::V4(Ipv4Addr::LOCALHOST)].into();
        proxy.resolver = Arc::new(FixtureDns(dns_count.clone()));
        let db = Database::open(
            temp.path().join("state/main.db"),
            DatabaseKind::Main,
            StorageConfig::default(),
        )
        .unwrap();
        let redactor = Redactor::default();
        let token = CapabilityToken {
            roots: vec![Root {
                path: root.clone(),
                write: true,
            }],
            network: Network::Domains(["first.test".into(), "second.test".into()].into()),
            processes: true,
            sandbox_profile: Some("project-net".into()),
            secrets: ["secret://fixture".into()].into(),
            budget: 1000,
            ..Default::default()
        };
        let mut base = token.clone();
        if expired {
            base.network = Network::Domains(["first.test".into()].into());
        }
        let security = Security::new(db.clone(), token.clone(), redactor.clone())
            .await
            .unwrap();
        let run = Journal::new(db.clone())
            .await
            .unwrap()
            .create_run(
                if taint { "taint-egress" } else { "deny-egress" },
                json!({}),
            )
            .await
            .unwrap();
        security
            .register_run(
                &run,
                RunSecurity {
                    parent: None,
                    project: "synthetic-project".into(),
                    workspace: root.clone(),
                    token: base,
                    autonomy: vec![Autonomy::Full],
                    full_acknowledged: true,
                    user_requested: true,
                },
                excalibur_store::unix_seconds(),
            )
            .await
            .unwrap();
        let at = excalibur_store::unix_seconds();
        let expiry = at + 1000;
        let clock = Arc::new(AtomicI64::new(at));
        if expired {
            let mut grant = token.clone();
            grant.network = Network::Domains(["second.test".into()].into());
            let approval = security
                .request_grant(&run, &grant, expiry, "synthetic temporary egress", at)
                .await
                .unwrap();
            assert!(
                security
                    .answer_approval(
                        &approval,
                        true,
                        ApprovalScope::Once,
                        "authenticated-grant-response",
                        at
                    )
                    .await
                    .unwrap()
            );
            security
                .grant(&run, grant, &approval, expiry, at)
                .await
                .unwrap();
            let current = clock.clone();
            proxy.authorizer = Some(
                security
                    .network_authorizer_with_clock(
                        run.clone(),
                        Arc::new(move || current.load(Ordering::SeqCst)),
                    )
                    .unwrap(),
            );
            security
                .add_rule(
                    Rule {
                        id: "second-confirmation".into(),
                        action: "net".into(),
                        pattern: "second.test".into(),
                        effect: RuleEffect::Ask,
                        run: Some(run.clone()),
                        project: None,
                        expires_at: None,
                    },
                    true,
                    at,
                )
                .await
                .unwrap();
            approve(
                &security,
                &run,
                Action::Network {
                    domain: "second.test".into(),
                },
                ApprovalScope::Task,
            )
            .await;
        }
        security
            .add_rule(
                Rule {
                    id: "initial-destination-confirmation".into(),
                    action: "net".into(),
                    pattern: "first.test".into(),
                    effect: RuleEffect::Ask,
                    run: Some(run.clone()),
                    project: None,
                    expires_at: None,
                },
                true,
                excalibur_store::unix_seconds(),
            )
            .await
            .unwrap();
        approve(
            &security,
            &run,
            Action::Network {
                domain: "first.test".into(),
            },
            if expired {
                ApprovalScope::Task
            } else {
                ApprovalScope::Once
            },
        )
        .await;
        let local = Arc::new(
            LocalTools::new(
                db.clone(),
                root.clone(),
                ToolPolicy {
                    read: Access::Allow,
                    write: Access::Allow,
                    exec: Access::Allow,
                    redactor,
                    sandbox: Some(SandboxConfig::project_net(&root, proxy, bridge)),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
        );
        let terminal = local.terminals();
        let tools = SecuredTools::new(local.clone(), security.clone());
        let command = format!(
            r#"python3 - <<'PY'
import urllib.request,urllib.error
assert urllib.request.urlopen('http://first.test:{port}/',timeout=5).read()==b'fixture'
open('first-complete','w').write('ready')
print('FIRST_COMPLETE',flush=True)
with open('release','rb') as barrier:
    assert barrier.read(1)==b'x'
try:
    urllib.request.urlopen('http://second.test:{port}/',timeout=5).read()
    raise AssertionError('second destination passed changed policy')
except urllib.error.HTTPError as error:
    assert error.code==403
print('SECOND_BLOCKED',flush=True)
PY"#,
            port = origin.port
        );
        approve(
            &security,
            &run,
            Action::Exec {
                command: command.clone(),
            },
            ApprovalScope::Once,
        )
        .await;
        let prepared = tools
            .prepare(
                &run,
                ToolCall {
                    id: "live-network-command".into(),
                    name: "exec".into(),
                    arguments: json!({"cmd":command,"yield_ms":250}).to_string(),
                },
            )
            .await
            .unwrap();
        let launched = tools.execute(&run, "live-network-effect", &prepared).await;
        assert!(launched.success, "{launched:?}");
        let session = launched.output["session_id"].as_str().unwrap();
        let first=terminal.io(&run,json!({"action":"wait_for","session_id":session,"pattern":"FIRST_COMPLETE","timeout_ms":10000})).await;
        assert_eq!(first.output["matched"], true, "{first:?}");
        assert!(root.join("first-complete").is_file());
        assert_eq!(origin.hits.load(Ordering::SeqCst), 1);
        assert_eq!(dns_count.load(Ordering::SeqCst), 1);
        let run_copy = run.clone();
        let previously_tainted = db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT tainted_secret FROM security_state WHERE run_id=?1",
                    [run_copy],
                    |r| r.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap();
        assert!(
            !previously_tainted,
            "the initial successful connection must precede the explicit secret fact"
        );
        if taint {
            security
                .record_secret_use(&run, "secret://fixture", excalibur_store::unix_seconds())
                .await
                .unwrap();
        } else if expired {
            clock.store(expiry + 1, Ordering::SeqCst);
        } else if scenario >= 3 {
            Journal::new(db.clone())
                .await
                .unwrap()
                .set_state(
                    &run,
                    if scenario == 3 { "paused" } else { "cancelled" },
                    Some("synthetic user control".into()),
                    "network-control",
                )
                .await
                .unwrap();
            if scenario == 4 {
                let run_copy = run.clone();
                db.write(excalibur_store::Priority::Journal, move |c| {
                    c.execute("UPDATE runs SET state='running' WHERE id=?1", [run_copy])?;
                    Ok(())
                })
                .await
                .unwrap();
            }
        } else {
            security
                .add_rule(
                    Rule {
                        id: "new-destination-denial".into(),
                        action: "net".into(),
                        pattern: "second.test".into(),
                        effect: RuleEffect::Deny,
                        run: Some(run.clone()),
                        project: None,
                        expires_at: None,
                    },
                    true,
                    excalibur_store::unix_seconds(),
                )
                .await
                .unwrap();
        }
        let release = root.join("release");
        tokio::task::spawn_blocking(move || std::fs::write(release, b"x"))
            .await
            .unwrap()
            .unwrap();
        let finished = terminal
            .io(
                &run,
                json!({"action":"wait_for","session_id":session,"timeout_ms":10000}),
            )
            .await;
        assert_eq!(finished.output["exit_code"], 0, "{finished:?}");
        assert!(
            finished.output["output"]
                .as_str()
                .unwrap()
                .contains("SECOND_BLOCKED"),
            "{finished:?}"
        );
        assert_eq!(
            origin.hits.load(Ordering::SeqCst),
            1,
            "policy change must stop before upstream"
        );
        assert_eq!(
            dns_count.load(Ordering::SeqCst),
            1,
            "policy change must stop before DNS"
        );
        assert_eq!(
            security
                .pending_approval(&run, excalibur_store::unix_seconds())
                .await
                .unwrap(),
            taint
        );
        let audit = serde_json::to_string(&security.audit_query(1000).await.unwrap()).unwrap();
        if taint {
            assert!(audit.contains("secret-tainted"));
        } else if expired {
            assert!(audit.contains("action exceeds execution capabilities"));
        } else if scenario >= 3 {
            assert!(audit.contains("network_connection_denied"));
        } else {
            assert!(audit.contains("deny rule matched"));
        }
        assert!(security.verify_audit().await.unwrap() > 0);
        terminal.shutdown().await;
        db.shutdown().await.unwrap();
    }
}
