//! Harness integration for the browser and search capabilities (arch 16).
//!
//! This module provides the `ToolRuntime` adapter, the tool catalog, the
//! capability manifests and the reconciliation, cancellation and eval-profile
//! behavior behind explicit host configuration. Nothing here registers itself;
//! the host composition opts in through `wrap_from_env`, which returns the
//! inner runtime unchanged unless the harness is explicitly enabled.

use excalibur_tools::deferred::{
    CancellationClass, CatalogTool, DryRunBehavior, ToolEffect, ToolRisk,
};
use excalibur_types::runtime::{
    BoxFuture, Effect, PreparedTool, Reconcile, Resource, ToolCall, ToolDefinition, ToolResult,
    ToolRuntime, ToolScheduling,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use crate::BrowserBackend;

/// Environment variable enabling the harness registration.
pub const HARNESS_ENV: &str = "EXCALIBUR_WEB_HARNESS";
/// Environment variable carrying the webd socket path.
pub const HARNESS_SOCKET_ENV: &str = "EXCALIBUR_WEB_SOCKET";

/// Host configuration for the browser and search harness.
#[derive(Clone, Debug)]
pub struct HarnessConfig {
    /// webd socket path used by the adapter backend.
    pub socket: PathBuf,
    /// Eval profile: persistent profiles denied, benchmark sources blocked,
    /// human blocks fail immediately, ephemeral contexts only.
    pub eval: bool,
}

/// Build a socket backend from explicit host configuration.
///
/// Returns `None` unless the harness is explicitly enabled, so the default
/// host composition never changes behavior.
pub fn backend_from_env() -> Option<Arc<dyn BrowserBackend>> {
    let enabled = matches!(
        std::env::var(HARNESS_ENV).as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    );
    if !enabled {
        return None;
    }
    let socket = std::env::var(HARNESS_SOCKET_ENV)
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(Arc::new(crate::SocketBackend {
        socket: PathBuf::from(socket),
    }) as Arc<dyn BrowserBackend>)
}

/// True when the host explicitly enabled the web harness registration.
pub fn registration_enabled() -> bool {
    backend_from_env().is_some()
}

/// Browser capability manifest (arch 16.1).
pub fn browser_manifest() -> Value {
    json!({
        "capability": "browser",
        "version": "0.1.0",
        "implementation": "excalibur-webd",
        "engine": {"name": "chrome", "version": "stable", "channel": "stable", "headless": "new"},
        "platforms": ["linux-x86_64"],
        "profiles": ["full"],
        "operations": ["browser_open_session", "browser_close_session", "browser_suspend_session",
            "browser_tabs", "browser_navigate", "browser_wait_ready", "browser_snapshot",
            "browser_act", "browser_download", "browser_devtools", "browser_emulate", "browser_status"],
        "unsupported": [{"operation": "browser_routine", "reason": "planned extension A-01"}],
        "resources": {"ram_fixed_mib_per_instance": 260, "ram_typical_mib_per_tab": 200,
            "max_sessions": 6, "max_tabs_per_run": 4, "max_tabs_total": 12, "max_ephemeral_contexts": 3},
        "concurrency": {"ops_per_tab": 1, "queue_timeout_ms": 30000},
        "states": ["working", "input_required", "completed", "failed", "cancelled"],
        "blocked_kinds": ["blocked_captcha", "needs_login", "needs_2fa", "blocked_by_site",
            "human_control", "sandbox_unavailable", "version_blocked"]
    })
}

/// Search capability manifest (arch 16.1).
pub fn search_manifest() -> Value {
    json!({
        "capability": "search",
        "version": "0.1.0",
        "implementation": "excalibur-webd",
        "profiles": ["full"],
        "operations": ["web_search", "web_read"],
        "limits": {"k_max": 50, "read_max_bytes": 10485760},
        "retention": {"coverage": "index build date reported per query"},
        "states": ["working", "completed", "failed", "cancelled"]
    })
}

struct Entry {
    name: &'static str,
    description: &'static str,
    tags: &'static [&'static str],
    effect: ToolEffect,
    idempotency: Effect,
    risk: ToolRisk,
    cancel: CancellationClass,
    timeout_ms: u64,
    budget: usize,
    dry: DryRunBehavior,
    fast: bool,
    schema: Value,
}

fn entry(e: &Entry) -> CatalogTool {
    CatalogTool {
        name: e.name.into(),
        description: e.description.into(),
        input_schema: e.schema.clone(),
        output_schema: json!({"type": "object"}),
        tags: e.tags.iter().map(|s| s.to_string()).collect(),
        effect: e.effect,
        idempotency: e.idempotency,
        resources_description: "Resolved per call to the owning browser session and tab.".into(),
        risk: e.risk,
        timeout_ms: e.timeout_ms,
        output_budget_bytes: e.budget,
        platforms: vec!["linux-x86_64".into()],
        inverse: None,
        probe: None,
        dry_run: e.dry.clone(),
        fast: e.fast,
        cancel: e.cancel,
        embedding: None,
    }
}

