//! Bounded JSON-RPC local process sessions. File mutations remain host proposals.
use crate::*;
use excalibur_vault::Redactor;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    path::{Component, Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Child,
    sync::{Notify, mpsc, oneshot},
};
pub type LspFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
#[derive(Clone)]
pub struct ServerConfig {
    pub root: PathBuf,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub initialization_options: Value,
    pub read_roots: Vec<PathBuf>,
    pub hidden_paths: Vec<PathBuf>,
    pub request_timeout_ms: u64,
    pub max_frame_bytes: usize,
}
impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("root", &self.root)
            .field("command", &self.command)
            .finish_non_exhaustive()
    }
}
impl ServerConfig {
    pub fn new(root: PathBuf, command: PathBuf, args: Vec<String>) -> Self {
        Self {
            root,
            command,
            args,
            environment: BTreeMap::new(),
            initialization_options: Value::Null,
            read_roots: vec![],
            hidden_paths: vec![],
            request_timeout_ms: 5000,
            max_frame_bytes: 16 * 1024 * 1024,
        }
    }
}
pub struct SpawnedLsp {
    pub child: Child,
    pub guard: Option<Arc<dyn Send + Sync>>,
}
pub trait LspSpawner: Send + Sync {
    fn spawn<'a>(&'a self, config: &'a ServerConfig) -> LspFuture<'a, Result<SpawnedLsp>>;
}
/// Concrete native adapter. Other hosts can supply an equally confined spawner.
#[derive(Clone, Default)]
pub struct ReadOnlySpawner {
    pub bubblewrap: Option<PathBuf>,
}
impl LspSpawner for ReadOnlySpawner {
    fn spawn<'a>(&'a self, config: &'a ServerConfig) -> LspFuture<'a, Result<SpawnedLsp>> {
        Box::pin(async move {
            #[cfg(target_os = "linux")]
            {
                let program = config.command.canonicalize()?;
                if !program.is_file() || !config.command.is_absolute() {
                    return Err(invalid(
                        "LSP executable must be an absolute host-configured file",
                    ));
                }
                if program.starts_with(&config.root) {
                    return Err(invalid(
                        "LSP executable must be provisioned outside the writable project",
                    ));
                }
                let mut sandbox = excalibur_sandbox::SandboxConfig::read_only(config.root.clone());
                sandbox.bubblewrap = self.bubblewrap.clone();
                sandbox.read_roots.extend(config.read_roots.clone());
                sandbox.hidden_paths.extend(config.hidden_paths.clone());
                let mut prepared =
                    excalibur_sandbox::PreparedSandbox::for_shell(&sandbox, &program)
                        .map_err(invalid)?;
                // The namespace clears its environment. Pass host-declared values only
                // after that clear, inside the confined child, never to bubblewrap itself.
                let mut environment = BTreeMap::from([
                    ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
                    ("LANG".to_owned(), "C.UTF-8".to_owned()),
                    ("HOME".to_owned(), "/tmp".to_owned()),
                    ("TMPDIR".to_owned(), "/tmp".to_owned()),
                ]);
                environment.extend(config.environment.clone());
                for (key, value) in environment {
                    prepared
                        .args
                        .extend(["--setenv".into(), key.into(), value.into()]);
                }
                let mut command = std::process::Command::new(&prepared.program);
                prepared.configure(&mut command, &program, &config.args, false);
                command
                    .current_dir(&config.root)
                    .env_clear()
                    .env("PATH", "/usr/bin:/bin")
                    .env("LANG", "C.UTF-8")
                    .env("HOME", "/tmp")
                    .env("TMPDIR", "/tmp")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                use std::os::unix::process::CommandExt;
                let filter_fd = prepared.filter_fd();
                let parent = unsafe { libc::getpid() };
                // Only async-signal-safe descriptor/process operations run after fork.
                unsafe {
                    command.pre_exec(move || {
                        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1
                            || libc::fcntl(filter_fd, libc::F_SETFD, 0) == -1
                        {
                            return Err(std::io::Error::last_os_error());
                        }
                        if libc::getppid() != parent {
                            libc::_exit(125);
                        }
                        Ok(())
                    });
                }
                let mut command = tokio::process::Command::from(command);
                command.kill_on_drop(true);
                let child = command.spawn()?;
                Ok(SpawnedLsp {
                    child,
                    guard: Some(Arc::new(prepared)),
                })
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = config;
                Err(invalid(
                    "no native read-only LSP spawner is configured on this host",
                ))
            }
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum LspOperation {
    Definition {
        path: PathBuf,
        position: Position,
    },
    References {
        path: PathBuf,
        position: Position,
        include_declaration: bool,
    },
    DocumentSymbols {
        path: PathBuf,
    },
    WorkspaceSymbols {
        query: String,
    },
    Diagnostics {
        path: PathBuf,
    },
    Rename {
        path: PathBuf,
        position: Position,
        new_name: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LspReply {
    pub result: Value,
    pub document_hashes: BTreeMap<String, String>,
    pub document_versions: BTreeMap<String, i32>,
}
/// Host-only synchronized source; deliberately has no Debug/Serialize implementation.
#[derive(Clone)]
pub struct DocumentSnapshot {
    pub uri: String,
    pub version: i32,
    pub hash: String,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DiagnosticSnapshot {
    pub uri: String,
    pub document_version: i32,
    pub published_version: Option<i32>,
    pub diagnostics: Vec<Value>,
    pub new_errors: Vec<Value>,
    pub fresh: bool,
    pub unavailable_reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerState {
    pub project: String,
    pub language: String,
    pub running: bool,
    pub pid: Option<u32>,
    pub documents: usize,
}
struct Slot {
    config: ServerConfig,
    session: tokio::sync::Mutex<Option<Arc<Session>>>,
    starts: Mutex<VecDeque<Instant>>,
}
struct ManagerInner {
    slots: tokio::sync::Mutex<BTreeMap<(String, String), Arc<Slot>>>,
    spawner: Arc<dyn LspSpawner>,
    redactor: Redactor,
    idle: Duration,
    reaper: AtomicBool,
    changed: Arc<Notify>,
}
#[derive(Clone)]
pub struct LspManager {
    inner: Arc<ManagerInner>,
}
impl std::fmt::Debug for LspManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspManager").finish_non_exhaustive()
    }
}
impl LspManager {
    pub fn new(spawner: Arc<dyn LspSpawner>) -> Self {
        Self::with_options(spawner, Redactor::default(), Duration::from_secs(600))
    }
    pub fn with_options(spawner: Arc<dyn LspSpawner>, redactor: Redactor, idle: Duration) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                slots: tokio::sync::Mutex::new(BTreeMap::new()),
                spawner,
                redactor,
                idle,
                reaper: AtomicBool::new(false),
                changed: Arc::new(Notify::new()),
            }),
        }
    }
    pub async fn register(
        &self,
        project: &str,
        language: &str,
        mut config: ServerConfig,
    ) -> Result<()> {
        if project.is_empty()
            || language.is_empty()
            || language.len() > 128
            || config.request_timeout_ms == 0
            || config.max_frame_bytes == 0
            || config.max_frame_bytes > 64 * 1024 * 1024
        {
            return Err(invalid("invalid project/language server configuration"));
        }
        config.root = config.root.canonicalize()?;
        if !config.root.is_dir() || config.root == Path::new("/") {
            return Err(invalid(
                "LSP project root must be a directory below the host root",
            ));
        }
        config.command = config.command.canonicalize()?;
        for (key, value) in &config.environment {
            let lower = key.to_ascii_lowercase();
            if ["token", "secret", "password", "credential", "cookie"]
                .iter()
                .any(|word| lower.contains(word))
                || self.inner.redactor.redact_text(value) != *value
            {
                return Err(invalid("LSP environment cannot carry secrets"));
            }
        }
        if config
            .args
            .iter()
            .any(|arg| self.inner.redactor.redact_text(arg) != *arg)
        {
            return Err(invalid("LSP arguments cannot carry secrets"));
        }
        if self
            .inner
            .redactor
            .redact_json(&config.initialization_options)
            != config.initialization_options
        {
            return Err(invalid("LSP options cannot carry secrets"));
        }
        let old = self.inner.slots.lock().await.insert(
            (project.into(), language.into()),
            Arc::new(Slot {
                config,
                session: tokio::sync::Mutex::new(None),
                starts: Mutex::new(VecDeque::new()),
            }),
        );
        if let Some(old) = old
            && let Some(session) = old.session.lock().await.take()
        {
            session.shutdown().await;
        }
        self.inner.changed.notify_one();
        Ok(())
    }
    pub async fn configured(&self, project: &str, language: &str) -> bool {
        self.inner
            .slots
            .lock()
            .await
            .contains_key(&(project.into(), language.into()))
    }
    async fn session(&self, project: &str, language: &str) -> Result<Lease> {
        self.ensure_reaper();
        let slot = self
            .inner
            .slots
            .lock()
            .await
            .get(&(project.into(), language.into()))
            .cloned()
            .ok_or_else(|| {
                invalid("no language server is configured for this project and language")
            })?;
        let mut active = slot.session.lock().await;
        if active
            .as_ref()
            .is_some_and(|s| s.state.closed.load(Ordering::SeqCst))
            && let Some(old) = active.take()
        {
            old.shutdown().await;
        }
        if active.is_none() {
            {
                let mut starts = slot.starts.lock().unwrap();
                let now = Instant::now();
                starts.retain(|at| now.saturating_duration_since(*at) < Duration::from_secs(60));
                if starts.len() >= 3 {
                    return Err(invalid(
                        "language server restart budget exhausted; host intervention or cooldown required",
                    ));
                }
                starts.push_back(now);
            }
            *active = Some(
                Session::start(
                    &slot.config,
                    language,
                    self.inner.spawner.as_ref(),
                    self.inner.redactor.clone(),
                )
                .await?,
            );
        }
        let session = active.as_ref().unwrap().clone();
        session.touch();
        session.in_flight.fetch_add(1, Ordering::SeqCst);
        self.inner.changed.notify_one();
        Ok(Lease {
            session,
            changed: self.inner.changed.clone(),
        })
    }
    pub async fn sync_document(
        &self,
        project: &str,
        language: &str,
        path: &Path,
        text: &str,
    ) -> Result<DiagnosticSnapshot> {
        let lease = self.session(project, language).await?;
        lease.session.sync_document(path, text).await
    }
    pub async fn request(
        &self,
        project: &str,
        language: &str,
        operation: LspOperation,
    ) -> Result<LspReply> {
        let lease = self.session(project, language).await?;
        let session = &lease.session;
        let before = session.documents.lock().await.clone();
        let rename = matches!(&operation, LspOperation::Rename { .. });
        let result = match operation {
            LspOperation::WorkspaceSymbols { query } => {
                session.supported("workspaceSymbolProvider")?;
                session
                    .rpc("workspace/symbol", json!({"query":query}))
                    .await?
            }
            LspOperation::Diagnostics { path } => serde_json::to_value(
                session
                    .diagnostics(&uri(&session.config, &path)?, None)
                    .await?,
            )?,
            LspOperation::Definition { path, position } => {
                session.supported("definitionProvider")?;
                session.rpc("textDocument/definition",json!({"textDocument":{"uri":uri(&session.config,&path)?},"position":position})).await?
            }
            LspOperation::References {
                path,
                position,
                include_declaration,
            } => {
                session.supported("referencesProvider")?;
                session.rpc("textDocument/references",json!({"textDocument":{"uri":uri(&session.config,&path)?},"position":position,"context":{"includeDeclaration":include_declaration}})).await?
            }
            LspOperation::DocumentSymbols { path } => {
                session.supported("documentSymbolProvider")?;
                session
                    .rpc(
                        "textDocument/documentSymbol",
                        json!({"textDocument":{"uri":uri(&session.config,&path)?}}),
                    )
                    .await?
            }
            LspOperation::Rename {
                path,
                position,
                new_name,
            } => {
                session.supported("renameProvider")?;
                if new_name.is_empty() || new_name.len() > 1024 {
                    return Err(invalid("rename requires a bounded nonempty identifier"));
                }
                session.rpc("textDocument/rename",json!({"textDocument":{"uri":uri(&session.config,&path)?},"position":position,"newName":new_name})).await?
            }
        };
        if rename {
            let after = session.documents.lock().await;
            if before.iter().any(|(uri, old)| {
                after.get(uri).is_none_or(|current| {
                    current.version != old.version || current.text != old.text
                })
            }) {
                return Err(invalid(
                    "documents changed while the rename proposal was computed",
                ));
            }
        }
        let hashes = before
            .iter()
            .map(|(uri, doc)| (uri.clone(), hash(doc.text.as_bytes())))
            .collect();
        let versions = before
            .iter()
            .map(|(uri, doc)| (uri.clone(), doc.version))
            .collect();
        Ok(LspReply {
            result: self.inner.redactor.redact_json(&result),
            document_hashes: hashes,
            document_versions: versions,
        })
    }
    pub async fn document_snapshot(
        &self,
        project: &str,
        language: &str,
        path: &Path,
    ) -> Result<DocumentSnapshot> {
        let lease = self.session(project, language).await?;
        let uri = uri(&lease.session.config, path)?;
        let documents = lease.session.documents.lock().await;
        let doc = documents
            .get(&uri)
            .ok_or_else(|| invalid("document has not been synchronized"))?;
        Ok(DocumentSnapshot {
            uri,
            version: doc.version,
            hash: hash(doc.text.as_bytes()),
            text: doc.text.clone(),
        })
    }
    /// Bounded, redacted protocol notices for trusted host diagnostics; never terminal logs.
    pub async fn server_messages(&self, project: &str, language: &str) -> Vec<Value> {
        let slot = self
            .inner
            .slots
            .lock()
            .await
            .get(&(project.into(), language.into()))
            .cloned();
        let Some(slot) = slot else { return vec![] };
        let session = slot.session.lock().await;
        session
            .as_ref()
            .map(|session| {
                session
                    .state
                    .messages
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|message| self.inner.redactor.redact_json(message))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub async fn project_root(&self, project: &str, language: &str) -> Result<PathBuf> {
        self.inner
            .slots
            .lock()
            .await
            .get(&(project.into(), language.into()))
            .map(|slot| slot.config.root.clone())
            .ok_or_else(|| invalid("language server is not configured"))
    }
    pub async fn close_document(&self, project: &str, language: &str, path: &Path) -> Result<()> {
        let lease = self.session(project, language).await?;
        let uri = uri(&lease.session.config, path)?;
        if lease.session.documents.lock().await.remove(&uri).is_some() {
            lease
                .session
                .notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}))
                .await?;
        }
        Ok(())
    }
    pub async fn states(&self) -> Vec<ServerState> {
        let slots = self
            .inner
            .slots
            .lock()
            .await
            .iter()
            .map(|((p, l), s)| (p.clone(), l.clone(), s.clone()))
            .collect::<Vec<_>>();
        let mut out = Vec::new();
        for (project, language, slot) in slots {
            let session = slot.session.lock().await;
            let (running, pid, documents) = if let Some(session) = session.as_ref() {
                (
                    !session.state.closed.load(Ordering::SeqCst),
                    session.pid,
                    session.documents.lock().await.len(),
                )
            } else {
                (false, None, 0)
            };
            out.push(ServerState {
                project,
                language,
                running,
                pid,
                documents,
            });
        }
        out
    }
    pub async fn reap_idle(&self, now: Instant) -> usize {
        reap(&self.inner, now, false).await
    }
    /// Memory pressure can release idle servers immediately, never a server
    /// with an in-flight request. Normal age-based reaping remains unchanged.
    pub async fn unload_idle(&self) -> usize {
        reap(&self.inner, Instant::now(), true).await
    }
    pub async fn shutdown_all(&self) {
        let slots = self
            .inner
            .slots
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for slot in slots {
            if let Some(session) = slot.session.lock().await.take() {
                session.shutdown().await;
            }
        }
    }
    fn ensure_reaper(&self) {
        if self.inner.reaper.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let changed = self.inner.changed.clone();
        tokio::spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else { break };
                reap(&inner, Instant::now(), false).await;
                let idle = inner
                    .idle
                    .min(Duration::from_secs(30))
                    .max(Duration::from_millis(1));
                drop(inner);
                tokio::select! {_=tokio::time::sleep(idle)=>{},_=changed.notified()=>{}}
            }
        });
    }
}
async fn reap(inner: &ManagerInner, now: Instant, force: bool) -> usize {
    let slots = inner
        .slots
        .lock()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let mut removed = 0;
    for slot in slots {
        let mut session = slot.session.lock().await;
        if session.as_ref().is_some_and(|session| {
            session.in_flight.load(Ordering::SeqCst) == 0
                && (force
                    || now.saturating_duration_since(*session.last_used.lock().unwrap())
                        >= inner.idle)
        }) {
            let old = session.take().unwrap();
            drop(session);
            old.shutdown().await;
            removed += 1;
        }
    }
    removed
}
struct Lease {
    session: Arc<Session>,
    changed: Arc<Notify>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.session.touch();
        self.session.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.changed.notify_one();
    }
}
#[derive(Clone)]
struct Document {
    version: i32,
    text: String,
}
#[derive(Clone, Default)]
struct Published {
    version: Option<i32>,
    items: Vec<Value>,
    result_id: Option<String>,
}
struct State {
    pending: Mutex<BTreeMap<u64, oneshot::Sender<Result<Value>>>>,
    diagnostics: Mutex<BTreeMap<String, Published>>,
    messages: Mutex<VecDeque<Value>>,
    changed: Notify,
    closed: AtomicBool,
    redactor: Redactor,
}
struct Session {
    config: ServerConfig,
    language: String,
    writer: mpsc::Sender<Value>,
    state: Arc<State>,
    next: AtomicU64,
    capabilities: Value,
    documents: tokio::sync::Mutex<BTreeMap<String, Document>>,
    child: tokio::sync::Mutex<Child>,
    pid: Option<u32>,
    _guard: Option<Arc<dyn Send + Sync>>,
    last_used: Mutex<Instant>,
    in_flight: AtomicUsize,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}
