//! Linux Chrome instance boundary. No sessions, RPC service or agent tools.
pub mod installation;
pub mod process;
pub mod version;
use excalibur_cdp::{
    Client,
    protocol::{Attached, Method, Version},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    net::SocketAddrV4,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::MetadataExt, process::CommandExt},
    },
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast, watch};
use version::{Assessment, VersionPolicy, VersionState};
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid browser configuration: {0}")]
    Invalid(String),
    #[error("sandbox_unavailable: {0}")]
    Sandbox(String),
    #[error("persistent profile blocked: {0}")]
    VersionBlocked(String),
    #[error(transparent)]
    Cdp(#[from] excalibur_cdp::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    #[default]
    Ephemeral,
    Persistent,
}
#[derive(Clone, Debug)]
pub struct LaunchOptions {
    pub binary: PathBuf,
    pub profile: PathBuf,
    pub proxy: SocketAddrV4,
    pub class: Class,
    pub headless: bool,
    pub listening: bool,
    pub secret_service: bool,
    pub extra_args: Vec<String>,
    pub version_policy: VersionPolicy,
}
pub fn validate_args(args: &[String]) -> Result<(), Error> {
    for arg in args {
        if !matches!(
            arg.as_str(),
            "--disable-background-timer-throttling"
                | "--disable-renderer-backgrounding"
                | "--disable-backgrounding-occluded-windows"
        ) {
            return Err(Error::Invalid(format!(
                "unapproved Chrome flag: {}",
                arg.split('=').next().unwrap_or("")
            )));
        }
    }
    Ok(())
}
impl LaunchOptions {
    pub fn arguments(&self) -> Result<Vec<String>, Error> {
        validate_args(&self.extra_args)?;
        if self.proxy.ip().octets() != [127, 0, 0, 1] || self.proxy.port() == 0 {
            return Err(Error::Invalid(
                "proxy must be an explicit IPv4 loopback listener".into(),
            ));
        }
        if !self.binary.is_absolute() || !self.profile.is_absolute() {
            return Err(Error::Invalid(
                "absolute binary and profile paths required".into(),
            ));
        }
        let mut args: Vec<String> = ["--remote-debugging-pipe", "--no-first-run", "--no-default-browser-check", "--disable-default-apps", "--disable-sync", "--mute-audio", "--disable-extensions", "--disable-component-extensions-with-background-pages", "--disable-background-networking", "--disable-features=Translate,OptimizationHints,MediaRouter,AutofillServerCommunication", "--proxy-bypass-list=<-loopback>", "--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1", "--force-webrtc-ip-handling-policy=disable_non_proxied_udp", "--disable-quic", "--window-size=1280,900"].into_iter().map(str::to_owned).collect();
        if self.headless {
            args.push("--headless".into());
        }
        args.push(format!("--user-data-dir={}", self.profile.display()));
        args.push(format!("--proxy-server=http://{}", self.proxy));
        args.push(format!(
            "--disk-cache-size={}",
            if self.class == Class::Persistent {
                64 * 1024 * 1024
            } else {
                32 * 1024 * 1024
            }
        ));
        args.push(format!(
            "--password-store={}",
            if self.secret_service {
                "gnome-libsecret"
            } else {
                "basic"
            }
        ));
        if self.listening {
            if self.class != Class::Persistent {
                return Err(Error::Invalid(
                    "listeners require a persistent instance".into(),
                ));
            }
            args.extend(
                [
                    "--disable-background-timer-throttling",
                    "--disable-renderer-backgrounding",
                    "--disable-backgrounding-occluded-windows",
                ]
                .map(str::to_owned),
            );
        } else if !self.extra_args.is_empty() {
            return Err(Error::Invalid(
                "background flags require a listening instance".into(),
            ));
        }
        args.extend(self.extra_args.clone());
        args.push("about:blank".into());
        Ok(args)
    }
}
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Starting,
    Running,
    Degraded,
    Stopping,
    Stopped,
    Crashed,
    Blocked,
}
#[derive(Clone, Debug, Serialize)]
pub struct InstanceEvent {
    pub kind: String,
    pub detail: String,
}
struct Inner {
    transition: Mutex<()>,
    gate: Mutex<process::SandboxGate>,
    profile: PathBuf,
    killed: AtomicBool,
    child: Mutex<std::process::Child>,
    pids: Mutex<Vec<process::Process>>,
    status: watch::Sender<Status>,
    events: broadcast::Sender<InstanceEvent>,
    assessment: Mutex<Assessment>,
    policy: Mutex<VersionPolicy>,
    class: Class,
}
impl Inner {
    fn begin_stop(&self) {
        let _transition = self.transition.lock().unwrap();
        let current = self.status.borrow().clone();
        if current != Status::Stopped {
            self.status.send_replace(Status::Stopping);
        }
    }
    fn stop(&self, status: Status, kind: &str, detail: String) {
        let _transition = self.transition.lock().unwrap();
        let current = self.status.borrow().clone();
        if current == status
            || current == Status::Stopped
            || (matches!(
                current,
                Status::Blocked | Status::Crashed | Status::Stopping
            ) && status != Status::Stopped)
        {
            return;
        }
        self.kill();
        self.status.send_replace(status);
        let _ = self.events.send(InstanceEvent {
            kind: kind.into(),
            detail,
        });
    }
    fn kill(&self) {
        let mut child = self.child.lock().unwrap();
        if self.killed.swap(true, Ordering::SeqCst) {
            return;
        }
        // SAFETY: only this owned child's process group and verified start-time identities.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        for p in self.pids.lock().unwrap().iter().rev() {
            if process::tree(p.pid)
                .first()
                .is_some_and(|now| now.start_ticks == p.start_ticks)
            {
                unsafe {
                    libc::kill(p.pid as i32, libc::SIGKILL);
                }
            }
        }
        let _ = child.wait();
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.kill();
    }
}
pub struct Instance {
    inner: Arc<Inner>,
    client: Client,
    pub version: Version,
    pub warnings: Vec<String>,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
impl Instance {
    pub async fn launch(mut options: LaunchOptions) -> Result<Self, Error> {
        if unsafe { libc::geteuid() } == 0 || unsafe { libc::getuid() } == 0 {
            return Err(Error::Invalid("Chrome must never run as root".into()));
        }
        let status = fs::read_to_string("/proc/self/status")?;
        for field in ["CapInh:", "CapPrm:", "CapEff:", "CapAmb:"] {
            if status
                .lines()
                .find_map(|l| l.strip_prefix(field))
                .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
                != Some(0)
            {
                return Err(Error::Invalid(
                    "Chrome launcher must have no capabilities".into(),
                ));
            }
        }
        let args = options.arguments()?;
        if options.profile.join("DevToolsActivePort").exists() {
            return Err(Error::Invalid(
                "profile contains a TCP debugging endpoint file".into(),
            ));
        }
        let metadata = fs::symlink_metadata(&options.profile)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o7777 != 0o700
        {
            return Err(Error::Invalid(
                "profile must be a private owned directory (0700)".into(),
            ));
        }
        let mut warnings = Vec::new();
        if let Err(e) = options.version_policy.refresh().await {
            warnings.push(format!("stable metadata unavailable: {e}"));
        }
        let binary = options.binary.clone();
        let installed =
            tokio::task::spawn_blocking(move || installation::detect_chrome(Some(&binary)))
                .await
                .map_err(|e| Error::Invalid(e.to_string()))?
                .map_err(Error::Invalid)?;
        let installed_major = installed
            .version
            .split_whitespace()
            .find_map(|part| part.split('.').next().and_then(|v| v.parse::<u32>().ok()))
            .ok_or_else(|| Error::Invalid("invalid installed version".into()))?;
        let admission = options.version_policy.assess(installed_major, now());
        if options.class == Class::Persistent && admission.state == VersionState::BlockedPersistent
        {
            return Err(Error::VersionBlocked(admission.warning.unwrap_or_default()));
        }
        let (sender, chrome_in) = tokio::net::unix::pipe::pipe()?;
        let (chrome_out, receiver) = tokio::net::unix::pipe::pipe()?;
        let input = high_fd(chrome_in.into_blocking_fd()?)?;
        let output = high_fd(chrome_out.into_blocking_fd()?)?;
        let (input_fd, output_fd) = (input.as_raw_fd(), output.as_raw_fd());
        let parent = unsafe { libc::getpid() };
        let mut command = Command::new(&options.binary);
        command
            .args(args)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_clear()
            .env("HOME", &options.profile)
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8");
        if !options.headless {
            for key in ["DISPLAY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR"] {
                if let Some(value) = std::env::var_os(key) {
                    command.env(key, value);
                }
            }
        }
        if options.secret_service
            && let Some(value) = std::env::var_os("DBUS_SESSION_BUS_ADDRESS")
        {
            command.env("DBUS_SESSION_BUS_ADDRESS", value);
        }
        // SAFETY: async-signal-safe syscalls only, no allocation in the post-fork child.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
                    || libc::getppid() != parent
                {
                    return Err(std::io::Error::other("parent death guard unavailable"));
                }
                if libc::dup2(input_fd, 3) < 0 || libc::dup2(output_fd, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        drop(input);
        drop(output);
        let client = Client::connect(receiver, sender);
        if !options.secret_service {
            warnings.push(
                "password-store=basic: profile encryption does not protect against a local reader"
                    .into(),
            );
        }
        let (status, _) = watch::channel(Status::Starting);
        let (events, _) = broadcast::channel(256);
        let inner = Arc::new(Inner {
            transition: Mutex::new(()),
            gate: Mutex::new(process::SandboxGate::default()),
            profile: options.profile.clone(),
            killed: AtomicBool::new(false),
            child: Mutex::new(child),
            pids: Mutex::new(Vec::new()),
            status,
            events,
            assessment: Mutex::new(options.version_policy.assess(0, now())),
            policy: Mutex::new(options.version_policy.clone()),
            class: options.class,
        });
        let version: Version = client
            .execute(
                excalibur_cdp::protocol::Command {
                    method: Method::BrowserGetVersion,
                    params: excalibur_cdp::protocol::Empty::default(),
                },
                None,
            )
            .await
            .map_err(|e| Error::Sandbox(format!("startup failed before verification: {e}")))?;
        let major = version
            .product
            .split('/')
            .nth(1)
            .and_then(|v| v.split('.').next())
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| Error::Invalid("invalid Browser.getVersion product".into()))?;
        let assessment = inner.policy.lock().unwrap().assess(major, now());
        if options.class == Class::Persistent && assessment.state == VersionState::BlockedPersistent
        {
            return Err(Error::VersionBlocked(
                assessment.warning.unwrap_or_default(),
            ));
        }
        if let Some(w) = &assessment.warning {
            warnings.push(w.clone());
        }
        *inner.assessment.lock().unwrap() = assessment;
        wait_for_sandbox(&inner).await.map_err(Error::Sandbox)?;
        let mut event_rx = client.subscribe_all();
        client
            .send(
                Method::TargetSetAutoAttach,
                json!({"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true}),
                None,
            )
            .await?;
        inner.status.send_replace(
            if inner.assessment.lock().unwrap().state == VersionState::Current {
                Status::Running
            } else {
                Status::Degraded
            },
        );
        let weak = Arc::downgrade(&inner);
        let monitor_client = client.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(50));
            let mut refresh = tokio::time::Instant::now() + Duration::from_secs(86400);
            loop {
                tokio::select! {
                    _ = tick.tick() => {},
                    event = event_rx.recv() => match event {
                        Ok(event) if event.method == "Target.attachedToTarget" => {
                            let weak=weak.clone(); let client=monitor_client.clone();
                            tokio::spawn(async move {
                                let Some(inner)=weak.upgrade() else {return;};
                                if inner.killed.load(Ordering::SeqCst) {return;}
                                let attached=match serde_json::from_value::<Attached>(event.params) {Ok(v)=>v,Err(_)=>{inner.stop(Status::Crashed,"instance_crashed","invalid attached target event".into());return;}};
                                if let Err(e)=wait_for_sandbox(&inner).await {if !inner.killed.load(Ordering::SeqCst) {if e=="browser process unavailable" {inner.stop(Status::Crashed,"instance_crashed",e);}else{inner.stop(Status::Blocked,"sandbox_unavailable",e);}}return;}
                                if (attached.target_info.kind=="page" || attached.target_info.kind=="iframe") && client.send(Method::TargetSetAutoAttach,json!({"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true}),Some(&attached.session_id)).await.is_err() {return;}
                                if attached.waiting_for_debugger {let _=client.send(Method::RuntimeRunIfWaitingForDebugger,json!({}),Some(&attached.session_id)).await;}
                            });
                        },
                        Ok(event) if event.method == "Target.targetCrashed" || event.method == "Inspector.targetCrashed" || event.method == "session_crashed" => {
                            if let Some(inner) = weak.upgrade() { let _ = inner.events.send(InstanceEvent {kind:"target_crashed".into(),detail: event.session_id.unwrap_or_default()}); }
                        },
                        Ok(event) if event.method == "instance_crashed" => {
                            if let Some(inner)=weak.upgrade() {inner.stop(Status::Crashed,"instance_crashed","CDP EOF".into());} break;
                        },
                        Err(_) => { if let Some(inner) = weak.upgrade() { inner.stop(Status::Crashed,"instance_crashed","event stream lagged or closed".into()); } break; },
                        _ => {},
                    }
                }
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                if matches!(
                    *inner.status.borrow(),
                    Status::Stopped | Status::Blocked | Status::Crashed
                ) {
                    break;
                }
                if *inner.status.borrow() == Status::Stopping {
                    continue;
                }
                let exited = inner
                    .child
                    .lock()
                    .unwrap()
                    .try_wait()
                    .ok()
                    .flatten()
                    .is_some();
                if exited {
                    inner.stop(Status::Crashed, "instance_crashed", "browser exited".into());
                    break;
                }
                let tree =
                    process::tree_for_profile(inner.child.lock().unwrap().id(), &inner.profile);
                if tree.is_empty() {
                    inner.stop(
                        Status::Crashed,
                        "instance_crashed",
                        "browser process disappeared".into(),
                    );
                    break;
                }
                let checked = inner
                    .gate
                    .lock()
                    .unwrap()
                    .check(&tree, std::time::Instant::now());
                if let Err(error) = checked {
                    inner.stop(Status::Blocked, "sandbox_unavailable", error);
                    break;
                }
                *inner.pids.lock().unwrap() = tree;
                if tokio::time::Instant::now() >= refresh {
                    let inner = inner.clone();
                    let client = monitor_client.clone();
                    tokio::spawn(async move {
                        let _ = refresh_version(&inner, &client).await;
                    });
                    refresh = tokio::time::Instant::now() + Duration::from_secs(86400);
                }
            }
            monitor_client.shutdown();
        });
        Ok(Self {
            inner,
            client,
            version,
            warnings,
        })
    }
    pub fn client(&self) -> &Client {
        &self.client
    }
    pub fn pid(&self) -> u32 {
        self.inner.child.lock().unwrap().id()
    }
    pub async fn verify_sandbox(&self) -> Result<Vec<process::Process>, Error> {
        wait_for_sandbox(&self.inner).await.map_err(Error::Sandbox)
    }
    pub fn processes(&self) -> Vec<process::Process> {
        self.inner.pids.lock().unwrap().clone()
    }
    pub fn status(&self) -> watch::Receiver<Status> {
        self.inner.status.subscribe()
    }
    pub fn events(&self) -> broadcast::Receiver<InstanceEvent> {
        let receiver = self.inner.events.subscribe();
        let assessment = self.assessment();
        if matches!(
            assessment.state,
            VersionState::Warning | VersionState::BlockedPersistent
        ) && let Some(detail) = assessment.warning
        {
            let _ = self.inner.events.send(InstanceEvent {
                kind: "version_warning".into(),
                detail,
            });
        }
        receiver
    }
    pub fn assessment(&self) -> Assessment {
        self.inner.assessment.lock().unwrap().clone()
    }
    pub async fn refresh_version(&self) -> Result<Assessment, Error> {
        refresh_version(&self.inner, &self.client).await
    }
    pub async fn attach_page(&self, target: &str) -> Result<String, Error> {
        let result = self
            .client
            .send(
                Method::TargetAttachToTarget,
                json!({"targetId":target,"flatten":true}),
                None,
            )
            .await?;
        let sid = result["sessionId"]
            .as_str()
            .ok_or_else(|| Error::Invalid("missing sessionId".into()))?
            .to_owned();
        self.client
            .send(
                Method::TargetSetAutoAttach,
                json!({"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true}),
                Some(&sid),
            )
            .await?;
        Ok(sid)
    }
    pub async fn close(&self) -> Result<(), Error> {
        self.inner.begin_stop();
        let result = self
            .client
            .send_with_timeout(
                Method::BrowserClose,
                json!({}),
                None,
                Duration::from_secs(2),
            )
            .await;
        self.inner
            .stop(Status::Stopped, "instance_stopped", "closed by host".into());
        self.client.shutdown();
        match result {
            Ok(_) | Err(excalibur_cdp::Error::Closed(_)) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}
async fn refresh_version(inner: &Arc<Inner>, client: &Client) -> Result<Assessment, Error> {
    let mut policy = inner.policy.lock().unwrap().clone();
    if let Err(error) = policy.refresh().await {
        let _ = inner.events.send(InstanceEvent {
            kind: "version_warning".into(),
            detail: format!("metadata unavailable: {error}"),
        });
    }
    let value = client
        .send(Method::BrowserGetVersion, json!({}), None)
        .await?;
    let installed = value["product"]
        .as_str()
        .and_then(|s| s.split('/').nth(1))
        .and_then(|s| s.split('.').next())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::Invalid("invalid browser version".into()))?;
    let assessment = policy.assess(installed, now());
    let transition = inner.transition.lock().unwrap();
    let current = inner.status.borrow().clone();
    if inner.killed.load(Ordering::SeqCst) || matches!(current, Status::Stopping | Status::Stopped)
    {
        return Err(Error::Invalid("instance is no longer active".into()));
    }
    *inner.policy.lock().unwrap() = policy;
    *inner.assessment.lock().unwrap() = assessment.clone();
    if let Some(warning) = &assessment.warning {
        let _ = inner.events.send(InstanceEvent {
            kind: "version_warning".into(),
            detail: warning.clone(),
        });
    }
    if inner.class == Class::Persistent && assessment.state == VersionState::BlockedPersistent {
        drop(transition);
        inner.stop(
            Status::Blocked,
            "version_blocked",
            assessment.warning.clone().unwrap_or_default(),
        );
    } else {
        inner
            .status
            .send_replace(if assessment.state == VersionState::Current {
                Status::Running
            } else {
                Status::Degraded
            });
    }
    Ok(assessment)
}
fn high_fd(fd: OwnedFd) -> Result<OwnedFd, std::io::Error> {
    // SAFETY: duplicate a live owned descriptor above the reserved CDP descriptors.
    let duplicate = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if duplicate < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.inner.begin_stop();
        self.inner.stop(
            Status::Stopped,
            "instance_stopped",
            "instance handle dropped".into(),
        );
        self.client.shutdown();
    }
}

async fn wait_for_sandbox(inner: &Arc<Inner>) -> Result<Vec<process::Process>, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if inner.killed.load(Ordering::SeqCst) {
            return Err("instance closed".into());
        }
        let tree = process::tree_for_profile(inner.child.lock().unwrap().id(), &inner.profile);
        if tree.is_empty() {
            return Err("browser process unavailable".into());
        }
        let ready = inner
            .gate
            .lock()
            .unwrap()
            .check(&tree, std::time::Instant::now())?;
        let renderers = tree.iter().filter(|p| p.kind == "renderer").count();
        *inner.pids.lock().unwrap() = tree.clone();
        if ready && renderers > 0 {
            return Ok(tree);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("no verified renderer appeared".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