fn obj(props: Value) -> Value {
    json!({"type": "object", "properties": props, "additionalProperties": false})
}

/// Deferred browser tools (arch 16.2): registered outside the eager prefix.
pub fn browser_catalog() -> Vec<CatalogTool> {
    let entries = [
        Entry {
            name: "browser_open_session",
            description: "Open a browser session with an ephemeral or persistent profile and an optional login site.",
            tags: &["browser", "session", "open", "profile"],
            effect: ToolEffect::WriteLocal,
            idempotency: Effect::Keyed,
            risk: ToolRisk::Medium,
            cancel: CancellationClass::Graceful,
            timeout_ms: 45_000,
            budget: 16_384,
            dry: DryRunBehavior::Adapter,
            fast: false,
            schema: obj(
                json!({"profile": {"type": "string"}, "mode": {"type": "string"}, "site": {"type": "string"}, "label": {"type": "string"}}),
            ),
        },
        Entry {
            name: "browser_close_session",
            description: "Close a browser session and release its tabs, leases and ephemeral contexts.",
            tags: &["browser", "session", "close"],
            effect: ToolEffect::WriteLocal,
            idempotency: Effect::Idempotent,
            risk: ToolRisk::Low,
            cancel: CancellationClass::AfterCompletion,
            timeout_ms: 15_000,
            budget: 4_096,
            dry: DryRunBehavior::Adapter,
            fast: false,
            schema: obj(json!({"session_id": {"type": "string"}})),
        },
        Entry {
            name: "browser_suspend_session",
            description: "Suspend a persistent browser session without deleting its profile for later resume.",
            tags: &["browser", "session", "suspend", "resume"],
            effect: ToolEffect::WriteLocal,
            idempotency: Effect::Idempotent,
            risk: ToolRisk::Low,
            cancel: CancellationClass::AfterCompletion,
            timeout_ms: 15_000,
            budget: 4_096,
            dry: DryRunBehavior::Adapter,
            fast: false,
            schema: obj(json!({"session_id": {"type": "string"}})),
        },
        Entry {
            name: "browser_tabs",
            description: "List tabs, open a new tab, switch the active tab or close a tab in a browser session.",
            tags: &["browser", "tabs", "list", "open", "switch", "close"],
            effect: ToolEffect::WriteLocal,
            idempotency: Effect::Keyed,
            risk: ToolRisk::Low,
            cancel: CancellationClass::AfterCompletion,
            timeout_ms: 15_000,
            budget: 16_384,
            dry: DryRunBehavior::Adapter,
            fast: false,
            schema: obj(
                json!({"session_id": {"type": "string"}, "op": {"type": "string"}, "tab_id": {"type": "string"}, "url": {"type": "string"}, "label": {"type": "string"}}),
            ),
        },
        Entry {
            name: "browser_navigate",
            description: "Navigate a tab to a URL, or go back, forward and reload the current document.",
            tags: &["browser", "navigate", "url", "back", "reload"],
            effect: ToolEffect::WriteExternal,
            idempotency: Effect::Idempotent,
            risk: ToolRisk::Medium,
            cancel: CancellationClass::Graceful,
            timeout_ms: 30_000,
            budget: 16_384,
            dry: DryRunBehavior::Adapter,
            fast: false,
            schema: obj(
                json!({"session_id": {"type": "string"}, "tab_id": {"type": "string"}, "url": {"type": "string"}, "op": {"type": "string"}}),
            ),
        },
        Entry {
            name: "browser_wait_ready",
            description: "Wait until the page reaches the requested readiness pattern before observing or acting.",
            tags: &["browser", "wait", "ready", "readiness"],
            effect: ToolEffect::ReadOnly,
            idempotency: Effect::Pure,
            risk: ToolRisk::Low,
            cancel: CancellationClass::Immediate,
            timeout_ms: 60_000,
            budget: 16_384,
            dry: DryRunBehavior::ReadOnly,
            fast: false,
            schema: obj(
                json!({"session_id": {"type": "string"}, "tab_id": {"type": "string"}, "strict": {"type": "boolean"}}),
            ),
        },
        Entry {
            name: "browser_snapshot",
            description: "Capture an accessible snapshot of the current page with roles, names and stable references for later actions.",
            tags: &["browser", "page", "snapshot", "capture", "accessibility"],
            effect: ToolEffect::ReadOnly,
            idempotency: Effect::Pure,
            risk: ToolRisk::Low,
            cancel: CancellationClass::Immediate,
            timeout_ms: 30_000,
            budget: 51_200,
            dry: DryRunBehavior::ReadOnly,
            fast: false,
            schema: obj(json!({"session_id": {"type": "string"}, "tab_id": {"type": "string"}})),
        },
        Entry {
            name: "browser_act",
            description: "Click a button or link, fill a form input, type text, select an option, upload a file or perform a committed page action from a snapshot reference.",
            tags: &[
                "browser", "page", "click", "button", "form", "input", "action",
            ],
            effect: ToolEffect::WriteExternal,
            idempotency: Effect::NonIdempotent,
            risk: ToolRisk::High,
            cancel: CancellationClass::AfterCompletion,
            timeout_ms: 20_000,
            budget: 16_384,
            dry: DryRunBehavior::Adapter,
            fast: false,
            schema: obj(
                json!({"session_id": {"type": "string"}, "tab_id": {"type": "string"}, "snapshot_id": {"type": "string"}, "request": {"type": "object"}}),
            ),
        },
        Entry {
            name: "browser_download",
            description: "Trigger a download in a tab and return the quarantined blob reference after scanning.",
            tags: &["browser", "download", "file", "quarantine"],
            effect: ToolEffect::WriteLocal,
            idempotency: Effect::Keyed,
            risk: ToolRisk::Medium,
            cancel: CancellationClass::Graceful,
            timeout_ms: 150_000,
            budget: 16_384,
            dry: DryRunBehavior::Adapter,
            fast: false,
            schema: obj(
                json!({"session_id": {"type": "string"}, "tab_id": {"type": "string"}, "trigger": {"type": "object"}}),
            ),
        },
        Entry {
            name: "browser_devtools",
            description: "Read console messages, network requests or performance timings for the current tab.",
            tags: &["browser", "devtools", "console", "network", "performance"],
            effect: ToolEffect::ReadOnly,
            idempotency: Effect::Pure,
            risk: ToolRisk::Low,
            cancel: CancellationClass::Immediate,
            timeout_ms: 10_000,
            budget: 16_384,
            dry: DryRunBehavior::ReadOnly,
            fast: false,
            schema: obj(
                json!({"session_id": {"type": "string"}, "tab_id": {"type": "string"}, "target": {"type": "string"}}),
            ),
        },
        Entry {
            name: "browser_emulate",
            description: "Apply a small or large viewport emulation preset to the current tab for viewer review.",
            tags: &["browser", "emulate", "viewport", "mobile", "desktop"],
            effect: ToolEffect::WriteLocal,
            idempotency: Effect::Idempotent,
            risk: ToolRisk::Low,
            cancel: CancellationClass::Immediate,
            timeout_ms: 10_000,
            budget: 4_096,
            dry: DryRunBehavior::Adapter,
            fast: false,
            schema: obj(
                json!({"session_id": {"type": "string"}, "tab_id": {"type": "string"}, "preset": {"type": "string"}}),
            ),
        },
        Entry {
            name: "browser_status",
            description: "Report browser instances, sessions, memory budgets and pending blocks for the run.",
            tags: &["browser", "status", "sessions", "memory"],
            effect: ToolEffect::ReadOnly,
            idempotency: Effect::Pure,
            risk: ToolRisk::Low,
            cancel: CancellationClass::Immediate,
            timeout_ms: 5_000,
            budget: 4_096,
            dry: DryRunBehavior::ReadOnly,
            fast: true,
            schema: obj(json!({})),
        },
    ];
    entries.iter().map(entry).collect()
}

