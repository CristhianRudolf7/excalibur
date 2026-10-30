//! Trusted host action boundary. No arbitrary JavaScript or host filesystem paths are accepted.
use super::*;
use crate::sessions::Manager;
use std::collections::HashSet;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRequest {
    pub action: String,
    #[serde(default)]
    pub snapshot_id: String,
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub keys: Option<String>,
    #[serde(default)]
    pub target_ref: Option<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub options: ActionOptions,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActionOptions {
    pub button: Option<String>,
    pub click_count: Option<u32>,
    pub modifiers: Option<u32>,
    pub key_events: bool,
    pub html5: bool,
    pub direction: Option<String>,
    pub amount: Option<f64>,
    pub accept: Option<bool>,
    pub prompt_text: Option<String>,
}
#[derive(Clone)]
struct Plan {
    run: String,
    sid: String,
    tid: String,
    request: ActionRequest,
    class: String,
    expires: tokio::time::Instant,
}
#[derive(Default)]
pub(crate) struct ActionState {
    plans: Mutex<HashMap<String, Plan>>,
    pub(super) staged: Mutex<HashMap<String, super::files::Staged>>,
    pub(super) download_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    filled: Mutex<HashSet<ElementKey>>,
    vision: Mutex<HashMap<String, (String, tokio::time::Instant)>>,
}
/// While alive, alerts raised by input are accepted and reported, and `beforeunload`
/// is accepted for agent navigation. Navigation alone leaves alerts as blocking dialogs.
pub(crate) struct ActionScope {
    tracker: Arc<TabObserver>,
    input: bool,
    nav: bool,
}
impl Drop for ActionScope {
    fn drop(&mut self) {
        let mut s = self.tracker.state.lock().unwrap();
        if self.input {
            s.active_actions = s.active_actions.saturating_sub(1);
        }
        if self.nav {
            s.active_navigation = s.active_navigation.saturating_sub(1);
        }
    }
}
impl Observation {
    pub(crate) fn action_scope(&self, ctx: &Context, input: bool, nav: bool) -> ActionScope {
        let tracker = self.tracker(ctx);
        {
            let mut s = tracker.state.lock().unwrap();
            if input {
                if s.active_actions == 0 {
                    s.action_alerts.clear();
                }
                s.active_actions += 1;
            }
            if nav {
                s.active_navigation += 1;
            }
        }
        ActionScope {
            tracker,
            input,
            nav,
        }
    }
    pub(crate) fn secret_mask(&self, key: &ElementKey) -> bool {
        self.actions.filled.lock().unwrap().contains(key)
    }
    pub(crate) fn vision_blocked(&self, ctx: &Context, loader: &str) -> bool {
        self.actions
            .vision
            .lock()
            .unwrap()
            .get(&ctx.tab.id)
            .is_some_and(|(old, at)| old == loader || at.elapsed() < Duration::from_secs(5))
    }
    async fn action_key(
        &self,
        ctx: &Context,
        r: &ActionRequest,
        reference: &str,
    ) -> Result<ElementKey> {
        let check = self.validate(ctx, &r.snapshot_id, reference).await?;
        if check["status"] != "valid" {
            return Err(Fault::new("stale_ref", check.to_string()));
        }
        serde_json::from_value(check["key"].clone()).map_err(|e| Fault::new("invalid_reference", e))
    }
    pub(super) async fn target(
        &self,
        ctx: &Context,
        r: &ActionRequest,
        reference: &str,
    ) -> Result<Target> {
        let key = self.action_key(ctx, r, reference).await?;
        let frames = self.frames(ctx).await?;
        let frame = frames
            .iter()
            .find(|f| f.id == key.frame_id)
            .cloned()
            .ok_or_else(|| Fault::new("stale_ref", "frame_gone"))?;
        let world = ctx
            .client
            .send_with_timeout(
                Method::PageCreateIsolatedWorld,
                json!({"frameId":frame.id,"worldName":"excalibur-actions"}),
                Some(&frame.session),
                Duration::from_millis(500),
            )
            .await?;
        let value=ctx.client.send_with_timeout(Method::DOMResolveNode,json!({"backendNodeId":key.backend_node_id,"executionContextId":world["executionContextId"]}),Some(&frame.session),Duration::from_millis(500)).await?;
        let object = value["object"]["objectId"]
            .as_str()
            .ok_or_else(|| Fault::new("stale_ref", "node_gone"))?
            .to_owned();
        let metadata = call(
            &ctx.client,
            &frame.session,
            &object,
            include_str!("actions/target.js"),
            vec![],
        )
        .await?;
        Ok(Target {
            client: ctx.client.clone(),
            key,
            frame,
            object,
            world: world["executionContextId"].clone(),
            metadata,
        })
    }
}
pub(super) struct Target {
    client: Client,
    pub(super) key: ElementKey,
    pub(super) frame: Frame,
    object: String,
    world: Value,
    metadata: Value,
}
impl Drop for Target {
    fn drop(&mut self) {
        let c = self.client.clone();
        let sid = self.frame.session.clone();
        let object = self.object.clone();
        tokio::spawn(async move {
            let _ = c
                .send_with_timeout(
                    Method::RuntimeReleaseObject,
                    json!({"objectId":object}),
                    Some(&sid),
                    Duration::from_millis(500),
                )
                .await;
        });
    }
}
async fn call(
    c: &Client,
    sid: &str,
    object: &str,
    function: &str,
    args: Vec<Value>,
) -> Result<Value> {
    let v=c.send_with_timeout(Method::RuntimeCallFunctionOn,json!({"objectId":object,"functionDeclaration":function,"arguments":args,"awaitPromise":true,"returnByValue":true}),Some(sid),Duration::from_millis(750)).await?;
    if !v["exceptionDetails"].is_null() {
        return Err(Fault::new("not_actionable", "DOM operation failed"));
    }
    Ok(v["result"]["value"].clone())
}
fn classify(
    r: &ActionRequest,
    m: &Value,
    persistent: bool,
    listen: bool,
) -> (&'static str, &'static str, &'static str) {
    let name = m["name"].as_str().unwrap_or("").to_lowercase();
    let words = [
        "enviar",
        "send",
        "submit",
        "publicar",
        "post",
        "comprar",
        "buy",
        "pagar",
        "pay",
        "confirmar",
        "confirm",
        "eliminar",
        "delete",
        "borrar",
        "transferir",
        "transfer",
        "firmar",
        "sign",
        "reservar",
        "book",
        "suscribir",
        "subscribe",
        "aceptar",
        "accept",
    ];
    let commit = words
        .iter()
        .any(|w| name.split(|c: char| !c.is_alphanumeric()).any(|s| s == *w))
        || (r.action == "click"
            && m["form"] == true
            && matches!(m["tag"].as_str(), Some("button" | "input")))
        || (r.action == "press"
            && r.keys.as_deref().unwrap_or("").contains("Enter")
            && (m["form"] == true || listen))
        || (r.action == "dialog" && r.options.accept == Some(true) && persistent);
    let class = match r.action.as_str() {
        "scroll" | "hover" => "observe",
        "type" | "select" => "input",
        "upload" => "upload",
        "drag" => "drag",
        _ if commit => "commit",
        "click"
            if m["tag"] == "a"
                && m["href"].as_str().is_some_and(|s| !s.is_empty())
                && m["form"] != true
                && m["clickable"] != true =>
        {
            "navigate_link"
        }
        "click" if persistent => "commit",
        "click" | "press" => "navigate_link",
        _ => "input",
    };
    let effect = if r.action == "dialog" {
        "non_idempotent"
    } else {
        match class {
            "observe" => "pure",
            "input" => "idempotent",
            _ => "non_idempotent",
        }
    };
    let risk = if class == "observe" {
        "low"
    } else if matches!(class, "upload" | "credential")
        || (persistent && matches!(class, "commit" | "drag"))
    {
        "high"
    } else if persistent || matches!(class, "commit" | "drag") {
        "medium"
    } else {
        "low"
    };
    (class, effect, risk)
}
fn request_valid(r: &ActionRequest) -> Result<()> {
    if !matches!(
        r.action.as_str(),
        "click" | "type" | "select" | "hover" | "scroll" | "press" | "upload" | "drag" | "dialog"
    ) {
        return Err(Fault::new("invalid_params", "unsupported action"));
    }
    if r.value.as_ref().is_some_and(|v| v.len() > 65536)
        || r.files.len() > 16
        || r.options.modifiers.unwrap_or(0) > 15
        || r.options.click_count.unwrap_or(1) > 3
        || r.options.click_count == Some(0)
        || r.options
            .amount
            .is_some_and(|n| !n.is_finite() || n <= 0. || n > 20.)
        || r.options
            .prompt_text
            .as_ref()
            .is_some_and(|v| v.len() > 8192)
    {
        return Err(Fault::new("invalid_params", "action limits"));
    }
    if r.action == "press" {
        key(r.keys.as_deref().unwrap_or(""))?;
    }
    if r.action == "type"
        && r.options.key_events
        && r.value.as_deref().unwrap_or("").chars().count() > 2048
    {
        return Err(Fault::new("invalid_params", "key event limit"));
    }
    Ok(())
}
impl Manager {
    pub async fn prepare_action(
        &self,
        run: &str,
        sid: &str,
        tid: &str,
        r: ActionRequest,
    ) -> Result<Value> {
        request_valid(&r)?;
        let _lease = self.lease(run, sid, tid, false).await?;
        let mut ctx = self.observation_context(run, sid, tid).await?;
        ctx.session.owner_run = run.into();
        let info = ctx
            .client
            .send_with_timeout(
                Method::TargetGetTargetInfo,
                json!({"targetId":ctx.tab.target_id}),
                None,
                Duration::from_millis(500),
            )
            .await?;
        self.document_allowed(
            &ctx.session,
            info["targetInfo"]["url"].as_str().unwrap_or(""),
            !matches!(r.action.as_str(), "scroll" | "hover" | "dialog"),
        )?;
        let dialog = self
            .observation
            .tracker(&ctx)
            .state
            .lock()
            .unwrap()
            .dialog
            .clone();
        if dialog.is_some() && r.action != "dialog" {
            return Err(Fault::new(
                "dialog_open",
                self.observation
                    .redactor
                    .redact_json(&json!(dialog))
                    .to_string(),
            ));
        }
        if self
            .observation
            .tracker(&ctx)
            .state
            .lock()
            .unwrap()
            .file_chooser
            && r.action != "upload"
        {
            return Err(Fault::new("blocked", "file_chooser"));
        }
        let metadata = if let Some(reference) = &r.reference {
            let t = self.observation.target(&ctx, &r, reference).await?;
            self.document_allowed(&ctx.session, &t.frame.url, true)
                .map_err(|_| Fault::new("denied", "origin_not_writable"))?;
            t.metadata.clone()
        } else if matches!(r.action.as_str(), "scroll" | "press" | "dialog") {
            json!({})
        } else {
            return Err(Fault::new("invalid_params", "ref required"));
        };
        let (class, effect, risk) = classify(
            &r,
            &metadata,
            ctx.session.profile != "ephemeral",
            ctx.session.listen,
        );
        let plan_id = uuid::Uuid::new_v4().to_string();
        let expires = tokio::time::Instant::now() + Duration::from_secs(60);
        let mut plans = self.observation.actions.plans.lock().unwrap();
        plans.retain(|_, p| p.expires > tokio::time::Instant::now());
        if plans.len() >= 128 {
            return Err(Fault::new("capacity_exceeded", "action plans"));
        }
        plans.insert(
            plan_id.clone(),
            Plan {
                run: run.into(),
                sid: sid.into(),
                tid: tid.into(),
                request: r,
                class: class.into(),
                expires,
            },
        );
        Ok(self.observation.redactor.redact_json(&json!({"plan_id":plan_id,"action_class":class,"effect":effect,"risk":risk,"origin":ctx.tab.url,"name":metadata["name"],"persistent":ctx.session.profile!="ephemeral"})))
    }
    pub async fn perform_action(
        &self,
        run: &str,
        sid: &str,
        tid: &str,
        plan_id: &str,
        op_key: &str,
    ) -> Result<Value> {
        let plan = self
            .observation
            .actions
            .plans
            .lock()
            .unwrap()
            .get(plan_id)
            .cloned()
            .ok_or_else(|| Fault::new("unknown_plan", "prepare again"))?;
        if plan.run != run
            || plan.sid != sid
            || plan.tid != tid
            || plan.expires <= tokio::time::Instant::now()
        {
            return Err(Fault::new("denied", "action plan owner or expiry"));
        }
        let r = &plan.request;
        let _lease = self.lease(run, sid, tid, false).await?;
        let mut ctx = self.observation_context(run, sid, tid).await?;
        ctx.session.owner_run = run.into();
        let info = ctx
            .client
            .send_with_timeout(
                Method::TargetGetTargetInfo,
                json!({"targetId":ctx.tab.target_id}),
                None,
                Duration::from_millis(500),
            )
            .await?;
        self.document_allowed(
            &ctx.session,
            info["targetInfo"]["url"].as_str().unwrap_or(""),
            !matches!(r.action.as_str(), "scroll" | "hover" | "dialog"),
        )?;
        let tracker = self.observation.tracker(&ctx);
        let dialog = tracker.state.lock().unwrap().dialog.clone();
        if dialog.is_some() && r.action != "dialog" {
            return Err(Fault::new(
                "dialog_open",
                self.observation
                    .redactor
                    .redact_json(&json!(dialog))
                    .to_string(),
            ));
        }
        if tracker.state.lock().unwrap().file_chooser && r.action != "upload" {
            return Err(Fault::new("blocked", "file_chooser"));
        }
        let mut target = if let Some(reference) = &r.reference {
            Some(self.observation.target(&ctx, r, reference).await?)
        } else {
            None
        };
        if let Some(t) = &target {
            self.document_allowed(&ctx.session, &t.frame.url, true)
                .map_err(|_| Fault::new("denied", "origin_not_writable"))?;
        } else if r.action != "dialog" {
            let frames = self.observation.frames(&ctx).await?;
            self.document_allowed(&ctx.session, &frames[0].url, r.action != "scroll")?;
        }
        let meta = target
            .as_ref()
            .map(|t| t.metadata.clone())
            .unwrap_or(json!({}));
        let (class, _, _) = classify(
            r,
            &meta,
            ctx.session.profile != "ephemeral",
            ctx.session.listen,
        );
        if class != plan.class {
            return Err(Fault::new("authorization_changed", "action class changed"));
        }
        let point = if let Some(t) = target.as_mut() {
            Some(actionable(&ctx, t, &r.action).await?)
        } else {
            None
        };
        let mut destination = if r.action == "drag" {
            Some(
                self.observation
                    .target(
                        &ctx,
                        r,
                        r.target_ref
                            .as_deref()
                            .ok_or_else(|| Fault::new("invalid_params", "target_ref required"))?,
                    )
                    .await?,
            )
        } else {
            None
        };
        let destination_point = if let Some(t) = destination.as_mut() {
            self.document_allowed(&ctx.session, &t.frame.url, true)
                .map_err(|_| Fault::new("denied", "origin_not_writable"))?;
            Some(actionable(&ctx, t, "drag").await?)
        } else {
            None
        };
        let staged = if r.action == "upload" {
            self.observation.upload_paths(run, sid, &r.files)?
        } else {
            vec![]
        };
        self.observation
            .actions
            .plans
            .lock()
            .unwrap()
            .remove(plan_id)
            .ok_or_else(|| Fault::new("unknown_plan", "plan already consumed"))?;
        let started = tokio::time::Instant::now();
        let _scope =
            self.observation
                .action_scope(&ctx, true, matches!(class, "navigate_link" | "commit"));
        let key = op_key.to_owned();
        self.db
            .call(move |c| {
                c.execute(
                    "UPDATE web_ops SET state='dispatched',updated_at=?1 WHERE op_key=?2",
                    rusqlite::params![crate::sessions::now(), key],
                )?;
                Ok(())
            })
            .await?;
        self.db.event("action_dispatch",json!({"run_id":run,"session_id":sid,"tab_id":tid,"op_key":op_key,"action":r.action,"class":class})).await?;
        let mut raw = ctx.client.subscribe_all();
        let c = &ctx.client;
        let root = ctx.tab.cdp_session.as_deref().unwrap();
        let options = &r.options;
        let mut result = json!({"result":"ok"});
        let dispatch_input = async {
            match r.action.as_str() {
                "click" | "hover" => {
                    let (x, y) = point.unwrap();
                    mouse(c, root, "mouseMoved", x, y, options, 0).await?;
                    if r.action == "click" {
                        mouse(c, root, "mousePressed", x, y, options, 1).await?;
                        mouse(c, root, "mouseReleased", x, y, options, 0).await?;
                    }
                }
                "type" => {
                    let t = target.as_ref().unwrap();
                    fill(
                        c,
                        t,
                        r.value
                            .as_deref()
                            .ok_or_else(|| Fault::new("invalid_params", "value required"))?,
                        options.key_events,
                    )
                    .await?;
                }
                "select" => {
                    let t = target.as_ref().unwrap();
                    let v=call(c,&t.frame.session,&t.object,"function(value){if(this.tagName!=='SELECT')return false;const o=[...this.options].find(o=>o.value===value||o.label===value);if(!o||o.disabled)return false;this.value=o.value;this.dispatchEvent(new Event('input',{bubbles:true}));this.dispatchEvent(new Event('change',{bubbles:true}));return true}",vec![json!({"value":r.value})]).await?;
                    if v != true {
                        return Err(Fault::new("not_actionable", "option unavailable"));
                    }
                }
                "press" => {
                    if let Some(t) = &target {
                        c.send(
                            Method::DOMFocus,
                            json!({"backendNodeId":t.key.backend_node_id}),
                            Some(&t.frame.session),
                        )
                        .await?;
                    }
                    press(c, root, r.keys.as_deref().unwrap()).await?;
                }
                "scroll" => {
                    if target.is_none() {
                        let metrics = c
                            .send(Method::PageGetLayoutMetrics, json!({}), Some(root))
                            .await?;
                        let h = metrics["cssLayoutViewport"]["clientHeight"]
                            .as_f64()
                            .unwrap_or(900.);
                        let w = metrics["cssLayoutViewport"]["clientWidth"]
                            .as_f64()
                            .unwrap_or(1280.);
                        let n = options.amount.unwrap_or(1.);
                        let (dx, dy) = match options.direction.as_deref().unwrap_or("down") {
                            "up" => (0., -h * n),
                            "left" => (-w * n, 0.),
                            "right" => (w * n, 0.),
                            "down" => (0., h * n),
                            _ => return Err(Fault::new("invalid_params", "scroll direction")),
                        };
                        c.send(
                            Method::InputDispatchMouseEvent,
                            json!({"type":"mouseWheel","x":w/2.,"y":h/2.,"deltaX":dx,"deltaY":dy}),
                            Some(root),
                        )
                        .await?;
                    }
                }
                "upload" => {
                    let t = target.as_ref().unwrap();
                    if t.metadata["tag"] != "input" || t.metadata["type"] != "file" {
                        return Err(Fault::new("not_actionable", "file input required"));
                    }
                    c.send(
                        Method::DOMSetFileInputFiles,
                        json!({"backendNodeId":t.key.backend_node_id,"files":staged}),
                        Some(&t.frame.session),
                    )
                    .await?;
                    tracker.state.lock().unwrap().file_chooser = false;
                }
                "drag" => {
                    let (x, y) = point.unwrap();
                    let (tx, ty) = destination_point.unwrap();
                    if options.html5 {
                        c.send(
                            Method::InputSetInterceptDrags,
                            json!({"enabled":true}),
                            Some(root),
                        )
                        .await?;
                    }
                    mouse(c, root, "mouseMoved", x, y, options, 0).await?;
                    mouse(c, root, "mousePressed", x, y, options, 1).await?;
                    for n in 1..=5 {
                        mouse(
                            c,
                            root,
                            "mouseMoved",
                            x + (tx - x) * n as f64 / 5.,
                            y + (ty - y) * n as f64 / 5.,
                            options,
                            1,
                        )
                        .await?;
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    if options.html5 {
                        let data = tokio::time::timeout(Duration::from_secs(1), async {
                            loop {
                                let e = raw
                                    .recv()
                                    .await
                                    .map_err(|_| Fault::new("uncertain", "drag stream lost"))?;
                                if e.method == "Input.dragIntercepted" {
                                    break Ok::<_, Fault>(e.params["data"].clone());
                                }
                            }
                        })
                        .await
                        .map_err(|_| Fault::new("not_actionable", "HTML5 drag data absent"))??;
                        if data["files"].as_array().is_some_and(|a| !a.is_empty()) {
                            return Err(Fault::new("denied", "native drag files"));
                        }
                        for kind in ["dragEnter", "dragOver", "drop"] {
                            c.send(
                                Method::InputDispatchDragEvent,
                                json!({"type":kind,"x":tx,"y":ty,"data":data}),
                                Some(root),
                            )
                            .await?;
                        }
                    }
                    mouse(c, root, "mouseReleased", tx, ty, options, 0).await?;
                    if options.html5 {
                        c.send(
                            Method::InputSetInterceptDrags,
                            json!({"enabled":false}),
                            Some(root),
                        )
                        .await?;
                    }
                }
                "dialog" => {
                    let mut p = json!({"accept":options.accept.unwrap_or(false)});
                    if let Some(text) = &options.prompt_text {
                        p["promptText"] = json!(text)
                    }
                    c.send(Method::PageHandleJavaScriptDialog, p, Some(root))
                        .await?;
                }
                _ => unreachable!(),
            }
            Ok::<_, Fault>(())
        };
        // A page-opened confirm/prompt suspends the renderer, so input dispatch does not
        // return until the dialog is handled. That dialog is itself the receipt.
        let navigation = matches!(class, "navigate_link" | "commit");
        let dispatch = if r.action == "dialog" {
            dispatch_input.await
        } else {
            tokio::select! {
                result = dispatch_input => result,
                _ = pending_dialog(&tracker, navigation) => Ok(()),
            }
        };
        if r.action == "drag" {
            let _ = c
                .send_with_timeout(
                    Method::InputSetInterceptDrags,
                    json!({"enabled":false}),
                    Some(root),
                    Duration::from_millis(500),
                )
                .await;
        }
        if let Err(error) = dispatch {
            if matches!(error.kind.as_str(), "cdp_error" | "cdp_timeout") {
                return Err(Fault::new("uncertain", "input receipt unavailable"));
            }
            return Err(error);
        }
        let wait = if blocking_dialog(&tracker, navigation) {
            Err(Fault::new("dialog_open", "page suspended by dialog"))
        } else {
            tokio::time::sleep(Duration::from_millis(250)).await;
            self.observation
                .wait(
                    &ctx,
                    WaitCriteria {
                        timeout_ms: 5000,
                        ..Default::default()
                    },
                )
                .await
        };
        let tabs = self.adopt_action_popups(&ctx).await?;
        let final_frames = self.observation.frames(&ctx).await.ok();
        let url = final_frames
            .as_ref()
            .and_then(|f| f.first())
            .map(|f| f.url.clone())
            .unwrap_or_else(|| ctx.tab.url.clone());
        let dialog = tracker.state.lock().unwrap().dialog.clone();
        result["alerts"] = json!(tracker.state.lock().unwrap().action_alerts.clone());
        result["untrusted"] = json!(true);
        result["file_chooser"] = json!(tracker.state.lock().unwrap().file_chooser);
        result["settled"] = json!(wait.as_ref().is_ok_and(|w| w.status == "ready"));
        result["wait"] = wait
            .map(|w| json!(w))
            .unwrap_or(json!({"status":"timeout"}));
        result["navigated"] = json!(url != ctx.tab.url);
        result["url"] = json!(url);
        result["new_tabs"] = tabs["tabs"].clone();
        result["popup_blocked"] = tabs["blocked"].clone();
        result["dialog"] = dialog.clone().unwrap_or(Value::Null);
        if dialog.is_some() {
            result["result"] = json!("dialog_open");
        }
        result["action_class"] = json!(class);
        result["elapsed_ms"] = json!(started.elapsed().as_millis());
        result["dom_changed"] = json!(true);
        result["suggest_snapshot"] = json!(true);
        while let Ok(event) = raw.try_recv() {
            if event.method == "Browser.downloadWillBegin" {
                result["download"] = json!({"state":"download_blocked","suggested_filename":event.params["suggestedFilename"],"origin":event.params["url"]});
            }
        }
        Ok(self.observation.redactor.redact_json(&result))
    }
    // The trusted boundary keeps run/session/tab and credential identity explicit.
    #[allow(clippy::too_many_arguments)]
    pub async fn check_credential_target(
        &self,
        run: &str,
        sid: &str,
        tid: &str,
        snapshot: &str,
        reference: &str,
        secret_ref: &str,
        expected_origin: &str,
    ) -> Result<Value> {
        let _lease = self.lease(run, sid, tid, false).await?;
        let mut ctx = self.observation_context(run, sid, tid).await?;
        ctx.session.owner_run = run.into();
        let r = ActionRequest {
            action: "type".into(),
            snapshot_id: snapshot.into(),
            reference: Some(reference.into()),
            value: None,
            keys: None,
            target_ref: None,
            files: vec![],
            options: Default::default(),
        };
        let target = self
            .credential_target(&ctx, &r, secret_ref, expected_origin)
            .await?;
        Ok(
            json!({"allowed":true,"key":target.key,"origin":expected_origin,"action_class":"credential","risk":"high"}),
        )
    }
    async fn credential_target(
        &self,
        ctx: &Context,
        r: &ActionRequest,
        secret_ref: &str,
        expected: &str,
    ) -> Result<Target> {
        if !ctx.session.credentials.iter().any(|s| s == secret_ref) {
            return Err(Fault::new("denied", "secret_not_bound"));
        }
        let origin = exact_https(expected)?;
        if origin != expected {
            return Err(Fault::new("denied", "expected_origin_must_be_canonical"));
        }
        let frames = self.observation.frames(ctx).await?;
        let top = frames
            .first()
            .ok_or_else(|| Fault::new("denied", "unknown top frame"))?;
        if exact_https(&top.url)? != expected || top.security_origin != expected {
            return Err(Fault::new("denied", "credential_origin_mismatch"));
        }
        self.document_allowed(&ctx.session, &top.url, true)?;
        let mut t = self
            .observation
            .target(ctx, r, r.reference.as_deref().unwrap())
            .await?;
        let embedded = self
            .credential_frame_allowed(&ctx.session, &t.frame.security_origin)
            .await;
        if (exact_https(&t.frame.url)? != expected || t.frame.security_origin != expected)
            && (!embedded || exact_https(&t.frame.url)? != t.frame.security_origin)
        {
            return Err(Fault::new("denied", "credential_frame_mismatch"));
        }
        self.document_allowed(&ctx.session, &t.frame.url, true)?;
        if t.metadata["tag"] != "input" || t.metadata["type"] != "password" {
            return Err(Fault::new("denied", "password_input_required"));
        }
        actionable(ctx, &mut t, "type").await?;
        Ok(t)
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn fill_secret(
        &self,
        run: &str,
        sid: &str,
        tid: &str,
        snapshot: &str,
        reference: &str,
        secret_ref: &str,
        expected: &str,
        value: excalibur_vault::SecretValue,
    ) -> Result<Value> {
        let _lease = self.lease(run, sid, tid, false).await?;
        let mut ctx = self.observation_context(run, sid, tid).await?;
        ctx.session.owner_run = run.into();
        let r = ActionRequest {
            action: "type".into(),
            snapshot_id: snapshot.into(),
            reference: Some(reference.into()),
            value: None,
            keys: None,
            target_ref: None,
            files: vec![],
            options: Default::default(),
        };
        let target = self
            .credential_target(&ctx, &r, secret_ref, expected)
            .await?;
        self.observation.redactor.register_secret(&value);
        let _scope = self.observation.action_scope(&ctx, true, false);
        self.db.event("credential_dispatch",json!({"run_id":run,"session_id":sid,"tab_id":tid,"secret_ref":secret_ref,"expected_origin":expected})).await?;
        let secret = zeroize::Zeroizing::new(
            value
                .expose(|b| String::from_utf8(b.to_vec()))
                .map_err(|_| Fault::new("denied", "credential encoding"))?,
        );
        self.observation
            .actions
            .filled
            .lock()
            .unwrap()
            .insert(target.key.clone());
        let frames = self.observation.frames(&ctx).await?;
        self.observation.actions.vision.lock().unwrap().insert(
            tid.into(),
            (frames[0].loader.clone(), tokio::time::Instant::now()),
        );
        focus_select(&ctx.client, &target).await?;
        let current = self
            .credential_target(&ctx, &r, secret_ref, expected)
            .await?;
        let focused = call(
            &ctx.client,
            &current.frame.session,
            &current.object,
            "function(){return this.getRootNode().activeElement===this}",
            vec![],
        )
        .await?;
        if focused != true {
            return Err(Fault::new("denied", "credential focus changed"));
        }
        ctx.client
            .send(
                Method::InputInsertText,
                json!({"text":secret.as_str()}),
                Some(&current.frame.session),
            )
            .await
            .map_err(|_| Fault::new("uncertain", "credential receipt unavailable"))?;
        self.observation
            .actions
            .filled
            .lock()
            .unwrap()
            .insert(target.key.clone());
        let frames = self.observation.frames(&ctx).await?;
        self.observation.actions.vision.lock().unwrap().insert(
            tid.into(),
            (frames[0].loader.clone(), tokio::time::Instant::now()),
        );
        Ok(json!({"filled":true}))
    }
}
/// True when a dialog is open that the action boundary does not accept automatically.
fn blocking_dialog(tracker: &TabObserver, navigation: bool) -> bool {
    tracker
        .state
        .lock()
        .unwrap()
        .dialog
        .as_ref()
        .is_some_and(|d| d["type"] != "alert" && !(d["type"] == "beforeunload" && navigation))
}
async fn pending_dialog(tracker: &TabObserver, navigation: bool) {
    while !blocking_dialog(tracker, navigation) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
pub(crate) fn exact_https(value: &str) -> Result<String> {
    let url =
        reqwest::Url::parse(value).map_err(|_| Fault::new("denied", "invalid credential URL"))?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(Fault::new("denied", "credential HTTPS origin required"));
    }
    Ok(url.origin().ascii_serialization())
}
async fn actionable(ctx: &Context, t: &mut Target, action: &str) -> Result<(f64, f64)> {
    let delays = [0, 20, 100, 100, 500];
    let mut covered = String::new();
    for (n, delay) in delays.into_iter().enumerate() {
        tokio::time::sleep(Duration::from_millis(delay)).await;
        let _ = ctx
            .client
            .send_with_timeout(
                Method::DOMScrollIntoViewIfNeeded,
                json!({"backendNodeId":t.key.backend_node_id}),
                Some(&t.frame.session),
                Duration::from_millis(500),
            )
            .await;
        if n > 0 {
            let _=call(&ctx.client,&t.frame.session,&t.object,"function(block){this.scrollIntoView({block,inline:'center',behavior:'instant'});return true}",vec![json!({"value":(["center","end","start"][n%3])})]).await;
        }
        let m = call(
            &ctx.client,
            &t.frame.session,
            &t.object,
            include_str!("actions/target.js"),
            vec![],
        )
        .await?;
        if m["connected"] != true {
            return Err(Fault::new("stale_ref", "node_detached"));
        }
        if m["visible"] != true
            || m["enabled"] != true
            || (action == "type" && m["editable"] != true)
        {
            continue;
        }
        let stable=call(&ctx.client,&t.frame.session,&t.object,"async function(){const a=this.getBoundingClientRect();await new Promise(requestAnimationFrame);await new Promise(requestAnimationFrame);const b=this.getBoundingClientRect();return Math.abs(a.x-b.x)<0.5&&Math.abs(a.y-b.y)<0.5&&Math.abs(a.width-b.width)<0.5&&Math.abs(a.height-b.height)<0.5}",vec![]).await.unwrap_or(json!(false));
        if stable != true {
            continue;
        }
        let x = m["x"].as_f64().unwrap_or(0.) + m["w"].as_f64().unwrap_or(0.) / 2.;
        let y = m["y"].as_f64().unwrap_or(0.) + m["h"].as_f64().unwrap_or(0.) / 2.;
        let global = (x + t.frame.offset.0, y + t.frame.offset.1);
        // Hit testing uses native backend identity, then a composed containment check that
        // crosses user-agent and author shadow roots in the isolated world.
        let hit = ctx
            .client
            .send_with_timeout(
                Method::DOMGetNodeForLocation,
                json!({"x":global.0.round(),"y":global.1.round(),"includeUserAgentShadowDOM":true}),
                ctx.tab.cdp_session.as_deref(),
                Duration::from_millis(500),
            )
            .await;
        let Ok(hit) = hit else { continue };
        let hit = if t.frame.session != ctx.tab.cdp_session.as_deref().unwrap_or("") {
            let owner = ctx
                .client
                .send(
                    Method::DOMGetFrameOwner,
                    json!({"frameId":t.frame.id}),
                    ctx.tab.cdp_session.as_deref(),
                )
                .await?;
            if owner["backendNodeId"] != hit["backendNodeId"] {
                covered = "outside frame overlay".into();
                continue;
            }
            ctx.client
                .send(
                    Method::DOMGetNodeForLocation,
                    json!({"x":x.round(),"y":y.round(),"includeUserAgentShadowDOM":true}),
                    Some(&t.frame.session),
                )
                .await?
        } else {
            hit
        };
        let hit_object = ctx
            .client
            .send(
                Method::DOMResolveNode,
                json!({"backendNodeId":hit["backendNodeId"],"executionContextId":t.world}),
                Some(&t.frame.session),
            )
            .await?;
        if let Some(object) = hit_object["object"]["objectId"].as_str() {
            let receives = call(
                &ctx.client,
                &t.frame.session,
                &t.object,
                "function(hit){for(let n=hit;n;n=n.parentNode||(n instanceof ShadowRoot?n.host:null)){if(n===this)return true}return false}",
                vec![json!({"objectId":object})],
            )
            .await?;
            if receives == true {
                t.metadata = m;
                return Ok(global);
            }
            covered=call(&ctx.client,&t.frame.session,object,"function(){return (this.getAttribute('role')||this.tagName)+' '+(this.getAttribute('aria-label')||this.innerText||'').slice(0,80)}",vec![]).await?.as_str().unwrap_or("overlay").into();
            let _ = ctx
                .client
                .send(
                    Method::RuntimeReleaseObject,
                    json!({"objectId":object}),
                    Some(&t.frame.session),
                )
                .await;
        }
    }
    Err(Fault::new(
        "not_actionable",
        json!({"covered_by":covered,"reason":"hidden, disabled, moving or covered"}).to_string(),
    ))
}
async fn mouse(
    c: &Client,
    sid: &str,
    kind: &str,
    x: f64,
    y: f64,
    o: &ActionOptions,
    buttons: u32,
) -> Result<()> {
    let button = o.button.as_deref().unwrap_or("left");
    if !matches!(button, "left" | "middle" | "right") {
        return Err(Fault::new("invalid_params", "mouse button"));
    }
    c.send(Method::InputDispatchMouseEvent,json!({"type":kind,"x":x,"y":y,"button":button,"buttons":buttons,"clickCount":o.click_count.unwrap_or(1),"modifiers":o.modifiers.unwrap_or(0)}),Some(sid)).await?;
    Ok(())
}
async fn focus_select(c: &Client, t: &Target) -> Result<()> {
    c.send(
        Method::DOMFocus,
        json!({"backendNodeId":t.key.backend_node_id}),
        Some(&t.frame.session),
    )
    .await?;
    c.send(
        Method::InputDispatchKeyEvent,
        json!({"type":"keyDown","key":"a","code":"KeyA","modifiers":2,"commands":["selectAll"]}),
        Some(&t.frame.session),
    )
    .await?;
    c.send(
        Method::InputDispatchKeyEvent,
        json!({"type":"keyUp","key":"a","code":"KeyA","modifiers":2}),
        Some(&t.frame.session),
    )
    .await?;
    Ok(())
}
async fn fill(c: &Client, t: &Target, value: &str, key_events: bool) -> Result<()> {
    focus_select(c, t).await?;
    if key_events {
        for ch in value.chars() {
            if ch == '\n' {
                c.send(
                    Method::InputInsertText,
                    json!({"text":"\n"}),
                    Some(&t.frame.session),
                )
                .await?;
            } else {
                c.send(
                    Method::InputDispatchKeyEvent,
                    json!({"type":"keyDown","key":ch.to_string(),"text":ch.to_string()}),
                    Some(&t.frame.session),
                )
                .await?;
                c.send(
                    Method::InputDispatchKeyEvent,
                    json!({"type":"keyUp","key":ch.to_string()}),
                    Some(&t.frame.session),
                )
                .await?;
            }
        }
    } else {
        c.send(
            Method::InputInsertText,
            json!({"text":value}),
            Some(&t.frame.session),
        )
        .await?;
    }
    Ok(())
}
fn key(keys: &str) -> Result<(String, String, u32, u32)> {
    let mut parts = keys.split('+').collect::<Vec<_>>();
    let k = parts.pop().unwrap_or("");
    let mut modifiers = 0;
    for p in parts {
        modifiers |= match p {
            "Control" | "Ctrl" => 2,
            "Alt" => 1,
            "Meta" => 4,
            "Shift" => 8,
            _ => return Err(Fault::new("invalid_params", "unknown modifier")),
        }
    }
    let (code, vk) = match k {
        "Enter" => ("Enter".into(), 13),
        "Tab" => ("Tab".into(), 9),
        "Escape" => ("Escape".into(), 27),
        "Backspace" => ("Backspace".into(), 8),
        "Delete" => ("Delete".into(), 46),
        "Insert" => ("Insert".into(), 45),
        "Home" => ("Home".into(), 36),
        "End" => ("End".into(), 35),
        "PageUp" => ("PageUp".into(), 33),
        "PageDown" => ("PageDown".into(), 34),
        "ArrowLeft" => ("ArrowLeft".into(), 37),
        "ArrowUp" => ("ArrowUp".into(), 38),
        "ArrowRight" => ("ArrowRight".into(), 39),
        "ArrowDown" => ("ArrowDown".into(), 40),
        "Space" => ("Space".into(), 32),
        _ if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => {
            let ch = k.as_bytes()[0].to_ascii_uppercase();
            (
                if ch.is_ascii_alphabetic() {
                    format!("Key{}", ch as char)
                } else {
                    format!("Digit{}", ch as char)
                },
                ch as u32,
            )
        }
        _ if k.starts_with('F') && k[1..].parse::<u32>().is_ok_and(|n| (1..=12).contains(&n)) => {
            (k.into(), 111 + k[1..].parse::<u32>().unwrap())
        }
        _ => return Err(Fault::new("invalid_params", "unknown key")),
    };
    Ok((
        if k == "Space" { " ".into() } else { k.into() },
        code,
        modifiers,
        vk,
    ))
}
async fn press(c: &Client, sid: &str, keys: &str) -> Result<()> {
    let (k, code, m, vk) = key(keys)?;
    for kind in ["keyDown", "keyUp"] {
        let text = if kind == "keyDown" && m & 7 == 0 {
            if k == "Enter" {
                "\r".into()
            } else if k.len() == 1 {
                k.clone()
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        c.send(Method::InputDispatchKeyEvent,json!({"type":kind,"key":k,"code":code,"modifiers":m,"windowsVirtualKeyCode":vk,"text":text}),Some(sid)).await?;
    }
    Ok(())
}
pub(super) async fn download_click(ctx: &Context, mut target: Target) -> Result<()> {
    let (x, y) = actionable(ctx, &mut target, "click").await?;
    let root = ctx.tab.cdp_session.as_deref().unwrap();
    let o = ActionOptions::default();
    mouse(&ctx.client, root, "mouseMoved", x, y, &o, 0).await?;
    mouse(&ctx.client, root, "mousePressed", x, y, &o, 1).await?;
    mouse(&ctx.client, root, "mouseReleased", x, y, &o, 0).await
}
#[cfg(test)]
mod tests {
    use super::*;
    fn request(action: &str) -> ActionRequest {
        serde_json::from_value(json!({"action":action,"snapshot_id":"s","ref":"e1"})).unwrap()
    }
    #[test]
    fn classes_follow_the_deterministic_table() {
        let link = json!({"tag":"a","href":"/next","name":"Next"});
        let submit = json!({"tag":"button","form":true,"name":"Save"});
        let lexicon = json!({"tag":"div","name":"Confirmar pedido"});
        let unknown = json!({"tag":"div","name":"?"});
        assert_eq!(
            classify(&request("hover"), &json!({}), true, false),
            ("observe", "pure", "low")
        );
        assert_eq!(
            classify(&request("type"), &json!({}), false, false),
            ("input", "idempotent", "low")
        );
        assert_eq!(
            classify(&request("select"), &json!({}), true, false),
            ("input", "idempotent", "medium")
        );
        assert_eq!(
            classify(&request("click"), &link, false, false),
            ("navigate_link", "non_idempotent", "low")
        );
        assert_eq!(
            classify(&request("click"), &link, true, false),
            ("navigate_link", "non_idempotent", "medium")
        );
        assert_eq!(
            classify(&request("click"), &submit, false, false).0,
            "commit"
        );
        assert_eq!(
            classify(&request("click"), &lexicon, true, false),
            ("commit", "non_idempotent", "high")
        );
        assert_eq!(
            classify(&request("click"), &unknown, true, false).0,
            "commit"
        );
        assert_eq!(
            classify(&request("click"), &unknown, false, false).0,
            "navigate_link"
        );
        assert_eq!(
            classify(&request("upload"), &json!({}), false, false).2,
            "high"
        );
        assert_eq!(
            classify(&request("drag"), &json!({}), false, false),
            ("drag", "non_idempotent", "medium")
        );
        let mut enter = request("press");
        enter.keys = Some("Enter".into());
        assert_eq!(classify(&enter, &json!({}), true, true).0, "commit");
        let mut accept = request("dialog");
        accept.options.accept = Some(true);
        assert_eq!(classify(&accept, &json!({}), true, false).0, "commit");
        assert_eq!(
            classify(&accept, &json!({}), false, false).1,
            "non_idempotent"
        );
    }
    #[test]
    fn requests_and_keys_are_bounded() {
        let mut r = request("press");
        r.keys = Some("Control+Enter".into());
        assert!(request_valid(&r).is_ok());
        assert_eq!(key("Control+Enter").unwrap().2, 2);
        assert_eq!(key("Shift+F5").unwrap().1, "F5");
        assert!(key("Hyper+Enter").is_err());
        assert!(key("javascript:alert(1)").is_err());
        let mut r = request("click");
        r.options.click_count = Some(0);
        assert!(request_valid(&r).is_err());
        assert!(request_valid(&request("evaluate")).is_err());
        let mut r = request("upload");
        r.files = vec!["x".into(); 17];
        assert!(request_valid(&r).is_err());
    }
    #[test]
    fn credential_origins_are_exact_https_without_userinfo() {
        assert_eq!(
            exact_https("https://one.test/login?x=1").unwrap(),
            "https://one.test"
        );
        assert_eq!(
            exact_https("https://one.test:8443/").unwrap(),
            "https://one.test:8443"
        );
        for bad in [
            "http://one.test/",
            "https://user:pass@one.test/",
            "https://one.test:pass@localhost:8080/",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(exact_https(bad).is_err(), "{bad}");
        }
        assert_ne!(
            exact_https("https://sub.one.test/").unwrap(),
            "https://one.test"
        );
    }
}
