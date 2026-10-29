//! Local process sessions. The embedding host authorizes commands and validates cwd.
//! Linux commands require an explicit kernel sandbox; unavailable profiles fail closed.
pub use excalibur_sandbox::{SandboxConfig, SandboxProfile, prepare_project_boundary};
use excalibur_types::runtime::{ToolDefinition, ToolResult};
use excalibur_vault::Redactor;
use portable_pty::{PtySize, native_pty_system};
use regex_automata::{
    Input,
    dfa::{Automaton, dense},
    util::primitives::StateID,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Notify;

const RESIDENT_BYTES: usize = 1024 * 1024;
const DEFAULT_OUTPUT: usize = 40 * 1024;
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
static LAST_USE: AtomicU64 = AtomicU64::new(1);
static SESSIONS: OnceLock<Mutex<Vec<Weak<Session>>>> = OnceLock::new();
static SPAWN_SERIAL: Mutex<()> = Mutex::new(());
fn session_registry() -> &'static Mutex<Vec<Weak<Session>>> {
    SESSIONS.get_or_init(|| Mutex::new(Vec::new()))
}

#[derive(Clone)]
pub struct TerminalConfig {
    pub spool_dir: PathBuf,
    pub shell: Option<PathBuf>,
    /// Host-provided values to redact. Values are never added to diagnostics.
    pub redacted_secrets: Vec<String>,
    pub max_per_run: usize,
    pub max_global: usize,
    pub protected_recent: usize,
    pub sandbox: Option<SandboxConfig>,
    pub redactor: Redactor,
}
impl std::fmt::Debug for TerminalConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalConfig")
            .field("spool_dir", &self.spool_dir)
            .field("shell", &self.shell)
            .field(
                "redacted_secrets",
                &format_args!("{} redacted values", self.redacted_secrets.len()),
            )
            .field("max_per_run", &self.max_per_run)
            .field("max_global", &self.max_global)
            .field("protected_recent", &self.protected_recent)
            .field("sandbox", &self.sandbox)
            .field("redactor", &"live redactor")
            .finish()
    }
}
impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            spool_dir: std::env::temp_dir().join("excalibur-terminal"),
            shell: None,
            redacted_secrets: Vec::new(),
            max_per_run: 16,
            max_global: 64,
            protected_recent: 8,
            sandbox: None,
            redactor: Redactor::default(),
        }
    }
}
#[derive(Clone)]
pub struct TerminalManager {
    inner: Arc<Inner>,
}
struct Inner {
    config: TerminalConfig,
    shell: PathBuf,
    environment: Value,
    state: Mutex<State>,
    stopped: AtomicBool,
    run_sandboxes: Mutex<BTreeMap<String, SandboxConfig>>,
}
#[derive(Default)]
struct State {
    sessions: BTreeMap<String, Arc<Session>>,
    notices: VecDeque<Value>,
}
struct Session {
    id: String,
    owner: String,
    command: String,
    pid: u32,
    last_used: AtomicU64,
    manager: Weak<Inner>,
    output: Mutex<Output>,
    input: Mutex<Option<Box<dyn Write + Send>>>,
    notify: Notify,
    termination_requested: AtomicBool,
    monitor: Mutex<Option<std::thread::JoinHandle<()>>>,
    #[cfg(windows)]
    job: WindowsJob,
    #[cfg(windows)]
    tty: bool,
}
struct Output {
    file: File,
    path: PathBuf,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: u64,
    exited: bool,
    exit_code: Option<i32>,
    timed_out: bool,
    error: Option<String>,
    persisted_blob: Option<String>,
}
impl Output {
    fn push(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(bytes)?;
        self.file.flush()?;
        let head_room = RESIDENT_BYTES / 2 - self.head.len();
        self.head
            .extend_from_slice(&bytes[..bytes.len().min(head_room)]);
        for b in bytes {
            if self.tail.len() == RESIDENT_BYTES / 2 {
                self.tail.pop_front();
            }
            self.tail.push_back(*b);
        }
        self.total += bytes.len() as u64;
        Ok(())
    }
    fn bounded(&mut self, from: u64, max: usize) -> std::io::Result<(String, bool)> {
        let available = self.total.saturating_sub(from);
        let mut bytes = Vec::with_capacity(max.min(available as usize));
        self.file.seek(SeekFrom::Start(from))?;
        if available <= max as u64 {
            Read::by_ref(&mut self.file)
                .take(available)
                .read_to_end(&mut bytes)?;
            return Ok((String::from_utf8_lossy(&bytes).into_owned(), false));
        }
        let half = max / 2;
        Read::by_ref(&mut self.file)
            .take(half as u64)
            .read_to_end(&mut bytes)?;
        bytes.extend_from_slice(b"\n[... output omitted; full output is retained ...]\n");
        self.file
            .seek(SeekFrom::Start(self.total - (max - half) as u64))?;
        Read::by_ref(&mut self.file)
            .take((max - half) as u64)
            .read_to_end(&mut bytes)?;
        Ok((String::from_utf8_lossy(&bytes).into_owned(), true))
    }
    fn live_tail(&self) -> String {
        let bytes = self.tail.iter().copied().collect::<Vec<_>>();
        let text = String::from_utf8_lossy(&bytes);
        let lines = text.lines().collect::<Vec<_>>();
        let text = lines[lines.len().saturating_sub(20)..].join("\n");
        let mut start = text.len().saturating_sub(2048);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text[start..].into()
    }
}
/// A bounded DFA consumes the complete spool incrementally, including matches
/// crossing reader chunks or the resident head/tail gap. It never loads the spool.
struct WaitPattern {
    dfa: dense::DFA<Vec<u32>>,
    state: StateID,
    cursor: u64,
}
impl WaitPattern {
    fn new(pattern: &str) -> Result<Self, String> {
        let dfa = dense::Builder::new()
            .configure(dense::Config::new().dfa_size_limit(Some(4 * 1024 * 1024)))
            .build(pattern)
            .map_err(|e| e.to_string())?;
        let state = dfa
            .start_state_forward(&Input::new(b""))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            dfa,
            state,
            cursor: 0,
        })
    }
    fn step(&mut self, file: &File, total: u64) -> Result<bool, String> {
        let mut buffer = [0u8; 65536];
        while self.cursor < total {
            let cap = (total - self.cursor).min(buffer.len() as u64) as usize;
            let n =
                read_file_at(file, &mut buffer[..cap], self.cursor).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            for byte in &buffer[..n] {
                self.state = self.dfa.next_state(self.state, *byte);
                self.cursor += 1;
                if self.dfa.is_match_state(self.state) {
                    return Ok(true);
                }
                if self.dfa.is_dead_state(self.state) {
                    self.cursor = total;
                    return Ok(false);
                }
                if self.dfa.is_quit_state(self.state) {
                    return Err("regex cannot search this byte sequence".into());
                }
            }
        }
        Ok(self.dfa.is_match_state(self.dfa.next_eoi_state(self.state)))
    }
}
impl TerminalManager {
    pub fn new(mut config: TerminalConfig) -> Result<Self, String> {
        if !config.spool_dir.is_absolute()
            || config.max_per_run == 0
            || config.max_global == 0
            || config.max_per_run > 16
            || config.max_global > 64
            || config.protected_recent > 8
        {
            return Err("invalid terminal configuration".into());
        }
        fs::create_dir_all(&config.spool_dir).map_err(|e| e.to_string())?;
        if let Some(sandbox) = config.sandbox.as_mut() {
            sandbox.workspace = sandbox
                .workspace
                .canonicalize()
                .map_err(|e| e.to_string())?;
            sandbox.hidden_paths.push(config.spool_dir.clone());
        }
        let mut environment = detect_environment();
        let shell = config
            .shell
            .clone()
            .or_else(|| {
                environment
                    .get("default_shell")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
            })
            .ok_or("no supported shell available")?;
        if !shell.is_file() {
            return Err("configured terminal shell is not a file".into());
        }
        environment["default_shell"] = json!(shell);
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                shell,
                environment,
                state: Mutex::new(State::default()),
                stopped: AtomicBool::new(false),
                run_sandboxes: Mutex::new(BTreeMap::new()),
            }),
        })
    }
    /// Trusted capability adapter narrows the base sandbox for a durable run.
    pub fn set_run_sandbox(&self, run: &str, sandbox: SandboxConfig) -> Result<(), String> {
        let base = self
            .inner
            .config
            .sandbox
            .as_ref()
            .ok_or("no host sandbox is configured")?;
        let restricted = sandbox.restricted_by(base)?;
        self.inner
            .run_sandboxes
            .lock()
            .unwrap()
            .insert(run.into(), restricted);
        Ok(())
    }
    pub fn spool_dir(&self) -> &Path {
        &self.inner.config.spool_dir
    }
    pub fn environment(&self) -> Value {
        self.inner.environment.clone()
    }
    pub fn definitions() -> Vec<ToolDefinition> {
        vec![ToolDefinition { name:"exec".into(), description:"Run an authorized local command in a pipe or PTY; retain a session if still running.".into(),
            input_schema:json!({"type":"object","required":["cmd"],"properties":{"cmd":{"type":"string","minLength":1},"cwd":{"type":"string"},"tty":{"type":"boolean"},"yield_ms":{"type":"integer","minimum":250,"maximum":30000},"timeout_ms":{"type":"integer","minimum":250,"maximum":86400000},"output_bytes":{"type":"integer","minimum":256,"maximum":1048576}},"additionalProperties":false}) },
            ToolDefinition { name:"exec_io".into(), description:"Read/write a retained process session, wait once for a regex or exit, signal or close it.".into(),
            input_schema:json!({"type":"object","required":["action"],"properties":{"action":{"enum":["list","read","write","keys","wait_for","signal","close"]},"session_id":{"type":"string"},"session":{"type":"string"},"cursor":{"type":"integer","minimum":0},"text":{"type":"string"},"keys":{"type":"array","items":{"type":"string"}},"pattern":{"type":"string"},"timeout_ms":{"type":"integer","minimum":0,"maximum":300000},"signal":{"enum":["interrupt","terminate","kill"]},"output_bytes":{"type":"integer","minimum":256,"maximum":1048576}},"additionalProperties":false}) }]
    }
    pub async fn exec(&self, run: &str, args: Value) -> ToolResult {
        let Some(cmd) = args
            .get("cmd")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty() && s.len() <= 65536)
        else {
            return ToolResult::error("cmd must be a nonempty string up to 65536 bytes");
        };
        let yield_ms = args
            .get("yield_ms")
            .and_then(Value::as_u64)
            .unwrap_or(10000);
        let timeout = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(120000);
        if !(250..=30000).contains(&yield_ms) || !(250..=86400000).contains(&timeout) {
            return ToolResult::error("yield_ms must be 250..30000 and timeout_ms 250..86400000");
        }
        if cfg!(not(target_os = "linux")) {
            return ToolResult::error(
                "mandatory native process sandbox is unavailable on this target",
            );
        }
        let sandbox = self
            .inner
            .run_sandboxes
            .lock()
            .unwrap()
            .get(run)
            .cloned()
            .or_else(|| self.inner.config.sandbox.clone());
        let Some(sandbox) = sandbox else {
            return ToolResult::error(
                "exec requires an explicit host sandbox; command was not executed",
            );
        };
        let cwd = match sandbox.workspace.canonicalize() {
            Ok(path) => path,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        if let Some(supplied) = args.get("cwd").and_then(Value::as_str)
            && Path::new(supplied).canonicalize().ok().as_ref() != Some(&cwd)
        {
            return ToolResult::error(
                "cwd is fixed by the host sandbox; a model cannot redirect its authority",
            );
        }
        let manager = self.clone();
        let run = run.to_string();
        let cmd = cmd.to_string();
        let tty = args.get("tty").and_then(Value::as_bool).unwrap_or(false);
        let session = match tokio::task::spawn_blocking(move || {
            manager.spawn(
                &run,
                &cmd,
                &cwd,
                tty,
                Duration::from_millis(timeout),
                sandbox,
            )
        })
        .await
        {
            Ok(Ok(session)) => session,
            Ok(Err(error)) => return ToolResult::error(error),
            Err(_) => return ToolResult::error("terminal spawn worker failed"),
        };
        let _ = self
            .wait(&session, None, Duration::from_millis(yield_ms))
            .await;
        let mut result = self.snapshot(&session, 0, output_limit(&args));
        if result.output["exited"] == true
            && (result.output["exit_code"] != 0
                || result.output["timed_out"] == true
                || !result.output["error"].is_null())
        {
            result.success = false;
        }
        result
    }
    pub async fn io(&self, run: &str, args: Value) -> ToolResult {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        if action == "list" {
            return ToolResult::ok(
                json!({"sessions":self.live_status(run),"notices":self.take_notices(run)}),
            );
        }
        let Some(id) = args
            .get("session_id")
            .or_else(|| args.get("session"))
            .and_then(Value::as_str)
        else {
            return ToolResult::error("session_id is required");
        };
        let session = self
            .inner
            .state
            .lock()
            .unwrap()
            .sessions
            .get(id)
            .filter(|s| s.owner == run)
            .cloned();
        let Some(session) = session else {
            return ToolResult::error(
                "session is absent or lost after restart; command is not repeated",
            );
        };
        session
            .last_used
            .store(LAST_USE.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
        let cursor = args.get("cursor").and_then(Value::as_u64).unwrap_or(0);
        match action {
            "read" => {}
            "write" | "keys" => {
                let bytes = if action == "write" {
                    let Some(text) = args.get("text").and_then(Value::as_str) else {
                        return ToolResult::error("write requires text");
                    };
                    if text.len() > 65536 {
                        return ToolResult::error("input exceeds 65536 bytes");
                    }
                    text.as_bytes().to_vec()
                } else {
                    let Some(keys) = args.get("keys").and_then(Value::as_array) else {
                        return ToolResult::error("keys requires an array of named keys");
                    };
                    let mut out = Vec::new();
                    for key in keys {
                        let Some(value) = key.as_str().and_then(key_bytes) else {
                            return ToolResult::error(
                                "unsupported key; use enter, ctrl-c, arrows, tab or esc",
                            );
                        };
                        out.extend_from_slice(value);
                    }
                    out
                };
                let session = session.clone();
                match tokio::task::spawn_blocking(move || {
                    let mut writer = session.input.lock().unwrap();
                    let Some(writer) = writer.as_mut() else {
                        return Err("session input is closed".to_string());
                    };
                    writer
                        .write_all(&bytes)
                        .and_then(|()| writer.flush())
                        .map_err(|e| e.to_string())
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => return ToolResult::error(e),
                    Err(_) => return ToolResult::error("terminal input worker failed"),
                }
            }
            "wait_for" => {
                let timeout = args
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(30000);
                if timeout > 300000 {
                    return ToolResult::error("wait timeout exceeds 300000 ms");
                }
                let pattern = match args.get("pattern").and_then(Value::as_str) {
                    Some(p) if p.len() <= 8192 => match WaitPattern::new(p) {
                        Ok(r) => Some(r),
                        Err(e) => return ToolResult::error(e.to_string()),
                    },
                    Some(_) => return ToolResult::error("wait pattern exceeds 8192 bytes"),
                    None => None,
                };
                let matched = match self
                    .wait(&session, pattern, Duration::from_millis(timeout))
                    .await
                {
                    Ok(value) => value,
                    Err(error) => return ToolResult::error(error),
                };
                let mut result = self.snapshot(&session, cursor, output_limit(&args));
                result.output["matched"] = json!(matched);
                result.output["wait_timed_out"] = json!(!matched);
                return result;
            }
            "signal" => {
                match args
                    .get("signal")
                    .and_then(Value::as_str)
                    .unwrap_or("interrupt")
                {
                    "interrupt" => session.signal(Signal::Interrupt),
                    "terminate" => self.close(&session).await,
                    "kill" => session.signal(Signal::Kill),
                    _ => return ToolResult::error("unsupported signal"),
                }
            }
            "close" => self.close(&session).await,
            _ => return ToolResult::error("unsupported exec_io action"),
        }
        self.snapshot(&session, cursor, output_limit(&args))
    }
    pub fn live_status(&self, run: &str) -> Vec<Value> {
        self.inner.state.lock().unwrap().sessions.values().filter(|s|s.owner==run).map(|s|{
            let output=s.output.lock().unwrap();
            json!({"session_id":s.id,"command":self.inner.config.redactor.redact_text(&redact_text(&s.command,&self.inner.config.redacted_secrets)),"tail":output.live_tail(),"exited":output.exited,"exit_code":output.exit_code,"cursor":output.total})
        }).collect()
    }
    pub fn take_notices(&self, run: &str) -> Vec<Value> {
        let mut state = self.inner.state.lock().unwrap();
        let mut own = Vec::new();
        let mut kept = VecDeque::new();
        for notice in state.notices.drain(..) {
            if notice["run"] == run {
                own.push(notice);
            } else {
                kept.push_back(notice);
            }
        }
        state.notices = kept;
        own
    }
    /// Host acknowledgement after durable blob publication. Retained sessions can
    /// still read their open spool handle; duplicate named spool files are unlinked.
    pub fn ack_output(&self, run: &str, id: &str, blob: &str) -> Result<(), String> {
        if blob.len() != 64 || !blob.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid output blob hash".into());
        }
        let session = session_registry()
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
            .find(|s| s.id == id);
        if let Some(session) = session {
            if session.owner != run {
                return Err("output belongs to another run".into());
            }
            let mut output = session.output.lock().unwrap();
            if !output.exited {
                return Err("cannot acknowledge live output".into());
            }
            output.persisted_blob = Some(blob.into());
            match fs::remove_file(&output.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        } else {
            // An evicted session may already have released its file handle while
            // the caller was publishing the final snapshot to the blob store.
            if !id.starts_with(&format!("term_{}_", std::process::id()))
                || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err("invalid local session ID".into());
            }
            let path = self.inner.config.spool_dir.join(format!("{id}.output"));
            match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }
    }
    /// Explicit maintenance for unclaimed crash/eviction diagnostics. Active
    /// session leases are never removed; retention must be at least one day.
    pub fn cleanup_released(&self, age_seconds: u64) -> Result<usize, String> {
        if age_seconds < 86400 {
            return Err("unclaimed output retention must be at least 86400 seconds".into());
        }
        let now = std::time::SystemTime::now();
        let mut removed = 0;
        for entry in fs::read_dir(&self.inner.config.spool_dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if !kind.is_file()
                || entry.path().extension().and_then(|e| e.to_str()) != Some("output")
            {
                continue;
            }
            let metadata = entry.metadata().map_err(|e| e.to_string())?;
            if now
                .duration_since(metadata.modified().map_err(|e| e.to_string())?)
                .unwrap_or_default()
                < Duration::from_secs(age_seconds)
            {
                continue;
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(entry.path())
                .map_err(|e| e.to_string())?;
            if fs2::FileExt::try_lock_exclusive(&file).is_err() {
                continue;
            }
            fs::remove_file(entry.path()).map_err(|e| e.to_string())?;
            removed += 1;
        }
        Ok(removed)
    }
    pub async fn shutdown(&self) {
        self.inner.stopped.store(true, Ordering::Release);
        // Wait for an in-flight spawn to publish its session before taking the
        // shutdown snapshot; no session can appear after shutdown has drained it.
        let manager = self.clone();
        let sessions = tokio::task::spawn_blocking(move || {
            let _serial = SPAWN_SERIAL.lock().unwrap();
            manager
                .inner
                .state
                .lock()
                .unwrap()
                .sessions
                .values()
                .cloned()
                .collect::<Vec<_>>()
        })
        .await;
        let sessions = match sessions {
            Ok(sessions) => sessions,
            Err(_) => self
                .inner
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .sessions
                .values()
                .cloned()
                .collect(),
        };
        self.terminate_sessions(&sessions).await;
        for session in &sessions {
            let output = session.output.lock().unwrap();
            if output.exited {
                // Ownership ends at shutdown, not when unrelated registry/read
                // snapshots eventually drop their transient Arc references.
                let _ = fs2::FileExt::unlock(&output.file);
            }
        }
        self.inner.state.lock().unwrap().sessions.clear();
    }
    /// The host stops run admissions before invoking this cancellation hook.
    /// Every owned live session receives the grace signal together, then KILL.
    pub async fn cancel_run(&self, run: &str) {
        let sessions = session_registry()
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|s| s.owner == run)
            .collect::<Vec<_>>();
        self.terminate_sessions(&sessions).await;
    }
    async fn close(&self, session: &Arc<Session>) {
        self.terminate_sessions(std::slice::from_ref(session)).await;
    }
    async fn terminate_sessions(&self, sessions: &[Arc<Session>]) {
        for session in sessions {
            session.termination_requested.store(true, Ordering::Release);
            session.signal(Signal::Terminate);
        }
        let grace = tokio::time::Instant::now() + Duration::from_secs(2);
        for session in sessions {
            let _ = self
                .wait(
                    session,
                    None,
                    grace.saturating_duration_since(tokio::time::Instant::now()),
                )
                .await;
        }
        for session in sessions {
            if !session.output.lock().unwrap().exited {
                session.signal(Signal::Kill);
            }
        }
        let reap = tokio::time::Instant::now() + Duration::from_secs(5);
        for session in sessions {
            let _ = self
                .wait(
                    session,
                    None,
                    reap.saturating_duration_since(tokio::time::Instant::now()),
                )
                .await;
        }
        // `exited` is published before thread-local guards are dropped. Join before
        // releasing manager ownership so shutdown also releases the spool's lease.
        self.join_exited_monitors(sessions).await;
    }
    async fn join_exited_monitors(&self, sessions: &[Arc<Session>]) {
        let handles = sessions
            .iter()
            .filter(|s| s.output.lock().unwrap().exited)
            .filter_map(|s| s.monitor.lock().unwrap().take())
            .collect::<Vec<_>>();
        let _ = tokio::task::spawn_blocking(move || {
            for handle in handles {
                let _ = handle.join();
            }
        })
        .await;
    }
    async fn wait(
        &self,
        session: &Arc<Session>,
        pattern: Option<WaitPattern>,
        timeout: Duration,
    ) -> Result<bool, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        let pattern = pattern.map(|p| Arc::new(Mutex::new(p)));
        loop {
            let notified = session.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (file, total, exited) = {
                let output = session.output.lock().unwrap();
                (
                    output.file.try_clone().map_err(|e| e.to_string())?,
                    output.total,
                    output.exited,
                )
            };
            if let Some(pattern) = &pattern {
                let pattern = pattern.clone();
                let found =
                    tokio::task::spawn_blocking(move || pattern.lock().unwrap().step(&file, total))
                        .await
                        .map_err(|_| "terminal regex worker failed".to_string())??;
                if found {
                    return Ok(true);
                }
            }
            if exited {
                return Ok(true);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Ok(false);
            }
        }
    }
    fn snapshot(&self, session: &Arc<Session>, from: u64, max: usize) -> ToolResult {
        let mut output = session.output.lock().unwrap();
        if from > output.total {
            return ToolResult::error("cursor exceeds current output length");
        }
        let (text, truncated) = match output.bounded(from, max) {
            Ok(v) => v,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        ToolResult {
            success: true,
            full_output_blob: output.persisted_blob.clone(),
            output: json!({"session_id":session.id,"pid":session.pid,"output":text,"cursor":output.total,"truncated":truncated,
            "exited":output.exited,"exit_code":output.exit_code,"timed_out":output.timed_out,"error":output.error,
            "spool_path":if output.persisted_blob.is_none(){Some(&output.path)}else{None}}),
        }
    }
    fn spawn(
        &self,
        run: &str,
        cmd: &str,
        cwd: &Path,
        tty: bool,
        timeout: Duration,
        sandbox: SandboxConfig,
    ) -> Result<Arc<Session>, String> {
        let _serial = SPAWN_SERIAL.lock().unwrap();
        if self.inner.stopped.load(Ordering::Acquire) {
            return Err("terminal manager is shut down".into());
        }
        self.make_room(run)?;
        make_global_room(run)?;
        let id = format!(
            "term_{}_{}",
            std::process::id(),
            NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
        );
        let path = self.inner.config.spool_dir.join(format!("{id}.output"));
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(&path).map_err(|e| e.to_string())?;
        fs2::FileExt::lock_exclusive(&file).map_err(|e| e.to_string())?;
        let (sender, receiver) = std::sync::mpsc::channel();
        let manager = self.clone();
        let cmd = cmd.to_string();
        let cwd = cwd.to_path_buf();
        let run = run.to_string();
        // The thread that spawns a Linux child remains alive until the child exits:
        // PR_SET_PDEATHSIG observes that thread, not only daemon process death.
        let monitor = std::thread::spawn(move || {
            let failed = sender.clone();
            let result = (|| -> Result<(), String> {
                let launched = launch(&manager.inner.shell, &cmd, &cwd, tty, &sandbox)?;
                let session = Arc::new(Session {
                    id: id.clone(),
                    owner: run.clone(),
                    manager: Arc::downgrade(&manager.inner),
                    command: cmd.clone(),
                    pid: launched.pid,
                    last_used: AtomicU64::new(LAST_USE.fetch_add(1, Ordering::Relaxed)),
                    output: Mutex::new(Output {
                        file,
                        path,
                        head: Vec::new(),
                        tail: VecDeque::new(),
                        total: 0,
                        exited: false,
                        exit_code: None,
                        timed_out: false,
                        error: None,
                        persisted_blob: None,
                    }),
                    input: Mutex::new(Some(launched.input)),
                    notify: Notify::new(),
                    termination_requested: AtomicBool::new(false),
                    monitor: Mutex::new(None),
                    #[cfg(windows)]
                    job: launched.job,
                    #[cfg(windows)]
                    tty,
                });
                manager
                    .inner
                    .state
                    .lock()
                    .unwrap()
                    .sessions
                    .insert(id, session.clone());
                session_registry()
                    .lock()
                    .unwrap()
                    .push(Arc::downgrade(&session));
                let mut readers = Vec::new();
                for mut reader in launched.readers {
                    let session = session.clone();
                    let secrets = manager.inner.config.redacted_secrets.clone();
                    let redactor = manager.inner.config.redactor.clone();
                    readers.push(std::thread::spawn(move || {
                        let mut sanitizer = StreamRedactor::new(&secrets);
                        let mut live = redactor.stream();
                        let mut buffer = [0u8; 8192];
                        loop {
                            match reader.read(&mut buffer) {
                                Ok(0) => break,
                                Ok(n) => {
                                    let clean = live.push(&sanitizer.push(&buffer[..n], false));
                                    let write_result = session.output.lock().unwrap().push(&clean);
                                    if let Err(e) = write_result {
                                        session.output.lock().unwrap().error = Some(e.to_string());
                                        session.signal(Signal::Kill);
                                        break;
                                    }
                                    session.notify.notify_waiters();
                                }
                                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                                #[cfg(unix)]
                                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                                Err(e) => {
                                    session.output.lock().unwrap().error = Some(e.to_string());
                                    break;
                                }
                            }
                        }
                        let mut clean = live.push(&sanitizer.push(&[], true));
                        clean.extend(live.finish());
                        let _ = session.output.lock().unwrap().push(&clean);
                        session.notify.notify_waiters();
                    }));
                }
                let observed = session.clone();
                let _kill_on_drop = KillOnDrop(session.clone());
                let _ = sender.send(Ok(session));
                drop(manager);
                {
                    let mut child = launched.child;
                    let started = Instant::now();
                    let mut timed_out = false;
                    let code = loop {
                        match child.try_wait() {
                            Ok(Some(code)) => break Some(code),
                            Ok(None) => {}
                            Err(e) => {
                                observed.output.lock().unwrap().error = Some(e.to_string());
                                break None;
                            }
                        }
                        if started.elapsed() >= timeout {
                            timed_out = true;
                            observed.signal(Signal::Terminate);
                            std::thread::sleep(Duration::from_secs(2));
                            observed.signal(Signal::Kill);
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    };
                    // A shell exiting must not leave descendants holding output pipes open.
                    observed.signal(Signal::Kill);
                    observed.input.lock().unwrap().take();
                    for reader in readers {
                        let _ = reader.join();
                    }
                    #[cfg(target_os = "linux")]
                    drop(launched.guardian);
                    #[cfg(target_os = "linux")]
                    drop(launched.sandbox);
                    let mut output = observed.output.lock().unwrap();
                    output.exited = true;
                    output.exit_code = code;
                    output.timed_out = timed_out;
                    let _ = output.file.sync_all();
                    drop(output);
                    observed.notify.notify_waiters();
                }

                Ok(())
            })();
            if let Err(error) = result {
                let _ = failed.send(Err(error));
            }
        });
        match receiver
            .recv()
            .map_err(|_| "terminal spawn thread ended without a result".to_string())?
        {
            Ok(session) => {
                *session.monitor.lock().unwrap() = Some(monitor);
                Ok(session)
            }
            Err(error) => {
                let _ = monitor.join();
                Err(error)
            }
        }
    }

    fn make_room(&self, run: &str) -> Result<(), String> {
        loop {
            let candidate = {
                let state = self.inner.state.lock().unwrap();
                let owner_count = state.sessions.values().filter(|s| s.owner == run).count();
                let owner_limit = owner_count >= self.inner.config.max_per_run;
                if !owner_limit && state.sessions.len() < self.inner.config.max_global {
                    return Ok(());
                }
                oldest_candidate(
                    state
                        .sessions
                        .values()
                        .filter(|s| !owner_limit || s.owner == run)
                        .cloned()
                        .collect(),
                    self.inner.config.protected_recent,
                )
            };
            if let Some(session) = candidate {
                evict(&session)?;
            } else {
                return Err("terminal capacity has no evictable session".into());
            }
        }
    }
}
fn oldest_candidate(mut all: Vec<Arc<Session>>, protected: usize) -> Option<Arc<Session>> {
    all.sort_by_key(|s| s.last_used.load(Ordering::Relaxed));
    let unprotected = all.len().saturating_sub(protected);
    let pool = if unprotected == 0 {
        &all[..]
    } else {
        &all[..unprotected]
    };
    pool.first().cloned()
}
fn make_global_room(run: &str) -> Result<(), String> {
    loop {
        let candidate = {
            let mut registry = session_registry().lock().unwrap();
            registry.retain(|entry| entry.strong_count() > 0);
            let all = registry
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            let owner_limit = all.iter().filter(|s| s.owner == run).count() >= 16;
            if !owner_limit && all.len() < 64 {
                return Ok(());
            }
            oldest_candidate(
                all.into_iter()
                    .filter(|s| !owner_limit || s.owner == run)
                    .collect(),
                8,
            )
        };
        if let Some(session) = candidate {
            evict(&session)?;
        } else {
            return Err("global terminal capacity is exhausted".into());
        }
    }
}
fn evict(session: &Arc<Session>) -> Result<(), String> {
    session.signal(Signal::Terminate);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !session.output.lock().unwrap().exited && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !session.output.lock().unwrap().exited {
        session.signal(Signal::Kill);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !session.output.lock().unwrap().exited && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !session.output.lock().unwrap().exited {
        return Err("evicted terminal session did not terminate".into());
    }
    if let Some(manager) = session.manager.upgrade() {
        let mut state = manager.state.lock().unwrap();
        state.sessions.remove(&session.id);
        state.notices.push_back(json!({"run":session.owner,"session_id":session.id,"command":manager.config.redactor.redact_text(&redact_text(&session.command,&manager.config.redacted_secrets)),"reason":"session_limit_eviction"}));
        while state.notices.len() > 256 {
            state.notices.pop_front();
        }
    }
    session_registry()
        .lock()
        .unwrap()
        .retain(|entry| entry.upgrade().is_some_and(|s| s.id != session.id));
    Ok(())
}
impl Drop for Inner {
    fn drop(&mut self) {
        for session in self.state.lock().unwrap().sessions.values() {
            session.signal(Signal::Kill);
        }
    }
}
#[derive(Clone, Copy)]
enum Signal {
    Interrupt,
    Terminate,
    Kill,
}
struct KillOnDrop(Arc<Session>);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.0.signal(Signal::Kill);
    }
}
impl Session {
    fn signal(&self, signal: Signal) {
        if self.output.lock().unwrap().exited {
            return;
        }
        #[cfg(unix)]
        unsafe {
            let sig = match signal {
                Signal::Interrupt => libc::SIGINT,
                Signal::Terminate => libc::SIGTERM,
                Signal::Kill => libc::SIGKILL,
            };
            libc::kill(-(self.pid as i32), sig);
        }
        #[cfg(windows)]
        {
            match signal {
                Signal::Kill => self.job.terminate(),
                Signal::Interrupt | Signal::Terminate => {
                    if self.tty {
                        if let Some(input) = self.input.lock().unwrap().as_mut() {
                            let _ = input.write_all(b"\x03");
                            let _ = input.flush();
                        }
                    } else {
                        unsafe {
                            use windows_sys::Win32::System::Console::{
                                CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent,
                            };
                            if GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.pid) == 0 {
                                self.output.lock().unwrap().error =
                                    Some(std::io::Error::last_os_error().to_string());
                            }
                        }
                    }
                }
            }
        }
    }
}
fn read_file_at(file: &File, bytes: &mut [u8], offset: u64) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_at(bytes, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_read(bytes, offset)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let mut file = file.try_clone()?;
        file.seek(SeekFrom::Start(offset))?;
        file.read(bytes)
    }
}
fn output_limit(args: &Value) -> usize {
    args.get("output_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_OUTPUT as u64)
        .clamp(256, RESIDENT_BYTES as u64) as usize
}
fn key_bytes(key: &str) -> Option<&'static [u8]> {
    Some(match key {
        "enter" => b"\r",
        "ctrl-c" => b"\x03",
        "ctrl-d" => b"\x04",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "tab" => b"\t",
        "esc" => b"\x1b",
        _ => return None,
    })
}
fn redact_text(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_string();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        text = text.replace(secret, "[REDACTED]");
    }
    text
}
struct StreamRedactor {
    pending: Vec<u8>,
    secrets: Vec<Vec<u8>>,
}
impl StreamRedactor {
    fn new(secrets: &[String]) -> Self {
        let secrets = secrets
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| s.as_bytes().to_vec())
            .collect::<Vec<_>>();
        Self {
            pending: Vec::new(),
            secrets,
        }
    }
    fn push(&mut self, bytes: &[u8], final_chunk: bool) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        let cutoff = if final_chunk {
            self.pending.len()
        } else {
            let withheld = self
                .secrets
                .iter()
                .flat_map(|secret| {
                    (1..secret.len()).filter(|length| self.pending.ends_with(&secret[..*length]))
                })
                .max()
                .unwrap_or(0);
            self.pending.len() - withheld
        };
        let mut out = Vec::new();
        let mut i = 0;
        while i < cutoff {
            if let Some(secret) = self
                .secrets
                .iter()
                .find(|s| self.pending[i..].starts_with(s))
            {
                out.extend_from_slice(b"[REDACTED]");
                i += secret.len();
            } else {
                out.push(self.pending[i]);
                i += 1;
            }
        }
        self.pending.drain(..i);
        out
    }
}
struct Launched {
    #[cfg(target_os = "linux")]
    sandbox: excalibur_sandbox::PreparedSandbox,
    pid: u32,
    input: Box<dyn Write + Send>,
    readers: Vec<Box<dyn Read + Send>>,
    child: Child,
    #[cfg(target_os = "linux")]
    guardian: LinuxGuardian,
    #[cfg(windows)]
    job: WindowsJob,
}
enum Child {
    Pipe(std::process::Child),
    #[cfg(windows)]
    Pty(Box<dyn portable_pty::Child + Send + Sync>),
}
impl Child {
    fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        match self {
            Self::Pipe(c) => c.try_wait().map(|s| s.map(|s| s.code().unwrap_or(-1))),
            #[cfg(windows)]
            Self::Pty(c) => c.try_wait().map(|s| s.map(|s| s.exit_code() as i32)),
        }
    }
}
fn shell_args(shell: &Path, cmd: &str) -> Vec<String> {
    let name = shell
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match name.as_str() {
        "powershell" | "pwsh" => vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            cmd.into(),
        ],
        "cmd" => vec!["/D".into(), "/S".into(), "/C".into(), cmd.into()],
        "bash" => vec![
            "--noprofile".into(),
            "--norc".into(),
            "-c".into(),
            cmd.into(),
        ],
        "zsh" => vec!["-f".into(), "-c".into(), cmd.into()],
        "fish" => vec!["--no-config".into(), "-c".into(), cmd.into()],
        _ => vec!["-c".into(), cmd.into()],
    }
}
fn safe_env() -> Vec<(String, String)> {
    [
        "PATH",
        "HOME",
        "USER",
        "TMPDIR",
        "TEMP",
        "TMP",
        "SystemRoot",
        "WINDIR",
        "PATHEXT",
        "COMSPEC",
        "LANG",
        "LC_ALL",
    ]
    .iter()
    .filter_map(|key| std::env::var(key).ok().map(|value| ((*key).into(), value)))
    .collect()
}
#[cfg(unix)]
fn launch(
    shell: &Path,
    cmd: &str,
    cwd: &Path,
    tty: bool,
    sandbox: &SandboxConfig,
) -> Result<Launched, String> {
    use std::os::unix::process::CommandExt;
    #[cfg(not(target_os = "linux"))]
    let _ = sandbox;
    #[cfg(target_os = "linux")]
    let prepared = excalibur_sandbox::PreparedSandbox::new(sandbox)?;
    #[cfg(target_os = "linux")]
    let mut command = Command::new(&prepared.program);
    #[cfg(not(target_os = "linux"))]
    let mut command = Command::new(shell);
    #[cfg(target_os = "linux")]
    prepared.configure(&mut command, shell, &shell_args(shell, cmd), tty);
    #[cfg(not(target_os = "linux"))]
    command.args(shell_args(shell, cmd));
    command
        .current_dir(cwd)
        .env_clear()
        .envs(safe_env())
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat");
    let mut pty = None;
    let (input, readers) = if tty {
        let pair = native_pty_system()
            .openpty(PtySize::default())
            .map_err(|e| e.to_string())?;
        let path = pair
            .master
            .tty_name()
            .ok_or("PTY does not expose slave name")?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        command
            .stdin(file.try_clone().map_err(|e| e.to_string())?)
            .stdout(file.try_clone().map_err(|e| e.to_string())?)
            .stderr(file);
        command.env("TERM", "xterm-256color");
        let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let input = pair.master.take_writer().map_err(|e| e.to_string())?;
        pty = Some(pair);
        (Some(input), vec![reader])
    } else {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0");
        (None, Vec::new())
    };
    #[cfg(target_os = "linux")]
    let mut guardian = LinuxGuardian::new().map_err(|e| e.to_string())?;
    #[cfg(target_os = "linux")]
    let (announce, acknowledge) = guardian.child_fds();
    #[cfg(target_os = "linux")]
    let filter_fd = prepared.filter_fd();
    let parent = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if tty && libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    libc::_exit(125);
                }
                if libc::fcntl(filter_fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                // User code starts only after a live guardian knows this process group.
                let bytes = libc::getpid().to_ne_bytes();
                let mut offset = 0;
                while offset < bytes.len() {
                    let n = libc::write(
                        announce,
                        bytes[offset..].as_ptr().cast(),
                        bytes.len() - offset,
                    );
                    if n > 0 {
                        offset += n as usize;
                    } else if *libc::__errno_location() != libc::EINTR {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                let mut byte = 0u8;
                loop {
                    let n = libc::read(acknowledge, (&mut byte as *mut u8).cast(), 1);
                    if n == 1 {
                        break;
                    }
                    if n == 0 {
                        return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
                    }
                    if *libc::__errno_location() != libc::EINTR {
                        return Err(std::io::Error::last_os_error());
                    }
                }
            }
            let _ = parent;
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    #[cfg(target_os = "linux")]
    guardian.complete_handshake();
    let pid = child.id();
    let (input, readers) = if let Some(input) = input {
        (input, readers)
    } else {
        (
            Box::new(child.stdin.take().unwrap()) as Box<dyn Write + Send>,
            vec![
                Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
                Box::new(child.stderr.take().unwrap()) as Box<dyn Read + Send>,
            ],
        )
    };
    drop(pty);
    Ok(Launched {
        #[cfg(target_os = "linux")]
        sandbox: prepared,
        pid,
        input,
        readers,
        child: Child::Pipe(child),
        #[cfg(target_os = "linux")]
        guardian,
    })
}
#[cfg(windows)]
fn launch(
    shell: &Path,
    cmd: &str,
    cwd: &Path,
    tty: bool,
    sandbox: &SandboxConfig,
) -> Result<Launched, String> {
    use std::os::windows::{io::AsRawHandle, process::CommandExt};
    let _ = sandbox;
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED};
    if tty {
        let pair = native_pty_system()
            .openpty(PtySize::default())
            .map_err(|e| e.to_string())?;
        // ConPTY does not expose CREATE_SUSPENDED. A trusted shell prelude waits
        // for one input line; no requested command runs until job assignment.
        let name = shell
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let gated = match name.as_str() {
            "pwsh" | "powershell" => format!("[void][Console]::ReadLine(); {cmd}"),
            "cmd" => format!("set /p _excalibur_gate= & {cmd}"),
            "bash" | "zsh" | "sh" => format!("IFS= read -r __excalibur_gate; {cmd}"),
            "fish" => format!("read __excalibur_gate; {cmd}"),
            _ => return Err("PTY job-assignment gate requires a supported shell".into()),
        };
        let mut command = portable_pty::CommandBuilder::new(shell);
        command.args(shell_args(shell, &gated));
        command.cwd(cwd);
        command.env_clear();
        for (k, v) in safe_env() {
            command.env(k, v);
        }
        command.env("PAGER", "cat");
        command.env("GIT_PAGER", "cat");
        command.env("TERM", "xterm-256color");
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| e.to_string())?;
        let pid = child.process_id().ok_or("PTY process has no ID")?;
        let raw = child.as_raw_handle().ok_or("PTY process has no handle")?;
        let job = match WindowsJob::attach(raw as _) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let mut input = pair.master.take_writer().map_err(|e| e.to_string())?;
        input
            .write_all(b"\r\n")
            .and_then(|()| input.flush())
            .map_err(|e| e.to_string())?;
        Ok(Launched {
            pid,
            input,
            readers: vec![reader],
            child: Child::Pty(child),
            job,
        })
    } else {
        let mut command = Command::new(shell);
        command
            .args(shell_args(shell, cmd))
            .current_dir(cwd)
            .env_clear()
            .envs(safe_env())
            .env("PAGER", "cat")
            .env("GIT_PAGER", "cat")
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP);
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let pid = child.id();
        let job = match WindowsJob::attach(child.as_raw_handle() as _) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        if let Err(error) = resume_primary_thread(pid) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(Launched {
            pid,
            input: Box::new(child.stdin.take().unwrap()),
            readers: vec![
                Box::new(child.stdout.take().unwrap()),
                Box::new(child.stderr.take().unwrap()),
            ],
            child: Child::Pipe(child),
            job,
        })
    }
}
#[cfg(not(any(unix, windows)))]
fn launch(
    _shell: &Path,
    _cmd: &str,
    _cwd: &Path,
    _tty: bool,
    _sandbox: &SandboxConfig,
) -> Result<Launched, String> {
    Err("local process adapter is unavailable on this target".into())
}