/// Core search tools (arch 16.2 and 14.3): part of the eager prefix.
pub fn search_catalog() -> Vec<CatalogTool> {
    let entries = [
        Entry {
            name: "web_search",
            description: "Search the local index for pages matching the query with ranking and coverage metadata.",
            tags: &["search", "web", "query", "index", "ranking"],
            effect: ToolEffect::ReadOnly,
            idempotency: Effect::Pure,
            risk: ToolRisk::Low,
            cancel: CancellationClass::Immediate,
            timeout_ms: 10_000,
            budget: 51_200,
            dry: DryRunBehavior::ReadOnly,
            fast: true,
            schema: obj(json!({"query": {"type": "string"}, "k": {"type": "number"}})),
        },
        Entry {
            name: "web_read",
            description: "Read a page over HTTP with a browser fallback, returning Markdown, fragments and citation data.",
            tags: &["search", "web", "read", "page", "citation"],
            effect: ToolEffect::ReadOnly,
            idempotency: Effect::Pure,
            risk: ToolRisk::Low,
            cancel: CancellationClass::Immediate,
            timeout_ms: 45_000,
            budget: 51_200,
            dry: DryRunBehavior::ReadOnly,
            fast: true,
            schema: obj(json!({"url": {"type": "string"}, "range": {"type": "string"}})),
        },
    ];
    entries.iter().map(entry).collect()
}

/// Full web catalog: deferred browser tools plus core search tools.
pub fn web_catalog() -> Vec<CatalogTool> {
    let mut tools = browser_catalog();
    tools.extend(search_catalog());
    tools
}

/// Static cancellation class per tool (arch 16.5).
pub fn cancellation_class(name: &str) -> CancellationClass {
    match name {
        "browser_wait_ready" | "browser_snapshot" | "browser_devtools" | "browser_emulate"
        | "browser_status" | "web_search" | "web_read" => CancellationClass::Immediate,
        "browser_open_session" | "browser_navigate" | "browser_download" => {
            CancellationClass::Graceful
        }
        _ => CancellationClass::AfterCompletion,
    }
}