impl Session {
    async fn start(
        config: &ServerConfig,
        language: &str,
        spawner: &dyn LspSpawner,
        redactor: Redactor,
    ) -> Result<Arc<Self>> {
        let SpawnedLsp { mut child, guard } = spawner.spawn(config).await?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| invalid("LSP spawner must pipe stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| invalid("LSP spawner must pipe stdout"))?;
        let stderr = child.stderr.take();
        let pid = child.id();
        let (writer, mut messages) = mpsc::channel::<Value>(32);
        let state = Arc::new(State {
            pending: Mutex::new(BTreeMap::new()),
            diagnostics: Mutex::new(BTreeMap::new()),
            messages: Mutex::new(VecDeque::new()),
            changed: Notify::new(),
            closed: AtomicBool::new(false),
            redactor,
        });
        let state_writer = state.clone();
        let writer_task = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(message) = messages.recv().await {
                let Ok(bytes) = serde_json::to_vec(&message) else {
                    break;
                };
                let header = format!("Content-Length: {}\r\n\r\n", bytes.len());
                if stdin.write_all(header.as_bytes()).await.is_err()
                    || stdin.write_all(&bytes).await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
            close_state(&state_writer);
        });
        let reader_state = state.clone();
        let responses = writer.clone();
        let reader_config = config.clone();
        let reader = tokio::spawn(async move {
            let mut stdout = stdout;
            while let Ok(message) = frame(&mut stdout, reader_config.max_frame_bytes).await {
                dispatch(&reader_state, &responses, &reader_config, message).await;
            }
            close_state(&reader_state);
        });
        let mut tasks = vec![reader, writer_task];
        if let Some(mut stderr) = stderr {
            let stderr_state = state.clone();
            tasks.push(tokio::spawn(async move {
                let mut redactor = stderr_state.redactor.stream();
                let mut buffer = [0u8; 4096];
                loop {
                    match stderr.read(&mut buffer).await {
                        Ok(0) => break,
                        Ok(n) => {
                            let safe = redactor.push(&buffer[..n]);
                            if !safe.is_empty() {
                                record_stderr(&stderr_state, &safe);
                            }
                        }
                        Err(_) => break,
                    }
                }
                let safe = redactor.finish();
                if !safe.is_empty() {
                    record_stderr(&stderr_state, &safe);
                }
            }));
        }
        let mut session = Self {
            config: config.clone(),
            language: language.into(),
            writer,
            state,
            next: AtomicU64::new(1),
            capabilities: Value::Null,
            documents: tokio::sync::Mutex::new(BTreeMap::new()),
            child: tokio::sync::Mutex::new(child),
            pid,
            _guard: guard,
            last_used: Mutex::new(Instant::now()),
            in_flight: AtomicUsize::new(0),
            tasks: Mutex::new(tasks),
        };
        let root_uri = url::Url::from_directory_path(&config.root)
            .map_err(|_| invalid("invalid LSP project URI"))?
            .to_string();
        let response=session.rpc("initialize",json!({"processId":std::process::id(),"clientInfo":{"name":"Excalibur","version":env!("CARGO_PKG_VERSION")},"rootUri":root_uri,"workspaceFolders":[{"uri":root_uri,"name":config.root.file_name().and_then(|n|n.to_str()).unwrap_or("project")}],"capabilities":{"general":{"positionEncodings":["utf-16"]},"workspace":{"applyEdit":false,"configuration":true,"workspaceFolders":true,"symbol":{"dynamicRegistration":false}},"textDocument":{"synchronization":{"dynamicRegistration":false,"didSave":true},"publishDiagnostics":{"versionSupport":true},"diagnostic":{"dynamicRegistration":false,"relatedDocumentSupport":true},"definition":{"linkSupport":true},"documentSymbol":{"hierarchicalDocumentSymbolSupport":true}}},"initializationOptions":config.initialization_options})).await;
        match response {
            Ok(response) => {
                let capabilities = response["capabilities"].clone();
                if capabilities["positionEncoding"]
                    .as_str()
                    .is_some_and(|v| v != "utf-16")
                {
                    session.shutdown().await;
                    return Err(Error::Protocol(
                        "server selected an unadvertised position encoding",
                    ));
                }
                session.capabilities = capabilities;
                session.notify("initialized", json!({})).await?;
                Ok(Arc::new(session))
            }
            Err(error) => {
                session.shutdown().await;
                Err(error)
            }
        }
    }
    fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }
    fn supported(&self, capability: &str) -> Result<()> {
        match self.capabilities.get(capability) {
            Some(Value::Bool(true) | Value::Object(_)) => Ok(()),
            _ => Err(Error::UnsupportedOperation),
        }
    }
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        if self.state.closed.load(Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        self.enqueue(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }
    async fn enqueue(&self, message: Value) -> Result<()> {
        if serde_json::to_vec(&message)?.len() > self.config.max_frame_bytes {
            return Err(Error::Protocol("outgoing LSP frame exceeds limit"));
        }
        tokio::time::timeout(
            Duration::from_millis(self.config.request_timeout_ms),
            self.writer.send(message),
        )
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|_| Error::Closed)
    }
    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        if self.state.closed.load(Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.state.pending.lock().unwrap().insert(id, tx);
        let mut guard = RequestGuard {
            id,
            state: self.state.clone(),
            writer: self.writer.clone(),
            done: false,
        };
        self.enqueue(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        match tokio::time::timeout(Duration::from_millis(self.config.request_timeout_ms), rx).await
        {
            Ok(Ok(result)) => {
                guard.done = true;
                result
            }
            Ok(Err(_)) => Err(Error::Closed),
            Err(_) => Err(Error::Timeout),
        }
    }
    async fn sync_document(&self, path: &Path, text: &str) -> Result<DiagnosticSnapshot> {
        let uri = uri(&self.config, path)?;
        let baseline = self
            .state
            .diagnostics
            .lock()
            .unwrap()
            .get(&uri)
            .map(|d| d.items.clone())
            .unwrap_or_default();
        let mut docs = self.documents.lock().await;
        let known = docs.get(&uri).cloned();
        let sync = &self.capabilities["textDocumentSync"];
        let change = sync
            .as_i64()
            .or_else(|| sync["change"].as_i64())
            .unwrap_or(0);
        if change == 0 {
            return Err(Error::UnsupportedOperation);
        }
        if let Some(old) = known {
            if old.text != text {
                let version = old
                    .version
                    .checked_add(1)
                    .ok_or_else(|| invalid("LSP document version overflow"))?;
                let change = if change == 2 {
                    json!({"range":{"start":{"line":0,"character":0},"end":end_position(&old.text)},"text":text})
                } else {
                    json!({"text":text})
                };
                self.notify(
                    "textDocument/didChange",
                    json!({"textDocument":{"uri":uri,"version":version},"contentChanges":[change]}),
                )
                .await?;
                docs.insert(
                    uri.clone(),
                    Document {
                        version,
                        text: text.into(),
                    },
                );
            }
        } else {
            self.notify("textDocument/didOpen",json!({"textDocument":{"uri":uri,"languageId":self.language,"version":1,"text":text}})).await?;
            docs.insert(
                uri.clone(),
                Document {
                    version: 1,
                    text: text.into(),
                },
            );
        }
        drop(docs);
        self.diagnostics(&uri, Some(baseline)).await
    }
    async fn diagnostics(
        &self,
        uri: &str,
        baseline: Option<Vec<Value>>,
    ) -> Result<DiagnosticSnapshot> {
        let version = self
            .documents
            .lock()
            .await
            .get(uri)
            .map(|d| d.version)
            .ok_or_else(|| invalid("document must be synchronized before diagnostics"))?;
        if self
            .capabilities
            .get("diagnosticProvider")
            .is_some_and(|v| v.is_object() || v == true)
        {
            let prior = self
                .state
                .diagnostics
                .lock()
                .unwrap()
                .get(uri)
                .and_then(|d| d.result_id.clone());
            let result = self
                .rpc(
                    "textDocument/diagnostic",
                    json!({"textDocument":{"uri":uri},"previousResultId":prior}),
                )
                .await?;
            let items = if result["kind"] == "full" {
                result["items"]
                    .as_array()
                    .cloned()
                    .ok_or(Error::Protocol("invalid full diagnostic report"))?
            } else if result["kind"] == "unchanged" {
                self.state
                    .diagnostics
                    .lock()
                    .unwrap()
                    .get(uri)
                    .map(|d| d.items.clone())
                    .ok_or(Error::Protocol(
                        "unchanged diagnostics without earlier report",
                    ))?
            } else {
                return Err(Error::Protocol("invalid diagnostic report kind"));
            };
            self.state.diagnostics.lock().unwrap().insert(
                uri.into(),
                Published {
                    version: Some(version),
                    items,
                    result_id: result["resultId"].as_str().map(str::to_owned),
                },
            );
        } else {
            let wait = async {
                loop {
                    let notified = self.state.changed.notified();
                    let known = self.state.diagnostics.lock().unwrap().get(uri).cloned();
                    if known.as_ref().is_some_and(|d| d.version == Some(version))
                        || self.state.closed.load(Ordering::SeqCst)
                    {
                        break;
                    }
                    notified.await;
                }
            };
            let _ =
                tokio::time::timeout(Duration::from_millis(self.config.request_timeout_ms), wait)
                    .await;
        }
        let known = self
            .state
            .diagnostics
            .lock()
            .unwrap()
            .get(uri)
            .cloned()
            .unwrap_or_default();
        let fresh = known.version == Some(version);
        let diagnostics = known
            .items
            .into_iter()
            .map(|v| self.state.redactor.redact_json(&v))
            .collect::<Vec<_>>();
        let baseline = baseline
            .unwrap_or_default()
            .iter()
            .map(|value| diagnostic_key(&self.state.redactor.redact_json(value)))
            .collect::<std::collections::BTreeSet<_>>();
        let new_errors = if fresh {
            diagnostics
                .iter()
                .filter(|d| d["severity"] == 1 && !baseline.contains(&diagnostic_key(d)))
                .cloned()
                .collect()
        } else {
            vec![]
        };
        Ok(DiagnosticSnapshot {
            uri: uri.into(),
            document_version: version,
            published_version: known.version,
            diagnostics,
            new_errors,
            fresh,
            unavailable_reason: (!fresh).then(|| {
                "server supplied no verifiable diagnostics for this document version".into()
            }),
        })
    }
    async fn shutdown(&self) {
        if !self.state.closed.load(Ordering::SeqCst) {
            let _ = tokio::time::timeout(Duration::from_secs(1), self.rpc("shutdown", Value::Null))
                .await;
            let _ = self.notify("exit", Value::Null).await;
        }
        let mut child = self.child.lock().await;
        if tokio::time::timeout(Duration::from_secs(1), child.wait())
            .await
            .is_err()
        {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        close_state(&self.state);
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
        close_state(&self.state);
    }
}
struct RequestGuard {
    id: u64,
    state: Arc<State>,
    writer: mpsc::Sender<Value>,
    done: bool,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.state.pending.lock().unwrap().remove(&self.id);
        if !self.done {
            let _ = self.writer.try_send(
                json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":self.id}}),
            );
        }
    }
}
fn record_stderr(state: &State, bytes: &[u8]) {
    let message = String::from_utf8_lossy(bytes)
        .chars()
        .take(4096)
        .collect::<String>();
    let notice = json!({"method":"stderr","params":{"message":message}});
    let mut messages = state.messages.lock().unwrap();
    if messages.len() == 32 {
        messages.pop_front();
    }
    messages.push_back(notice);
}
fn close_state(state: &State) {
    state.closed.store(true, Ordering::SeqCst);
    let pending = std::mem::take(&mut *state.pending.lock().unwrap());
    for (_, reply) in pending {
        let _ = reply.send(Err(Error::Closed));
    }
    state.changed.notify_waiters();
}
async fn dispatch(
    state: &State,
    writer: &mpsc::Sender<Value>,
    config: &ServerConfig,
    message: Value,
) {
    if message["jsonrpc"] != "2.0" {
        close_state(state);
        return;
    }
    if let Some(method) = message["method"].as_str() {
        if ["window/logMessage", "window/showMessage", "$/progress"].contains(&method) {
            let mut params = message["params"].clone();
            if let Some(text) = params["message"].as_str() {
                params["message"] = Value::String(text.chars().take(4096).collect());
            }
            let notice = if params.to_string().len() > 8192 {
                json!({"method":method,"truncated":true})
            } else {
                state
                    .redactor
                    .redact_json(&json!({"method":method,"params":params}))
            };
            let mut messages = state.messages.lock().unwrap();
            if messages.len() == 32 {
                messages.pop_front();
            }
            messages.push_back(notice);
        }

        if let Some(id) = message.get("id") {
            let response = match method {
                "workspace/applyEdit" => {
                    json!({"jsonrpc":"2.0","id":id,"result":{"applied":false,"failureReason":"Edits require the host authorization and durable editing pipeline"}})
                }
                "workspace/configuration" => {
                    let items = message["params"]["items"]
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .map(|_| config.initialization_options.clone())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    json!({"jsonrpc":"2.0","id":id,"result":items})
                }
                "workspace/workspaceFolders" => {
                    json!({"jsonrpc":"2.0","id":id,"result":[{"uri":url::Url::from_directory_path(&config.root).ok().map(|u|u.to_string()),"name":config.root.file_name().and_then(|v|v.to_str()).unwrap_or("project")}]})
                }
                "window/workDoneProgress/create" | "window/showMessageRequest" => {
                    json!({"jsonrpc":"2.0","id":id,"result":null})
                }
                _ => {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Client method is not supported; no host effects are authorized"}})
                }
            };
            let _ = writer.send(response).await;
        } else if method == "textDocument/publishDiagnostics"
            && let Some(uri) = message["params"]["uri"].as_str()
        {
            let version = message["params"]["version"]
                .as_i64()
                .and_then(|n| i32::try_from(n).ok());
            let items = message["params"]["diagnostics"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let mut diagnostics = state.diagnostics.lock().unwrap();
            if diagnostics
                .get(uri)
                .is_none_or(|old| old.version.zip(version).is_none_or(|(old, new)| new >= old))
            {
                diagnostics.insert(
                    uri.into(),
                    Published {
                        version,
                        items,
                        result_id: None,
                    },
                );
            }
            drop(diagnostics);
            state.changed.notify_waiters();
        }
    } else if let Some(id) = message["id"].as_u64()
        && let Some(reply) = state.pending.lock().unwrap().remove(&id)
    {
        let result = if message.get("error").is_some() && message.get("result").is_some() {
            Err(Error::Protocol("response contains both result and error"))
        } else if message.get("error").is_some() {
            let code = message["error"]["code"].as_i64().unwrap_or(-32603);
            let text = message["error"]["message"]
                .as_str()
                .unwrap_or("request failed");
            let text = state
                .redactor
                .redact_text(&text.chars().take(2048).collect::<String>());
            Err(invalid(format!("language server error {code}: {text}")))
        } else if let Some(result) = message.get("result") {
            Ok(result.clone())
        } else {
            Err(Error::Protocol("response has neither result nor error"))
        };
        let _ = reply.send(result);
    }
}
async fn frame(reader: &mut (impl AsyncRead + Unpin), max: usize) -> Result<Value> {
    let mut header = Vec::new();
    loop {
        if header.len() >= 8192 {
            return Err(Error::Protocol("LSP header exceeds limit"));
        }
        header.push(reader.read_u8().await?);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header =
        std::str::from_utf8(&header).map_err(|_| Error::Protocol("invalid LSP header encoding"))?;
    let mut length = None;
    for line in header.split("\r\n").filter(|line| !line.is_empty()) {
        let (key, value) = line
            .split_once(':')
            .ok_or(Error::Protocol("invalid LSP header"))?;
        if key.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err(Error::Protocol("duplicate Content-Length"));
            }
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| Error::Protocol("invalid Content-Length"))?,
            );
        } else if key.eq_ignore_ascii_case("content-type")
            && !value.to_ascii_lowercase().contains("utf-8")
            && !value.to_ascii_lowercase().contains("utf8")
        {
            return Err(Error::Protocol("unsupported LSP content encoding"));
        }
    }
    let length = length.ok_or(Error::Protocol("missing Content-Length"))?;
    if length == 0 || length > max {
        return Err(Error::Protocol("LSP frame exceeds limit"));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}
fn end_position(text: &str) -> Position {
    let line = text.bytes().filter(|&b| b == b'\n').count();
    let tail = text.rsplit('\n').next().unwrap_or("");
    Position {
        line: line as u32,
        character: tail.encode_utf16().count() as u32,
    }
}
fn uri(config: &ServerConfig, path: &Path) -> Result<String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        if path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(invalid("LSP path escapes its project"));
        }
        config.root.join(path)
    };
    let path = path.canonicalize()?;
    let allowed = std::iter::once(&config.root)
        .chain(config.read_roots.iter())
        .any(|root| root.canonicalize().is_ok_and(|root| path.starts_with(root)));
    if !allowed {
        return Err(invalid("LSP path is outside configured read authority"));
    }
    url::Url::from_file_path(path)
        .map(|u| u.to_string())
        .map_err(|_| invalid("invalid document URI"))
}
fn diagnostic_key(value: &Value) -> String {
    json!({"range":value["range"],"severity":value["severity"],"code":value["code"],"source":value["source"],"message":value["message"]}).to_string()
}