/// A separate minimal native guardian observes EOF if the daemon is killed. PDEATHSIG
/// protects the direct child; the guardian also terminates descendants in its group.
#[cfg(target_os = "linux")]
struct LinuxGuardian {
    pipe: File,
    acknowledgement: Option<File>,
    pid: i32,
}
#[cfg(target_os = "linux")]
impl LinuxGuardian {
    fn new() -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;
        let mut control = [0; 2];
        let mut ack = [0; 2];
        if unsafe { libc::pipe2(control.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::pipe2(ack.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
            unsafe {
                libc::close(control[0]);
                libc::close(control[1]);
            }
            return Err(std::io::Error::last_os_error());
        }
        let pid = unsafe { libc::fork() };
        if pid == -1 {
            unsafe {
                for fd in control.into_iter().chain(ack) {
                    libc::close(fd);
                }
            }
            return Err(std::io::Error::last_os_error());
        }
        if pid == 0 {
            unsafe {
                libc::close(control[1]);
                libc::close(ack[0]);
                let mut keep = [control[0] as u32, ack[1] as u32];
                if keep[0] > keep[1] {
                    keep.swap(0, 1);
                }
                let a = if keep[0] > 0 {
                    libc::syscall(libc::SYS_close_range, 0u32, keep[0] - 1, 0u32)
                } else {
                    0
                };
                let b = if keep[1] > keep[0] + 1 {
                    libc::syscall(libc::SYS_close_range, keep[0] + 1, keep[1] - 1, 0u32)
                } else {
                    0
                };
                let c = libc::syscall(libc::SYS_close_range, keep[1] + 1, u32::MAX, 0u32);
                if a < 0 || b < 0 || c < 0 {
                    let max = libc::sysconf(libc::_SC_OPEN_MAX).clamp(1024, 1048576) as i32;
                    for fd in 0..max {
                        if fd != control[0] && fd != ack[1] {
                            libc::close(fd);
                        }
                    }
                }
                let mut bytes = [0u8; 4];
                let mut offset = 0;
                while offset < 4 {
                    let n = libc::read(control[0], bytes[offset..].as_mut_ptr().cast(), 4 - offset);
                    if n > 0 {
                        offset += n as usize;
                    } else if n == 0 || *libc::__errno_location() != libc::EINTR {
                        libc::_exit(0);
                    }
                }
                let group = i32::from_ne_bytes(bytes);
                if group <= 0 {
                    libc::_exit(0);
                }
                let release = 1u8;
                let _ = libc::write(ack[1], (&release as *const u8).cast(), 1);
                libc::close(ack[1]);
                let mut byte = 0u8;
                loop {
                    let n = libc::read(control[0], (&mut byte as *mut u8).cast(), 1);
                    if n == 0 {
                        libc::kill(-group, libc::SIGKILL);
                        break;
                    }
                    if n > 0 {
                        break;
                    }
                    if *libc::__errno_location() != libc::EINTR {
                        break;
                    }
                }
                libc::_exit(0);
            }
        }
        unsafe {
            libc::close(control[0]);
            libc::close(ack[1]);
        }
        Ok(Self {
            pipe: unsafe { File::from_raw_fd(control[1]) },
            acknowledgement: Some(unsafe { File::from_raw_fd(ack[0]) }),
            pid,
        })
    }
    fn child_fds(&self) -> (i32, i32) {
        use std::os::fd::AsRawFd;
        (
            self.pipe.as_raw_fd(),
            self.acknowledgement.as_ref().unwrap().as_raw_fd(),
        )
    }
    fn complete_handshake(&mut self) {
        self.acknowledgement.take();
    }
}
#[cfg(target_os = "linux")]
impl Drop for LinuxGuardian {
    fn drop(&mut self) {
        let _ = self.pipe.write_all(&[0; 4]);
        unsafe {
            while libc::waitpid(self.pid, std::ptr::null_mut(), 0) < 0 {
                if *libc::__errno_location() != libc::EINTR {
                    break;
                }
            }
        }
    }
}

#[cfg(windows)]
struct WindowsJob(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
unsafe impl Send for WindowsJob {}
#[cfg(windows)]
unsafe impl Sync for WindowsJob {}
#[cfg(windows)]
impl WindowsJob {
    fn attach(process: windows_sys::Win32::Foundation::HANDLE) -> Result<Self, String> {
        use windows_sys::Win32::{Foundation::CloseHandle, System::JobObjects::*};
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
            ) == 0
                || AssignProcessToJobObject(job, process) == 0
            {
                CloseHandle(job);
                return Err(std::io::Error::last_os_error().to_string());
            }
            Ok(Self(job))
        }
    }
    fn terminate(&self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 130);
        }
    }
}
#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
#[cfg(windows)]
fn resume_primary_thread(pid: u32) -> Result<(), String> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::*,
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of_val(&entry) as u32;
        let mut found = false;
        if Thread32First(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32OwnerProcessID == pid {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if !thread.is_null() {
                        let resumed = ResumeThread(thread);
                        CloseHandle(thread);
                        found = resumed != u32::MAX;
                        break;
                    }
                }
                if Thread32Next(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        if found {
            Ok(())
        } else {
            Err("unable to resume primary process thread".into())
        }
    }
}
fn find_executable(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        for suffix in if cfg!(windows) {
            vec![".exe", ".cmd", ""]
        } else {
            vec![""]
        } {
            let path = dir.join(format!("{name}{suffix}"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if fs::metadata(&path).is_ok_and(|m| m.permissions().mode() & 0o111 == 0) {
                    continue;
                }
            }
            if path.is_file() {
                return fs::canonicalize(path).ok();
            }
        }
    }
    None
}
fn detect_environment() -> Value {
    let mut shells = BTreeMap::new();
    for name in ["bash", "zsh", "fish", "pwsh", "powershell", "cmd", "sh"] {
        if let Some(path) = find_executable(name) {
            shells.insert(name, path);
        }
    }
    let default = if cfg!(windows) {
        shells
            .get("pwsh")
            .or_else(|| shells.get("powershell"))
            .or_else(|| shells.get("cmd"))
    } else {
        shells
            .get("bash")
            .or_else(|| shells.get("zsh"))
            .or_else(|| shells.get("sh"))
    };
    let mut tools = BTreeMap::new();
    for name in [
        "cargo", "rustc", "git", "rg", "node", "pnpm", "npm", "python3", "pip", "uv", "apt", "dnf",
        "brew", "winget", "choco", "make", "cmake",
    ] {
        if let Some(path) = find_executable(name) {
            tools.insert(name, path);
        }
    }
    json!({"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"shells":shells,"default_shell":default,"tools":tools})
}