/// Static catalog effect per tool (arch 16.2); `prepare` may downgrade `browser_act`.
pub fn static_effect(name: &str) -> Effect {
    match name {
        "browser_open_session" | "browser_tabs" | "browser_download" => Effect::Keyed,
        "browser_close_session"
        | "browser_suspend_session"
        | "browser_navigate"
        | "browser_emulate" => Effect::Idempotent,
        "browser_act" => Effect::NonIdempotent,
        _ => Effect::Pure,
    }
}

/// Names handled by this adapter.
pub fn is_web_tool(name: &str) -> bool {
    web_catalog().iter().any(|t| t.name == name)
}

/// Shared benchmark-origin guard (same vocabulary as the eval profile guard;
/// network policy stays with the host container configuration).
pub fn benchmark_source_blocked(url: &str) -> bool {
    let value = url.to_ascii_lowercase();
    [
        "tbench.ai",
        "harborframework.com",
        "harbor-framework/harbor",
        "harbor-framework/terminal-bench",
        "stanfordnlp/terminal-bench",
        "laude-institute/terminal-bench",
        "terminal-bench/terminal-bench",
        "harbor hub",
        "terminal-bench-2-1",
    ]
    .iter()
    .any(|needle| value.contains(needle))
}

fn args_string(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Harness adapter for browser and search tools (arch 16).
///
/// Wraps an inner host runtime: web tools are prepared, executed and
/// reconciled here, every other tool delegates untouched.
pub struct WebHarness {
    inner: Arc<dyn ToolRuntime>,
    backend: Arc<dyn BrowserBackend>,
    web: bool,
    eval: bool,
    ephemeral: Mutex<HashMap<String, HashSet<String>>>,
}

impl WebHarness {
    /// Build the adapter over an inner runtime and an explicit backend.
    pub fn new(
        inner: Arc<dyn ToolRuntime>,
        backend: Arc<dyn BrowserBackend>,
        web: bool,
        eval: bool,
    ) -> Self {
        Self {
            inner,
            backend,
            web,
            eval,
            ephemeral: Mutex::new(HashMap::new()),
        }
    }

    /// Wrap the host runtime when the harness is explicitly enabled via the
    /// environment; otherwise return the inner runtime unchanged.
    pub fn wrap_from_env(
        inner: Arc<dyn ToolRuntime>,
        web: bool,
        eval: bool,
    ) -> Arc<dyn ToolRuntime> {
        match backend_from_env() {
            Some(backend) => Arc::new(Self::new(inner, backend, web, eval)),
            None => inner,
        }
    }

    /// Start human takeover for a blocked session (arch 17).
    ///
    /// Thin wrapper over the existing `takeover.start` RPC: no new
    /// authorization is introduced, ownership stays with webd.
    pub async fn takeover_start(&self, run: &str, session: &str) -> Result<Value, String> {
        self.backend
            .request(
                "takeover.start",
                json!({"run_id": run, "session_id": session, "deadline_ms": 30_000}),
            )
            .await
    }

    /// Issue a single-use viewer code for a tab under human control.
    ///
    /// Thin wrapper over the existing `viewer.issue` RPC.
    pub async fn viewer_issue(&self, run: &str, session: &str, tab: &str) -> Result<Value, String> {
        self.backend
            .request(
                "viewer.issue",
                json!({"run_id": run, "session_id": session, "tab_id": tab, "deadline_ms": 30_000}),
            )
            .await
    }

    fn web_definitions(&self) -> Vec<ToolDefinition> {
        web_catalog().into_iter().map(|t| t.definition()).collect()
    }

    fn check_args(&self, call: &ToolCall) -> Result<Value, String> {
        let args: Value =
            serde_json::from_str(&call.arguments).map_err(|_| "invalid tool arguments")?;
        if !args.is_object() {
            return Err("tool arguments must be an object".into());
        }
        Ok(args)
    }

    fn act_effect(args: &Value) -> Effect {
        let action = args
            .get("request")
            .and_then(|r| r.get("action"))
            .and_then(Value::as_str)
            .or_else(|| args.get("class").and_then(Value::as_str))
            .unwrap_or("");
        match action {
            "scroll" | "hover" | "observe" => Effect::Pure,
            "type" | "select" | "fill" | "input" => Effect::Idempotent,
            _ => Effect::NonIdempotent,
        }
    }

    fn eval_gate(&self, name: &str, args: &Value) -> Result<(), String> {
        if !self.eval {
            return Ok(());
        }
        if name == "browser_open_session" {
            let profile = args
                .get("profile")
                .and_then(Value::as_str)
                .unwrap_or("ephemeral");
            if profile != "ephemeral" {
                return Err("persistent profiles are denied by the eval profile".into());
            }
        }
        let url = match name {
            "browser_navigate" => args_string(args, "url"),
            "web_read" => args_string(args, "url"),
            "web_search" => args_string(args, "query"),
            _ => None,
        };
        if url.as_deref().is_some_and(benchmark_source_blocked) {
            return Err("benchmark sources are prohibited".into());
        }
        Ok(())
    }

    fn blocked_result(value: &Value) -> bool {
        value
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|s| matches!(s, "blocked" | "input_required" | "human_control"))
            || value.get("blocked").is_some()
    }

    fn track_ephemeral(&self, run: &str, session: &str, profile: Option<&str>) {
        if profile.unwrap_or("ephemeral") != "ephemeral" {
            return;
        }
        if let Ok(mut guard) = self.ephemeral.lock() {
            guard
                .entry(run.to_owned())
                .or_default()
                .insert(session.to_owned());
        }
    }

    fn params(&self, run: &str, data: &Value, extra: Value) -> Value {
        let mut params = data.get("params").cloned().unwrap_or_else(|| json!({}));
        if let Value::Object(ref mut map) = params {
            map.insert("run_id".into(), json!(run));
            for key in ["session_id", "tab_id"] {
                if let Some(value) = data.get(key) {
                    map.insert(key.into(), value.clone());
                }
            }
            if let Value::Object(extra) = extra {
                for (k, v) in extra {
                    map.insert(k, v);
                }
            }
            if map.get("deadline_ms").is_none() {
                map.insert("deadline_ms".into(), json!(30_000));
            }
        }
        params
    }

    async fn op_get(&self, run: &str, op_key: &str) -> Result<Value, String> {
        self.backend
            .request(
                "op.get",
                json!({"run_id": run, "op_key": op_key, "deadline_ms": 5_000}),
            )
            .await
    }

    /// Request server-side cancellation of an in-flight dispatched operation
    /// (arch 16.5, 16.10; NB-13).
    ///
    /// Thin wrapper over the `op.cancel` RPC: the owner check stays with
    /// webd (`run_id` must own `op_key`) and no new authorization is
    /// introduced. A server without `op.cancel` answers `method_not_found`;
    /// callers keep the existing client-side polling stop plus ephemeral
    /// cleanup fallback in that case.
    pub async fn cancel_operation(&self, run: &str, op_key: &str) -> Result<Value, String> {
        self.backend
            .request(
                "op.cancel",
                json!({"run_id": run, "op_key": op_key, "deadline_ms": 5_000}),
            )
            .await
    }

    async fn probe_url(&self, run: &str, session: &str, tab: &str, target: &str) -> bool {
        let listed = self
            .backend
            .request(
                "tab.list",
                json!({"run_id": run, "session_id": session, "deadline_ms": 10_000}),
            )
            .await;
        let tabs = match listed {
            Ok(v) => v.get("tabs").cloned().unwrap_or(v),
            Err(_) => Value::Null,
        };
        tabs.as_array().is_some_and(|items| {
            items.iter().any(|t| {
                t.get("id").and_then(Value::as_str) == Some(tab)
                    && t.get("url").and_then(Value::as_str) == Some(target)
            })
        })
    }

    async fn probe_session_gone(&self, run: &str, session: &str) -> bool {
        let listed = self
            .backend
            .request(
                "session.list",
                json!({"run_id": run, "deadline_ms": 10_000}),
            )
            .await;
        match listed {
            Err(_) => false,
            Ok(v) => {
                let sessions = v
                    .get("sessions")
                    .or(Some(&v))
                    .cloned()
                    .unwrap_or(Value::Null);
                !sessions.as_array().is_some_and(|items| {
                    items.iter().any(|s| {
                        s.get("id").and_then(Value::as_str) == Some(session)
                            || s.get("session_id").and_then(Value::as_str) == Some(session)
                    })
                })
            }
        }
    }
}

