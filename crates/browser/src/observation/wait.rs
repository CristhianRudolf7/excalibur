use super::*;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WaitCriteria {
    pub network_idle_ms: u64,
    pub dom_stable_ms: u64,
    pub selector: Option<String>,
    pub text: Option<String>,
    pub timeout_ms: u64,
    pub max_inflight: usize,
    pub strict_network: bool,
}
impl Default for WaitCriteria {
    fn default() -> Self {
        Self {
            network_idle_ms: 500,
            dom_stable_ms: 500,
            selector: None,
            text: None,
            timeout_ms: 10000,
            max_inflight: 0,
            strict_network: false,
        }
    }
}
impl WaitCriteria {
    pub fn validate(&self) -> Result<()> {
        if self.timeout_ms == 0
            || self.timeout_ms > 120000
            || self.network_idle_ms > 30000
            || self.dom_stable_ms > 30000
            || self.max_inflight > 1000
            || self.selector.as_ref().is_some_and(|s| s.len() > 4096)
            || self.text.as_ref().is_some_and(|s| s.len() > 4096)
        {
            return Err(Fault::new("invalid_params", "wait limits"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WaitResult {
    pub status: String,
    pub degraded: Vec<String>,
    pub criteria_met: CriteriaMet,
    pub blocked: Option<Value>,
    pub elapsed_ms: u64,
    pub document_epoch: u64,
    pub inflight_sample: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CriteriaMet {
    pub navigation: bool,
    pub network: bool,
    pub dom: bool,
    pub condition: bool,
}
pub(crate) struct NetworkScope {
    client: Client,
    tracker: Arc<TabObserver>,
    disable: bool,
}
impl Drop for NetworkScope {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        if self.tracker.network_users.fetch_sub(1, Ordering::SeqCst) == 1 && self.disable {
            let client = self.client.clone();
            let tracker = self.tracker.clone();
            tokio::spawn(async move {
                let _initialized = tracker.initialized.lock().await;
                if tracker.network_users.load(Ordering::SeqCst) != 0 {
                    return;
                }
                let sessions = tracker
                    .network_enabled
                    .lock()
                    .unwrap()
                    .drain()
                    .collect::<Vec<_>>();
                {
                    let mut state = tracker.state.lock().unwrap();
                    state.inflight.clear();
                    state.unobserved_sessions.extend(
                        sessions
                            .iter()
                            .cloned()
                            .map(|session| (session, String::new())),
                    );
                }
                *tracker.last_ready.lock().unwrap() = None;
                for session in &sessions {
                    let _ = client
                        .send_with_timeout(
                            Method::NetworkDisable,
                            json!({}),
                            Some(session),
                            Duration::from_millis(500),
                        )
                        .await;
                }
            });
        }
    }
}
impl Observation {
    pub(crate) async fn initialize(&self, ctx: &Context, frames: &[Frame]) -> Result<NetworkScope> {
        let tracker = self.tracker(ctx);
        tracker
            .network_users
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let scope = NetworkScope {
            client: ctx.client.clone(),
            tracker: tracker.clone(),
            disable: ctx.tab.listen,
        };
        let mut initialized = tracker.initialized.lock().await;
        let mut sessions = frames.iter().map(|f| f.session.clone()).collect::<Vec<_>>();
        sessions.sort();
        sessions.dedup();
        for sid in &sessions {
            if !initialized.contains(sid) {
                ctx.client
                    .send_with_timeout(
                        Method::PageEnable,
                        json!({}),
                        Some(sid),
                        Duration::from_millis(500),
                    )
                    .await?;
                ctx.client
                    .send_with_timeout(
                        Method::PageSetLifecycleEventsEnabled,
                        json!({"enabled":true}),
                        Some(sid),
                        Duration::from_millis(500),
                    )
                    .await?;
                ctx.client
                    .send_with_timeout(
                        Method::RuntimeEnable,
                        json!({}),
                        Some(sid),
                        Duration::from_millis(500),
                    )
                    .await?;
                ctx.client
                    .send_with_timeout(
                        Method::LogEnable,
                        json!({}),
                        Some(sid),
                        Duration::from_millis(500),
                    )
                    .await?;
                ctx.client.send_with_timeout(Method::RuntimeAddBinding,json!({"name":"__excalibur_observe","executionContextName":"excalibur-ready"}),Some(sid),Duration::from_millis(500)).await?;
                ctx.client.send_with_timeout(Method::PageAddScriptToEvaluateOnNewDocument,json!({"source":format!("({})(null,null)",include_str!("probe.js")),"worldName":"excalibur-ready"}),Some(sid),Duration::from_millis(500)).await?;
                ctx.client
                    .send_with_timeout(
                        Method::PageSetInterceptFileChooserDialog,
                        json!({"enabled":true}),
                        Some(sid),
                        Duration::from_millis(500),
                    )
                    .await?;
                initialized.insert(sid.clone());
            }
            let enable = !tracker.network_enabled.lock().unwrap().contains(sid);
            if enable {
                if frames
                    .iter()
                    .any(|frame| &frame.session == sid && frame.url != "about:blank")
                {
                    tracker.state.lock().unwrap().unobserved_sessions.insert(
                        sid.clone(),
                        frames
                            .iter()
                            .find(|f| &f.session == sid)
                            .map(|f| f.loader.clone())
                            .unwrap_or_default(),
                    );
                }
                ctx.client
                    .send_with_timeout(
                        Method::NetworkEnable,
                        json!({"maxTotalBufferSize":2097152,"maxResourceBufferSize":524288}),
                        Some(sid),
                        Duration::from_millis(500),
                    )
                    .await?;
                tracker.network_enabled.lock().unwrap().insert(sid.clone());
            }
        }
        tracker.state.lock().unwrap().sessions = sessions.clone();
        Ok(scope)
    }
    pub(crate) async fn wait(&self, ctx: &Context, criteria: WaitCriteria) -> Result<WaitResult> {
        criteria.validate()?;
        let tracker = self.tracker(ctx);
        {
            let state = tracker.state.lock().unwrap();
            if let Some(blocked) = state
                .dialog
                .clone()
                .or(state.response_blocked.clone())
                .or_else(|| {
                    state
                        .file_chooser
                        .then(|| json!({"kind":"file_chooser","confidence":"high"}))
                })
            {
                return Ok(WaitResult {
                    status: "blocked".into(),
                    degraded: vec![],
                    criteria_met: CriteriaMet::default(),
                    blocked: Some(blocked),
                    elapsed_ms: 0,
                    document_epoch: ctx.tab.document_epoch,
                    inflight_sample: vec![],
                });
            }
        }
        let frames = self.frames(ctx).await?;
        let _network = self.initialize(ctx, &frames).await?;
        let started = tokio::time::Instant::now();
        let deadline = started + Duration::from_millis(criteria.timeout_ms);
        let mut quiet = started;
        let mut long_quiet = started;
        let mut report = WaitResult {
            status: "timeout".into(),
            degraded: vec![],
            criteria_met: CriteriaMet::default(),
            blocked: None,
            elapsed_ms: 0,
            document_epoch: ctx.tab.document_epoch,
            inflight_sample: vec![],
        };
        loop {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            {
                let state = tracker.state.lock().unwrap();
                if let Some(dialog) = &state.dialog {
                    report.status = "blocked".into();
                    report.blocked = Some(dialog.clone());
                    break;
                }
            }
            let frames = match tokio::time::timeout_at(deadline, self.frames(ctx)).await {
                Ok(Ok(f)) => f,
                _ => {
                    if !report.degraded.iter().any(|s| s == "renderer_unresponsive") {
                        report.degraded.push("renderer_unresponsive".into());
                    }
                    frames.clone()
                }
            };
            let _frame_scope = self.initialize(ctx, &frames).await?;
            let mut probes = tokio::task::JoinSet::new();
            for frame in &frames {
                let client = ctx.client.clone();
                let f = frame.clone();
                let selector = criteria.selector.clone();
                let text = criteria.text.clone();
                let logged_in = ctx.logged_in;
                probes.spawn(async move {
                    let v = probe(&client, &f, selector, text, logged_in).await;
                    (f, v)
                });
            }
            let mut states = vec![];
            while let Ok(Some(Ok(v))) = tokio::time::timeout_at(deadline, probes.join_next()).await
            {
                states.push(v)
            }
            let complete = states.len() == frames.len() && states.iter().all(|(_, s)| s.is_ok());
            let has_condition = criteria.selector.is_some() || criteria.text.is_some();
            let mut nav = complete;
            let mut dom = complete;
            let mut condition = !has_condition;
            for (frame, value) in &states {
                if let Ok(s) = value {
                    nav &= s["ready"]
                        .as_str()
                        .is_some_and(|s| matches!(s, "interactive" | "complete"));
                    dom &= s["dom_age"].as_f64().unwrap_or(0.) >= criteria.dom_stable_ms as f64
                        && s["beat_age"].as_f64().unwrap_or(99999.) < 1000.;
                    condition |= has_condition && s["condition"] == true;
                    if !s["blocked"].is_null() {
                        let mut block = s["blocked"].clone();
                        block["frame_id"] = json!(frame.id);
                        report.blocked = Some(block);
                    }
                }
            }
            let (active, long, dialog, file, lagged, pending, sample) = {
                let s = tracker.state.lock().unwrap();
                let active = s
                    .inflight
                    .values()
                    .filter(|(at, _, _)| at.elapsed() < Duration::from_secs(3))
                    .count();
                let long = s.inflight.len() - active;
                (
                    active,
                    long,
                    s.dialog.clone().or(s.response_blocked.clone()),
                    s.file_chooser,
                    s.lagged,
                    s.navigation_pending,
                    s.inflight
                        .values()
                        .map(|(_, u, _)| u.clone())
                        .take(10)
                        .collect::<Vec<_>>(),
                )
            };
            if let Some(dialog) = dialog {
                report.blocked = Some(dialog)
            }
            if file {
                report.blocked = Some(json!({"kind":"file_chooser","confidence":"high"}))
            }
            if lagged && !report.degraded.iter().any(|v| v == "event_gap") {
                report.degraded.push("event_gap".into());
            }
            if active > criteria.max_inflight {
                quiet = tokio::time::Instant::now();
                long_quiet = quiet;
            }
            if long == 0 {
                long_quiet = tokio::time::Instant::now();
            }
            quiet = quiet.max(tracker.state.lock().unwrap().last_network_activity);
            let unobserved = !tracker.state.lock().unwrap().unobserved_sessions.is_empty();
            if unobserved && !report.degraded.iter().any(|s| s == "network_unobserved") {
                report.degraded.push("network_unobserved".into());
            }
            if !unobserved {
                report.degraded.retain(|s| s != "network_unobserved");
            }
            let network = active <= criteria.max_inflight
                && quiet.elapsed() >= Duration::from_millis(criteria.network_idle_ms)
                && long == 0
                && !lagged
                && !unobserved;
            let degraded_network = active <= criteria.max_inflight
                && long > 0
                && long_quiet.elapsed() >= Duration::from_millis(criteria.network_idle_ms + 2000);
            let unobserved_ready = unobserved
                && active <= criteria.max_inflight
                && quiet.elapsed() >= Duration::from_millis(criteria.network_idle_ms + 2000);
            nav &= !pending;
            report.criteria_met = CriteriaMet {
                navigation: nav,
                network,
                dom,
                condition,
            };
            report.inflight_sample = sample;
            if report.blocked.is_some() {
                report.status = "blocked".into();
                break;
            }
            if nav
                && dom
                && condition
                && (network
                    || (!criteria.strict_network
                        && (degraded_network || unobserved_ready)
                        && !lagged))
            {
                report.status = "ready".into();
                if degraded_network {
                    report.degraded.push("network_busy".into());
                }
                *tracker.last_ready.lock().unwrap() = Some((
                    frames
                        .iter()
                        .map(|f| (f.id.clone(), f.loader.clone()))
                        .collect(),
                    {
                        let mut ready = report.clone();
                        ready.elapsed_ms = started.elapsed().as_millis() as u64;
                        ready
                    },
                ));
                break;
            }
            tokio::time::sleep_until(
                (tokio::time::Instant::now() + Duration::from_millis(100)).min(deadline),
            )
            .await;
        }
        report.elapsed_ms = started.elapsed().as_millis() as u64;
        report.inflight_sample = report
            .inflight_sample
            .iter()
            .map(|s| self.redactor.redact_text(s))
            .collect();
        Ok(report)
    }
    pub(crate) async fn devtools(
        &self,
        ctx: &Context,
        kind: &str,
        filters: Value,
    ) -> Result<Value> {
        let tracker = self.tracker(ctx);
        let frames = self.frames(ctx).await?;
        let _network = self.initialize(ctx, &frames).await?;
        if filters["tainted"] == true {
            tracker.state.lock().unwrap().tainted = true;
        }
        if kind == "performance" {
            ctx.client
                .send_with_timeout(
                    Method::PerformanceEnable,
                    json!({}),
                    Some(&tracker.session),
                    Duration::from_millis(500),
                )
                .await?;
            return Ok(self.redactor.redact_json(
                &ctx.client
                    .send_with_timeout(
                        Method::PerformanceGetMetrics,
                        json!({}),
                        Some(&tracker.session),
                        Duration::from_millis(500),
                    )
                    .await?,
            ));
        }
        if !matches!(kind, "console" | "network") {
            return Err(Fault::new("unsupported_type", kind));
        }
        let s = tracker.state.lock().unwrap();
        let ring = if kind == "console" {
            &s.console
        } else {
            &s.network
        };
        let since = filters["since_seq"].as_u64().unwrap_or(0);
        let limit = filters["limit"].as_u64().unwrap_or(100).min(500) as usize;
        let records = ring
            .iter()
            .filter(|v| {
                v["seq"].as_u64().unwrap_or(0) > since
                    && filters["level"]
                        .as_str()
                        .is_none_or(|level| v["level"] == level)
                    && filters["url_contains"]
                        .as_str()
                        .is_none_or(|part| v["url"].as_str().unwrap_or("").contains(part))
            })
            .take(limit)
            .cloned()
            .map(|mut v| {
                if s.tainted
                    && let Some(url) = v["url"].as_str()
                {
                    v["url"] = json!(clean_url(url, true));
                }
                v
            })
            .collect::<Vec<_>>();
        Ok(self.redactor.redact_json(&json!({"records":records,"next_seq":s.sequence,"dropped":s.dropped,"event_gap":s.lagged,"untrusted":true})))
    }
}
async fn probe(
    client: &Client,
    frame: &Frame,
    selector: Option<String>,
    text: Option<String>,
    logged_in: bool,
) -> Result<Value> {
    let operation = async {
        let world = client
            .send(
                Method::PageCreateIsolatedWorld,
                json!({"frameId":frame.id,"worldName":"excalibur-ready"}),
                Some(&frame.session),
            )
            .await?;
        let result=client.send(Method::RuntimeCallFunctionOn,json!({"executionContextId":world["executionContextId"],"functionDeclaration":include_str!("probe.js"),"arguments":[{"value":selector},{"value":text},{"value":logged_in}],"returnByValue":true}),Some(&frame.session)).await?;
        if !result["exceptionDetails"].is_null() {
            return Err(Fault::new("invalid_selector", "readiness probe failed"));
        }
        Ok(result["result"]["value"].clone())
    };
    tokio::time::timeout(Duration::from_millis(500), operation)
        .await
        .map_err(|_| Fault::new("cdp_timeout", "readiness probe"))?
}