impl ToolRuntime for WebHarness {
    fn definitions(&self) -> Vec<ToolDefinition> {
        if !self.web {
            return self.inner.definitions();
        }
        let mut defs = self.inner.definitions();
        for def in self.web_definitions() {
            if !defs.iter().any(|d| d.name == def.name) {
                defs.push(def);
            }
        }
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    fn scheduling<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Option<ToolScheduling>> {
        Box::pin(async move {
            if !is_web_tool(&call.name) || !self.web {
                return self.inner.scheduling(run, call).await;
            }
            let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
            let mut resources = Vec::new();
            let write = !matches!(static_effect(&call.name), Effect::Pure);
            if let (Some(sid), Some(tid)) = (
                args_string(&args, "session_id"),
                args_string(&args, "tab_id"),
            ) {
                resources.push(Resource {
                    key: format!("browser-tab:{sid}/{tid}"),
                    write,
                });
            } else if let Some(sid) = args_string(&args, "session_id") {
                resources.push(Resource {
                    key: format!("browser-session:{sid}"),
                    write,
                });
            }
            Some(ToolScheduling {
                resources,
                exclusive: write,
            })
        })
    }

    fn prepare<'a>(
        &'a self,
        run: &'a str,
        call: ToolCall,
    ) -> BoxFuture<'a, Result<PreparedTool, String>> {
        Box::pin(async move {
            if !is_web_tool(&call.name) {
                return self.inner.prepare(run, call).await;
            }
            if !self.web {
                return Err("web tools are disabled by host configuration".into());
            }
            let args = self.check_args(&call)?;
            self.eval_gate(&call.name, &args)?;
            let (method, mut params) = route(&call.name, &args)?;
            let op_key = args_string(&args, "op_key").unwrap_or_else(|| call.id.clone());
            if params.get("op_key").is_none()
                && let Value::Object(ref mut map) = params
            {
                map.insert("op_key".into(), json!(op_key));
            }
            let effect = if call.name == "browser_act" {
                Self::act_effect(&args)
            } else {
                static_effect(&call.name)
            };
            let write = !matches!(effect, Effect::Pure);
            let mut resources = Vec::new();
            match (
                args_string(&args, "session_id"),
                args_string(&args, "tab_id"),
            ) {
                (Some(sid), Some(tid)) => resources.push(Resource {
                    key: format!("browser-tab:{sid}/{tid}"),
                    write,
                }),
                (Some(sid), None) => resources.push(Resource {
                    key: format!("browser-session:{sid}"),
                    write,
                }),
                _ => {}
            }
            let tool_name = call.name.clone();
            let mut data = json!({
                "tool": tool_name, "method": method, "params": params, "op_key": op_key,
                "session_id": args.get("session_id").cloned().unwrap_or(Value::Null),
                "tab_id": args.get("tab_id").cloned().unwrap_or(Value::Null),
                "target_url": args.get("url").cloned().unwrap_or(Value::Null),
                "profile": args.get("profile").cloned().unwrap_or(Value::Null),
            });
            if let Value::Object(ref mut map) = data {
                map.insert("timeout_ms".into(), json!(timeout_ms(&tool_name)));
            }
            Ok(PreparedTool {
                call,
                effect,
                resources,
                exclusive: write,
                data,
            })
        })
    }

    fn execute<'a>(
        &'a self,
        run: &'a str,
        _key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            if !is_web_tool(&tool.call.name) {
                return self.inner.execute(run, _key, tool).await;
            }
            let result = self.execute_web(run, tool).await;
            match result {
                Ok(value) => {
                    if self.eval && Self::blocked_result(&value) {
                        return ToolResult::error(
                            "blocked: human intervention is unavailable in eval".to_string(),
                        );
                    }
                    if tool.call.name == "web_search" {
                        return ToolResult::ok(filter_benchmark_results(&value));
                    }
                    ToolResult::ok(value)
                }
                Err(error) => ToolResult::error(error),
            }
        })
    }

    fn reconcile<'a>(
        &'a self,
        run: &'a str,
        _key: &'a str,
        tool: &'a PreparedTool,
    ) -> BoxFuture<'a, Reconcile> {
        Box::pin(async move {
            if !is_web_tool(&tool.call.name) {
                return self.inner.reconcile(run, _key, tool).await;
            }
            let name = tool.call.name.as_str();
            if matches!(static_effect(name), Effect::Pure) && name != "browser_act" {
                return Reconcile::Retry;
            }
            if name == "browser_act" && matches!(tool.effect, Effect::Pure | Effect::Idempotent) {
                return Reconcile::Retry;
            }
            if matches!(tool.effect, Effect::Keyed) {
                return Reconcile::Retry;
            }
            if matches!(tool.effect, Effect::Idempotent) {
                if name == "browser_navigate" {
                    let target = tool
                        .data
                        .get("target_url")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let sid = tool
                        .data
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let tid = tool
                        .data
                        .get("tab_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if !target.is_empty() && self.probe_url(run, sid, tid, target).await {
                        return Reconcile::Done(ToolResult::ok(
                            json!({"url": target, "reconciled": true}),
                        ));
                    }
                    return Reconcile::Retry;
                }
                if matches!(name, "browser_close_session" | "browser_suspend_session") {
                    let sid = tool
                        .data
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if self.probe_session_gone(run, sid).await {
                        return Reconcile::Done(ToolResult::ok(
                            json!({"session_id": sid, "reconciled": true}),
                        ));
                    }
                    return Reconcile::Retry;
                }
                return Reconcile::Retry;
            }
            let op_key = tool
                .data
                .get("op_key")
                .and_then(Value::as_str)
                .unwrap_or("");
            if op_key.is_empty() {
                return Reconcile::Uncertain("commit without operation key never repeats".into());
            }
            match self.op_get(run, op_key).await {
                Ok(state) => {
                    let status = state.get("state").and_then(Value::as_str).unwrap_or("");
                    match status {
                        "completed" => Reconcile::Done(ToolResult::ok(
                            state.get("result").cloned().unwrap_or(Value::Null),
                        )),
                        "received" => Reconcile::Retry,
                        "failed" => Reconcile::Uncertain(format!(
                            "operation {op_key} failed; effect unknown"
                        )),
                        _ => Reconcile::Uncertain(format!(
                            "operation {op_key} is {status}; effect unknown"
                        )),
                    }
                }
                Err(error) if error.contains("unknown_operation") => Reconcile::Retry,
                Err(error) => Reconcile::Uncertain(error),
            }
        })
    }

    fn interrupt_execution<'a>(
        &'a self,
        run: &'a str,
        cancelled: bool,
        tool: Option<&'a PreparedTool>,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let Some(tool) = tool else {
                return self.inner.interrupt_execution(run, cancelled, None).await;
            };
            if !is_web_tool(&tool.call.name) {
                return self
                    .inner
                    .interrupt_execution(run, cancelled, Some(tool))
                    .await;
            }
            let class = cancellation_class(&tool.call.name);
            if matches!(class, CancellationClass::Immediate) {
                return Ok(true);
            }
            // Best-effort server-side cancellation for in-flight dispatched
            // work (commit and keyed classes carry an `op_key`). A server
            // without `op.cancel` answers `method_not_found`; the polling
            // stop plus ephemeral cleanup below stays the fallback either
            // way, and the error is never surfaced as a cancellation
            // failure.
            if let Some(op_key) = tool.data.get("op_key").and_then(Value::as_str)
                && !op_key.is_empty()
                && !matches!(tool.effect, Effect::Pure)
            {
                let _ = self.cancel_operation(run, op_key).await;
            }
            let mut closed = false;
            if cancelled {
                let owned: Vec<String> = self
                    .ephemeral
                    .lock()
                    .map(|mut guard| {
                        guard
                            .remove(run)
                            .map(|s| s.into_iter().collect())
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                for sid in owned {
                    if self
                        .backend
                        .request(
                            "session.close",
                            json!({"run_id": run, "session_id": sid, "deadline_ms": 10_000}),
                        )
                        .await
                        .is_ok()
                    {
                        closed = true;
                    }
                }
            }
            if matches!(class, CancellationClass::Graceful) {
                return Ok(true);
            }
            Ok(closed)
        })
    }

    fn execution_state<'a>(
        &'a self,
        run: &'a str,
        state: &'a str,
        reason: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if matches!(state, "paused" | "completed" | "failed" | "cancelled")
                && let Ok(mut guard) = self.ephemeral.lock()
            {
                guard.remove(run);
            }
            self.inner.execution_state(run, state, reason).await
        })
    }

    fn live_status(&self, run: &str) -> Vec<Value> {
        let mut status = self.inner.live_status(run);
        if let Ok(guard) = self.ephemeral.lock()
            && let Some(sessions) = guard.get(run)
        {
            status.push(json!({"adapter": "web", "ephemeral_sessions": sessions.iter().collect::<Vec<_>>(), "eval": self.eval}));
        }
        status
    }

    fn result_requires_approval(&self, result: &ToolResult) -> bool {
        result
            .output
            .get("approval_required")
            .is_some_and(|v| v != &Value::Bool(false))
            || self.inner.result_requires_approval(result)
    }

    fn sanitize_durable(&self, value: &Value) -> Result<Value, String> {
        self.inner.sanitize_durable(value)
    }
    fn pending_approval<'a>(&'a self, run: &'a str) -> BoxFuture<'a, bool> {
        self.inner.pending_approval(run)
    }
    fn approval_blocked<'a>(&'a self, run: &'a str) -> BoxFuture<'a, Option<String>> {
        self.inner.approval_blocked(run)
    }
    fn begin_step<'a>(&'a self, run: &'a str, step: u64) -> BoxFuture<'a, Result<(), String>> {
        self.inner.begin_step(run, step)
    }
    fn end_step<'a>(&'a self, run: &'a str, step: u64) -> BoxFuture<'a, Result<(), String>> {
        self.inner.end_step(run, step)
    }
    fn observe_untrusted<'a>(
        &'a self,
        run: &'a str,
        source: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.inner.observe_untrusted(run, source)
    }
    fn audit_builtin<'a>(
        &'a self,
        run: &'a str,
        call: &'a ToolCall,
        result: Option<&'a ToolResult>,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.inner.audit_builtin(run, call, result)
    }
    fn configure_execution_limits<'a>(
        &'a self,
        run: &'a str,
        limits: excalibur_types::runtime::ExecutionLimits,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.inner.configure_execution_limits(run, limits)
    }
    fn execution_resources<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<excalibur_types::runtime::ExecutionResources>, String>> {
        self.inner.execution_resources(run)
    }
    fn execution_tool_budget<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Option<u64>, String>> {
        self.inner.execution_tool_budget(run)
    }
    fn configure_process_boundary<'a>(
        &'a self,
        run: &'a str,
        boundary: excalibur_types::runtime::ProcessBoundary,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.inner.configure_process_boundary(run, boundary)
    }
    fn configure_network_authorizer<'a>(
        &'a self,
        run: &'a str,
        authorizer: Arc<dyn excalibur_types::runtime::NetworkAuthorizer>,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.inner.configure_network_authorizer(run, authorizer)
    }
    fn finalization_guard<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn excalibur_types::runtime::FinalizationGuard>, String>> {
        self.inner.finalization_guard(run)
    }
    fn deferred_definitions<'a>(
        &'a self,
        run: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ToolDefinition>, String>> {
        self.inner.deferred_definitions(run)
    }
    fn durable_blob_inspector(
        &self,
    ) -> Option<Box<dyn excalibur_types::runtime::DurableByteInspector>> {
        self.inner.durable_blob_inspector()
    }
}

/// Catalog timeout per tool (arch 16.2).
fn timeout_ms(name: &str) -> u64 {
    web_catalog()
        .iter()
        .find(|t| t.name == name)
        .map(|t| t.timeout_ms)
        .unwrap_or(30_000)
}

/// Map a tool call to its webd RPC method and parameters (arch 16.2, 3.4).
fn route(name: &str, args: &Value) -> Result<(String, Value), String> {
    let sid = args.get("session_id").cloned().unwrap_or(Value::Null);
    let tid = args.get("tab_id").cloned().unwrap_or(Value::Null);
    let take = |keys: &[&str]| {
        let mut out = serde_json::Map::new();
        if let Value::Object(map) = args {
            for key in keys {
                if let Some(value) = map.get(*key) {
                    out.insert((*key).into(), value.clone());
                }
            }
        }
        Value::Object(out)
    };
    let method = match name {
        "browser_open_session" => ("session.open", take(&["profile", "mode", "site", "label"])),
        "browser_close_session" => ("session.close", json!({"session_id": sid})),
        "browser_suspend_session" => ("session.suspend", json!({"session_id": sid})),
        "browser_tabs" => match args.get("op").and_then(Value::as_str).unwrap_or("list") {
            "new" => ("tab.new", take(&["url", "label"])),
            "switch" => ("tab.switch", json!({"session_id": sid, "tab_id": tid})),
            "close" => ("tab.close", json!({"session_id": sid, "tab_id": tid})),
            "list" => ("tab.list", json!({"session_id": sid})),
            other => return Err(format!("unknown tabs operation: {other}")),
        },
        "browser_navigate" => match args.get("op").and_then(Value::as_str).unwrap_or("go") {
            "reload" => ("nav.reload", json!({"session_id": sid, "tab_id": tid})),
            "back" => ("nav.back", json!({"session_id": sid, "tab_id": tid})),
            "forward" => ("nav.forward", json!({"session_id": sid, "tab_id": tid})),
            "go" => ("nav.go", take(&["url"])),
            other => return Err(format!("unknown navigate operation: {other}")),
        },
        "browser_wait_ready" => ("observe.wait_ready", take(&["strict"])),
        "browser_snapshot" => ("observe.snapshot", json!({})),
        "browser_act" => ("act.perform", take(&["request"])),
        "browser_download" => ("download", take(&["trigger"])),
        "browser_devtools" => match args
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or("console")
        {
            "network" => ("devtools.network", json!({})),
            "performance" => ("devtools.performance", json!({})),
            "console" => ("devtools.console", json!({})),
            other => return Err(format!("unknown devtools target: {other}")),
        },
        "browser_emulate" => ("emulate.viewport", take(&["preset"])),
        "browser_status" => ("status.get", json!({})),
        "web_search" => ("search.query", take(&["query", "k"])),
        "web_read" => ("read.web_read", take(&["url", "range"])),
        _ => return Err(format!("unknown web tool: {name}")),
    };
    Ok((method.0.into(), method.1))
}

/// Drop benchmark sources from search results (arch 16.8).
fn filter_benchmark_results(value: &Value) -> Value {
    let mut filtered = value.clone();
    if let Some(results) = filtered.get_mut("results").and_then(Value::as_array_mut) {
        results.retain(|r| {
            let url = r.get("url").and_then(Value::as_str).unwrap_or("");
            !benchmark_source_blocked(url)
        });
    }
    filtered
}

impl WebHarness {
    async fn execute_web(&self, run: &str, tool: &PreparedTool) -> Result<Value, String> {
        let name = tool.call.name.as_str();
        let method = tool
            .data
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let backend = self.backend.clone();
        if name == "browser_act" {
            let request = tool
                .data
                .get("params")
                .and_then(|p| p.get("request"))
                .cloned()
                .unwrap_or(Value::Null);
            let plan = backend
                .request(
                    "act.prepare",
                    self.params(run, &tool.data, json!({"request": request})),
                )
                .await?;
            if plan
                .get("approval_required")
                .is_some_and(|v| v != &Value::Bool(false))
            {
                return Ok(plan);
            }
            let plan_id = plan.get("plan_id").cloned().unwrap_or(Value::Null);
            let op_key = tool.data.get("op_key").cloned().unwrap_or(Value::Null);
            let result = backend
                .request(
                    &method,
                    self.params(
                        run,
                        &tool.data,
                        json!({"plan_id": plan_id, "op_key": op_key}),
                    ),
                )
                .await?;
            return Ok(result);
        }
        if name == "browser_open_session" {
            let profile = tool
                .data
                .get("profile")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let result = backend
                .request(&method, self.params(run, &tool.data, json!({})))
                .await?;
            let sid = result
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if !sid.is_empty() {
                let owned = result
                    .get("profile")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(profile);
                self.track_ephemeral(run, &sid, owned.as_deref());
            }
            return Ok(result);
        }
        let timeout = tool
            .data
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000);
        backend
            .request(
                &method,
                self.params(run, &tool.data, json!({"deadline_ms": timeout})),
            )
            .await
    }
}
