use super::*;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElementKey {
    pub target_id: String,
    pub session_id: String,
    pub frame_id: String,
    pub loader_id: String,
    pub backend_node_id: u64,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}
impl Rect {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }
    fn intersects(&self, b: Rect) -> bool {
        self.x + self.w >= b.x
            && self.x <= b.x + b.w
            && self.y + self.h >= b.y
            && self.y <= b.y + b.h
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TreeNode {
    pub index: usize,
    pub parent: Option<usize>,
    pub tag: String,
    pub attributes: BTreeMap<String, String>,
    pub text: String,
    pub role: String,
    pub name: String,
    pub properties: Value,
    pub bbox: Rect,
    pub paint_order: i64,
    pub styles: BTreeMap<String, String>,
    pub frame: String,
    pub visible: bool,
    pub enabled: bool,
    pub in_viewport: bool,
    pub why: Vec<String>,
    pub value: Option<String>,
    pub reference: Option<String>,
    #[serde(skip)]
    pub key: Option<ElementKey>,
    #[serde(skip)]
    pub sensitive: bool,
    #[serde(skip)]
    pub fingerprint: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Element {
    #[serde(rename = "ref")]
    pub reference: String,
    pub role: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub bbox: Rect,
    pub visible: bool,
    pub enabled: bool,
    pub in_viewport: bool,
    pub frame: String,
    pub why: Vec<String>,
    pub value: Option<String>,
    pub new: bool,
}
pub(crate) const STYLES: &[&str] = &[
    "display",
    "visibility",
    "opacity",
    "pointer-events",
    "cursor",
    "overflow-x",
    "overflow-y",
    "position",
    "content-visibility",
];
fn string(strings: &[Value], value: &Value) -> String {
    value
        .as_u64()
        .and_then(|n| strings.get(n as usize))
        .and_then(Value::as_str)
        .unwrap_or("")
        .into()
}
fn rare(data: &Value, index: usize) -> Option<&Value> {
    let indices = data["index"].as_array()?;
    let n = indices
        .iter()
        .position(|v| v.as_u64() == Some(index as u64))?;
    data["value"].as_array()?.get(n)
}
fn rare_bool(data: &Value, index: usize) -> bool {
    data["index"]
        .as_array()
        .is_some_and(|a| a.iter().any(|v| v.as_u64() == Some(index as u64)))
}
fn bounded(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}
fn fallback_role(tag: &str, attrs: &BTreeMap<String, String>) -> String {
    if let Some(role) = attrs.get("role") {
        return role.clone();
    }
    match tag {
        "button" | "summary" => "button",
        "a" => "link",
        "textarea" => "textbox",
        "select" => "combobox",
        "input" => match attrs.get("type").map(String::as_str).unwrap_or("text") {
            "checkbox" => "checkbox",
            "radio" => "radio",
            "range" => "slider",
            "number" => "spinbutton",
            "button" | "submit" | "reset" => "button",
            "search" => "searchbox",
            _ => "textbox",
        },
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "heading",
        "main" => "main",
        "nav" => "navigation",
        "header" => "banner",
        "form" => "form",
        "dialog" => "dialog",
        _ => "generic",
    }
    .into()
}
pub(crate) fn fingerprint(role: &str, name: &str, tag: &str) -> String {
    format!("{:x}", Sha256::digest(json!([role, name, tag]).to_string()))
}
pub(crate) fn normalize(
    captures: Vec<(Frame, Value, Value)>,
    frames: &[Frame],
    viewport: Rect,
) -> Vec<TreeNode> {
    let mut out: Vec<TreeNode> = vec![];
    let mut frame_roots = HashMap::new();
    let mut owners = vec![];
    for (frame, dom, ax) in captures {
        let strings = dom["strings"].as_array().cloned().unwrap_or_default();
        let lookup = ax["nodes"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v["backendDOMNodeId"].as_u64().map(|id| (id, v.clone())))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        let docs = dom["documents"].as_array().cloned().unwrap_or_default();
        let mut doc_roots = vec![];
        for doc in &docs {
            let frame_id = string(&strings, &doc["frameId"]);
            let context = frames.iter().find(|f| f.id == frame_id).unwrap_or(&frame);
            let nodes = &doc["nodes"];
            let count = nodes["nodeName"].as_array().map_or(0, Vec::len);
            let base = out.len();
            doc_roots.push(base);
            frame_roots.insert(frame_id.clone(), base);
            let mut layout = HashMap::new();
            if let Some(indices) = doc["layout"]["nodeIndex"].as_array() {
                for (row, index) in indices.iter().enumerate() {
                    if let Some(index) = index.as_u64() {
                        layout.insert(index as usize, row);
                    }
                }
            }
            for n in 0..count {
                let tag = string(&strings, &nodes["nodeName"][n]).to_ascii_lowercase();
                let mut attrs = BTreeMap::new();
                if let Some(a) = nodes["attributes"][n].as_array() {
                    for pair in a.as_chunks::<2>().0 {
                        let key = string(&strings, &pair[0]).to_ascii_lowercase();
                        if matches!(
                            key.as_str(),
                            "id" | "class"
                                | "name"
                                | "type"
                                | "href"
                                | "role"
                                | "alt"
                                | "title"
                                | "placeholder"
                                | "autocomplete"
                                | "tabindex"
                                | "contenteditable"
                                | "onclick"
                                | "disabled"
                        ) || key.starts_with("aria-")
                        {
                            attrs.insert(key, bounded(&string(&strings, &pair[1]), 1024));
                        }
                    }
                }
                let backend = nodes["backendNodeId"][n].as_u64().unwrap_or(0);
                let accessible = lookup.get(&backend);
                let mut props = serde_json::Map::new();
                if let Some(a) = accessible.and_then(|v| v["properties"].as_array()) {
                    for p in a {
                        if let Some(k) = p["name"].as_str() {
                            props.insert(k.into(), p["value"]["value"].clone());
                        }
                    }
                }
                let role = accessible
                    .and_then(|v| v["role"]["value"].as_str())
                    .map(str::to_owned)
                    .filter(|s| s != "none")
                    .unwrap_or_else(|| fallback_role(&tag, &attrs));
                let name = accessible
                    .and_then(|v| v["name"]["value"].as_str())
                    .unwrap_or_else(|| {
                        attrs
                            .get("aria-label")
                            .or(attrs.get("alt"))
                            .or(attrs.get("title"))
                            .map(String::as_str)
                            .unwrap_or("")
                    })
                    .to_owned();
                let mut styles = BTreeMap::new();
                let mut bbox = Rect::default();
                let mut paint = 0;
                if let Some(row) = layout.get(&n) {
                    let b = &doc["layout"]["bounds"][*row];
                    bbox = Rect {
                        x: b[0].as_f64().unwrap_or(0.)
                            - doc["scrollOffsetX"].as_f64().unwrap_or(0.)
                            + context.offset.0,
                        y: b[1].as_f64().unwrap_or(0.)
                            - doc["scrollOffsetY"].as_f64().unwrap_or(0.)
                            + context.offset.1,
                        w: b[2].as_f64().unwrap_or(0.),
                        h: b[3].as_f64().unwrap_or(0.),
                    };
                    paint = doc["layout"]["paintOrders"][*row].as_i64().unwrap_or(0);
                    for (i, key) in STYLES.iter().enumerate() {
                        styles.insert(
                            (*key).into(),
                            string(&strings, &doc["layout"]["styles"][*row][i]),
                        );
                    }
                }
                let mut why = vec![];
                if matches!(
                    role.as_str(),
                    "button"
                        | "link"
                        | "textbox"
                        | "searchbox"
                        | "combobox"
                        | "checkbox"
                        | "radio"
                        | "switch"
                        | "slider"
                        | "tab"
                        | "menuitem"
                        | "option"
                        | "spinbutton"
                        | "treeitem"
                ) {
                    why.push("ax_widget".into())
                }
                if ["focusable", "editable", "settable"].iter().any(|k| {
                    props
                        .get(*k)
                        .is_some_and(|v| v == true || v == "plaintext" || v == "richtext")
                }) {
                    why.push("ax_property".into())
                }
                if matches!(
                    tag.as_str(),
                    "button" | "input" | "select" | "textarea" | "summary" | "label"
                ) || (tag == "a" && attrs.contains_key("href"))
                    || attrs.contains_key("contenteditable")
                    || attrs
                        .get("tabindex")
                        .and_then(|s| s.parse::<i32>().ok())
                        .is_some_and(|n| n >= 0)
                    || attrs.contains_key("onclick")
                {
                    why.push("dom_control".into())
                }
                if rare_bool(&nodes["isClickable"], n) {
                    why.push("click_listener".into())
                }
                let parent = nodes["parentIndex"][n]
                    .as_i64()
                    .filter(|n| *n >= 0)
                    .map(|n| base + n as usize);
                let inherited_cursor = parent
                    .and_then(|p| out.get(p))
                    .and_then(|p| p.styles.get("cursor"))
                    .is_some_and(|s| s == "pointer");
                if styles.get("cursor").is_some_and(|s| s == "pointer") && !inherited_cursor {
                    why.push("pointer_cursor".into())
                }
                let value = rare(&nodes["inputValue"], n)
                    .or_else(|| rare(&nodes["textValue"], n))
                    .map(|v| string(&strings, v))
                    .or_else(|| {
                        accessible
                            .and_then(|v| v["value"]["value"].as_str())
                            .map(str::to_owned)
                    });
                let sensitive = attrs
                    .get("type")
                    .is_some_and(|s| s.eq_ignore_ascii_case("password"))
                    || attrs.get("autocomplete").is_some_and(|s| {
                        s.split_whitespace()
                            .any(|v| v == "one-time-code" || v.starts_with("cc-"))
                    });
                let text = if matches!(tag.as_str(), "script" | "style" | "noscript")
                    || nodes["parentIndex"][n]
                        .as_i64()
                        .filter(|i| *i >= 0)
                        .is_some_and(|i| {
                            matches!(
                                string(&strings, &nodes["nodeName"][i as usize])
                                    .to_ascii_lowercase()
                                    .as_str(),
                                "script" | "style" | "noscript"
                            )
                        }) {
                    String::new()
                } else {
                    bounded(&string(&strings, &nodes["nodeValue"][n]), 8192)
                };
                let enabled = props.get("disabled") != Some(&json!(true))
                    && !attrs.contains_key("disabled")
                    && attrs.get("aria-disabled").is_none_or(|s| s != "true");
                let visible = bbox.w > 0.
                    && bbox.h > 0.
                    && styles
                        .get("visibility")
                        .is_none_or(|s| s != "hidden" && s != "collapse")
                    && styles.get("display").is_none_or(|s| s != "none")
                    && styles
                        .get("opacity")
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(1.)
                        > 0.;
                if let Some(child) = rare(&nodes["contentDocumentIndex"], n).and_then(Value::as_u64)
                {
                    owners.push((out.len(), doc_roots.len() - 1, child as usize));
                }
                out.push(TreeNode {
                    index: out.len(),
                    parent,
                    tag: tag.clone(),
                    attributes: attrs,
                    text,
                    role: role.clone(),
                    name: bounded(&name, 1024),
                    properties: Value::Object(props),
                    bbox,
                    paint_order: paint,
                    styles,
                    frame: frame_id.clone(),
                    visible,
                    enabled,
                    in_viewport: bbox.intersects(viewport),
                    why,
                    value,
                    reference: None,
                    key: Some(ElementKey {
                        target_id: context.target.clone(),
                        session_id: context.session.clone(),
                        frame_id: frame_id.clone(),
                        loader_id: context.loader.clone(),
                        backend_node_id: backend,
                    }),
                    sensitive,
                    fingerprint: fingerprint(&role, &name, &tag),
                });
            }
        }
        for (owner, _doc, child) in owners.drain(..) {
            if let Some(root) = doc_roots.get(child)
                && let Some(node) = out.get_mut(*root)
            {
                node.parent = Some(owner)
            }
        }
    }
    // OOPIF roots are attached to their iframe owner using document/frame identities, never backend IDs across sessions.
    for frame in frames.iter().filter(|f| f.parent.is_some()) {
        if let Some(root) = frame_roots.get(&frame.id).copied()
            && out[root].parent.is_none()
        {
            let owner = out.iter().position(|n| {
                n.tag == "iframe"
                    && n.key
                        .as_ref()
                        .is_some_and(|k| k.frame_id == frame.parent.as_deref().unwrap_or(""))
                    && n.bbox.contains(frame.offset.0 + 1., frame.offset.1 + 1.)
            });
            out[root].parent = owner;
        }
    }
    for index in 0..out.len() {
        let mut parent = out[index].parent;
        let mut hidden = false;
        let mut disabled = false;
        for _ in 0..out.len() {
            let Some(p) = parent.and_then(|p| out.get(p)) else {
                break;
            };
            hidden |= p
                .styles
                .get("opacity")
                .and_then(|s| s.parse::<f64>().ok())
                .is_some_and(|n| n <= 0.)
                || p.styles
                    .get("visibility")
                    .is_some_and(|s| s == "hidden" || s == "collapse")
                || p.styles.get("display").is_some_and(|s| s == "none")
                || p.styles
                    .get("content-visibility")
                    .is_some_and(|s| s == "hidden");
            disabled |= p.tag == "fieldset" && p.attributes.contains_key("disabled");
            parent = p.parent;
        }
        out[index].visible &= !hidden;
        out[index].enabled &= !disabled;
    }
    let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    let mut large = vec![];
    for n in &out {
        if !n.visible || n.tag.starts_with('#') {
            continue;
        }
        let (x0, x1, y0, y1) = (
            (n.bbox.x / 100.).floor() as i64,
            ((n.bbox.x + n.bbox.w) / 100.).floor() as i64,
            (n.bbox.y / 100.).floor() as i64,
            ((n.bbox.y + n.bbox.h) / 100.).floor() as i64,
        );
        if (x1 - x0 + 1).saturating_mul(y1 - y0 + 1) > 400 {
            large.push(n.index);
            continue;
        }
        for x in x0..=x1 {
            for y in y0..=y1 {
                grid.entry((x, y)).or_default().push(n.index);
            }
        }
    }
    let mut hidden = HashSet::new();
    for n in &out {
        if !n.visible || n.why.is_empty() {
            continue;
        }
        let x = n.bbox.x + n.bbox.w / 2.;
        let y = n.bbox.y + n.bbox.h / 2.;
        let cell = ((x / 100.).floor() as i64, (y / 100.).floor() as i64);
        for other in grid.get(&cell).into_iter().flatten().chain(large.iter()) {
            let o = &out[*other];
            let mut anchor = Some(n.index);
            let mut threshold = None;
            for _ in 0..out.len() {
                let Some(a) = anchor.and_then(|i| out.get(i)) else {
                    break;
                };
                if a.frame == o.frame {
                    threshold = Some(a.paint_order);
                    break;
                }
                anchor = a.parent;
            }
            if threshold.is_some_and(|paint| o.paint_order > paint)
                && o.bbox.contains(x, y)
                && !ancestor(&out, n.index, o.index)
                && !ancestor(&out, o.index, n.index)
            {
                hidden.insert(n.index);
                break;
            }
        }
    }
    for index in hidden {
        out[index].visible = false;
    }
    out
}
fn ancestor(nodes: &[TreeNode], parent: usize, mut child: usize) -> bool {
    for _ in 0..nodes.len() {
        let Some(p) = nodes[child].parent else {
            return false;
        };
        if p == parent {
            return true;
        }
        if p >= nodes.len() {
            return false;
        }
        child = p;
    }
    false
}
impl Observation {
    pub(crate) async fn snapshot(
        &self,
        ctx: &Context,
        options: SnapshotOptions,
    ) -> Result<Snapshot> {
        if !matches!(options.dom.as_str(), "interactive" | "full")
            || !matches!(options.extent.as_str(), "viewport" | "fullpage")
        {
            return Err(Fault::new("invalid_params", "snapshot mode"));
        }
        ctx.client
            .send_with_timeout(
                Method::PageBringToFront,
                json!({}),
                ctx.tab.cdp_session.as_deref(),
                Duration::from_millis(500),
            )
            .await?;
        let tracker = self.tracker(ctx);
        {
            let state = tracker.state.lock().unwrap();
            if let Some(blocked) = state.dialog.as_ref().or(state.response_blocked.as_ref()) {
                return Err(Fault::new(
                    "blocked",
                    self.redactor.redact_json(blocked).to_string(),
                ));
            }
            if state.file_chooser {
                return Err(Fault::new("blocked", "file_chooser"));
            }
        }
        let current_frames = self.frames(ctx).await?;
        // Hold collection through cache admission so a queued listener cleanup cannot
        // disable Network between validating a ready result and starting capture.
        let _admission_network = self.initialize(ctx, &current_frames).await?;
        let loaders = current_frames
            .iter()
            .map(|f| (f.id.clone(), f.loader.clone()))
            .collect::<BTreeMap<_, _>>();
        let cached = if options.wait.is_none() && !tracker.state.lock().unwrap().navigation_pending
        {
            tracker
                .last_ready
                .lock()
                .unwrap()
                .clone()
                .filter(|(frames, _)| *frames == loaders)
                .map(|(_, r)| r)
        } else {
            None
        };
        let report = if let Some(report) = cached {
            report
        } else {
            self.wait(ctx, options.wait.clone().unwrap_or_default())
                .await?
        };
        if report.status == "blocked" {
            return Err(Fault::new("blocked", json!(report).to_string()));
        }
        let tracker = self.tracker(ctx);
        let frames = self.frames(ctx).await?;
        let _network = self.initialize(ctx, &frames).await?;
        let started = tokio::time::Instant::now();
        let deadline = started + Duration::from_secs(3);
        let metrics = ctx
            .client
            .send_with_timeout(
                Method::PageGetLayoutMetrics,
                json!({}),
                Some(&tracker.session),
                Duration::from_millis(500),
            )
            .await?;
        let view = &metrics["cssLayoutViewport"];
        let viewport = Rect {
            x: 0.,
            y: 0.,
            w: view["clientWidth"].as_f64().unwrap_or(1280.),
            h: view["clientHeight"].as_f64().unwrap_or(900.),
        };
        let mut tasks = tokio::task::JoinSet::new();
        let mut unique = HashSet::new();
        let mut pending = HashSet::new();
        for frame in &frames {
            let client = ctx.client.clone();
            let frame = frame.clone();
            pending.insert(format!("ax:{}", frame.id));
            tasks.spawn(async move {
                let result = client
                    .send(
                        Method::AccessibilityGetFullAXTree,
                        json!({"frameId":frame.id}),
                        Some(&frame.session),
                    )
                    .await
                    .map_err(Fault::from);
                (frame, 1u8, result)
            });
        }
        for frame in &frames {
            if !unique.insert(frame.session.clone()) {
                continue;
            }
            let client = ctx.client.clone();
            let frame = frame.clone();
            pending.insert(format!("dom:{}", frame.id));
            tasks.spawn(async move{let result=client.send(Method::DOMSnapshotCaptureSnapshot,json!({"computedStyles":STYLES,"includePaintOrder":true,"includeDOMRects":true}),Some(&frame.session)).await.map_err(Fault::from);(frame,0u8,result)});
        }
        let image_ctx = ctx.clone();
        let image_metrics = metrics.clone();
        let extent = options.extent.clone();
        let frame = frames[0].clone();
        pending.insert(format!("screenshot:{}", frame.id));
        tasks.spawn(async move {
            let result = capture_raw(
                &image_ctx,
                &frame.session,
                &image_metrics,
                &extent,
                viewport,
            )
            .await;
            (frame, 2u8, result)
        });
        let mut captured: HashMap<String, (Frame, Option<Value>, Vec<Value>)> = HashMap::new();
        let mut raw_image = None;
        let mut missing = vec![];
        while !tasks.is_empty() {
            match tokio::time::timeout_at(deadline, tasks.join_next()).await {
                Ok(Some(Ok((frame, kind, result)))) => {
                    let label = format!(
                        "{}:{}",
                        match kind {
                            0 => "dom",
                            1 => "ax",
                            _ => "screenshot",
                        },
                        frame.id
                    );
                    pending.remove(&label);
                    match result {
                        Ok(value) => {
                            if kind == 2 {
                                raw_image = Some(value);
                                continue;
                            }
                            let entry = captured
                                .entry(frame.session.clone())
                                .or_insert_with(|| (frame.clone(), None, vec![]));
                            if kind == 0 {
                                entry.0 = frame;
                                entry.1 = Some(value)
                            } else {
                                entry.2.extend(
                                    value["nodes"].as_array().into_iter().flatten().cloned(),
                                );
                            }
                        }
                        Err(_) => missing.push(label),
                    }
                }
                _ => break,
            }
        }
        tasks.abort_all();
        missing.extend(pending);
        let title = captured
            .values()
            .filter_map(|(_, dom, _)| dom.as_ref())
            .find_map(|dom| {
                let strings = dom["strings"].as_array()?;
                let doc = dom["documents"]
                    .as_array()?
                    .iter()
                    .find(|d| string(strings, &d["frameId"]) == frames[0].id)?;
                Some(string(strings, &doc["title"]))
            })
            .unwrap_or_else(|| ctx.tab.title.clone());
        let mut captures = captured
            .into_values()
            .filter_map(|(frame, dom, ax)| dom.map(|dom| (frame, dom, json!({"nodes":ax}))))
            .collect::<Vec<_>>();
        captures.sort_by_key(|(f, _, _)| {
            frames
                .iter()
                .position(|known| known.id == f.id)
                .unwrap_or(usize::MAX)
        });
        let normalize_frames = frames.clone();
        let mut nodes = tokio::task::spawn_blocking(move || {
            unsafe {
                libc::setpriority(libc::PRIO_PROCESS, 0, 10);
            };
            normalize(captures, &normalize_frames, viewport)
        })
        .await
        .map_err(|e| Fault::new("capture_error", e))?;
        let secrets = nodes
            .iter()
            .filter(|n| n.sensitive)
            .filter_map(|n| n.value.clone())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        let redact = |s: &str| {
            let mut v = self.redactor.redact_text(s);
            for secret in &secrets {
                v = v.replace(secret, "[redacted]")
            }
            v
        };
        let depths = nodes
            .iter()
            .map(|n| {
                let mut d = 0;
                let mut p = n.parent;
                while let Some(i) = p {
                    d += 1;
                    if d >= 8 {
                        break;
                    }
                    p = nodes.get(i).and_then(|n| n.parent);
                }
                d
            })
            .collect::<Vec<_>>();
        for node in &mut nodes {
            if node
                .value
                .as_ref()
                .is_some_and(|v| self.redactor.redact_text(v) != *v)
                || self.redactor.redact_text(&node.text) != node.text
                || self.redactor.redact_text(&node.name) != node.name
            {
                node.sensitive = true;
            }
        }
        let mut book = tracker.book.lock().await;
        let previous = book
            .current
            .as_ref()
            .and_then(|id| book.saved.get(id))
            .map(|s| s.keys.values().cloned().collect::<HashSet<_>>())
            .unwrap_or_default();
        let mut elements = vec![];
        let mut keys = HashMap::new();
        let mut fingerprints = HashMap::new();
        let mut text = format!(
            "# tab {} · {} · {}\n# untrusted\n",
            ctx.tab.id,
            bounded(&redact(&frames[0].url), 4096),
            bounded(&redact(&title), 1024)
        );
        let mut omitted = 0;
        let expanded = Rect {
            x: 0.,
            y: -viewport.h,
            w: viewport.w,
            h: viewport.h * 3.,
        };
        for node in &mut nodes {
            node.name = redact(&node.name);
            node.text = redact(&node.text);
            for value in node.attributes.values_mut() {
                *value = redact(value)
            }
            node.attributes.remove("onclick");
            node.properties = self.redactor.redact_json(&node.properties);
            for secret in &secrets {
                node.properties = replace_strings(&node.properties, secret);
            }
            if node.sensitive {
                node.value = Some("[redacted]".into())
            } else if let Some(value) = &mut node.value {
                *value = redact(value)
            }
            if node.why.is_empty() {
                if options.dom == "full"
                    && !node.text.is_empty()
                    && (options.extent == "fullpage" || node.bbox.intersects(expanded))
                {
                    let line = format!(
                        "{}{} {:?}\n",
                        "  ".repeat(depths[node.index]),
                        node.tag,
                        node.text
                    );
                    if text.len() + line.len() <= 16384 {
                        text.push_str(&line);
                    }
                }
                if matches!(
                    node.role.as_str(),
                    "banner" | "main" | "navigation" | "form" | "dialog" | "heading"
                ) {
                    let line = format!(
                        "{}{} {:?}\n",
                        "  ".repeat(depths[node.index]),
                        node.role,
                        node.name
                    );
                    if text.len() + line.len() <= 16384 {
                        text.push_str(&line)
                    }
                }
                continue;
            }
            let key = node.key.clone().unwrap();
            let reference = if let Some(r) = book.refs.get(&key) {
                r.clone()
            } else {
                let n = self
                    .next_ref
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let frame_index = if key.frame_id == frames[0].id {
                    0
                } else {
                    let next = book.frames.len() as u64 + 1;
                    *book.frames.entry(key.frame_id.clone()).or_insert(next)
                };
                let r = if frame_index == 0 {
                    format!("e{n}")
                } else {
                    format!("f{frame_index}e{n}")
                };
                book.refs.insert(key.clone(), r.clone());
                r
            };
            node.reference = Some(reference.clone());
            keys.insert(reference.clone(), key.clone());
            fingerprints.insert(reference.clone(), node.fingerprint.clone());
            let new = !previous.contains(&key);
            let line = format!(
                "{}- {} {:?} [{}]{}{}{}\n",
                "  ".repeat(depths[node.index]),
                node.role,
                node.name,
                reference,
                if new { " *new" } else { "" },
                if !node.enabled { " disabled" } else { "" },
                node.value
                    .as_ref()
                    .map(|v| format!(" value={v:?}"))
                    .unwrap_or_default()
            );
            if (options.extent == "fullpage" || node.bbox.intersects(expanded))
                && text.len() + line.len() <= 16384
            {
                text.push_str(&line);
                elements.push(Element {
                    reference,
                    role: node.role.clone(),
                    name: node.name.clone(),
                    kind: node
                        .attributes
                        .get("type")
                        .cloned()
                        .unwrap_or_else(|| node.tag.clone()),
                    bbox: node.bbox,
                    visible: node.visible,
                    enabled: node.enabled,
                    in_viewport: node.in_viewport,
                    frame: node.frame.clone(),
                    why: node.why.clone(),
                    value: node.value.clone(),
                    new,
                });
            } else {
                omitted += 1;
            }
        }
        // Retain region/heading hierarchy in the full tree; references address all controls, including omitted ones.
        let id = format!("{}:{}", self.generation, uuid::Uuid::new_v4());
        let sensitive = nodes
            .iter()
            .filter(|n| n.sensitive)
            .map(|n| n.bbox)
            .collect::<Vec<_>>();
        // A missing DOM can hide a sensitive field. Never publish its unmasked pixels.
        let complete_dom = !missing
            .iter()
            .any(|component| component.starts_with("dom:"));
        let raw_image = if complete_dom {
            raw_image
        } else {
            missing.push("screenshot_redaction_incomplete_dom".into());
            None
        };
        let screenshot_ref = match raw_image {
            Some(raw) => {
                match sanitize_image(raw, &metrics, &options.extent, viewport, sensitive).await {
                    Ok(bytes) => Some(
                        self.blob(
                            &ctx.session.owner_run,
                            &id,
                            "screenshot",
                            bytes,
                            "image/jpeg",
                        )
                        .await?,
                    ),
                    Err(_) => {
                        missing.push("screenshot_redaction".into());
                        None
                    }
                }
            }
            None => None,
        };
        if started.elapsed() > Duration::from_secs(3) {
            missing.push("capture_budget".into())
        }
        let tree_bytes = serde_json::to_vec(&nodes).map_err(|e| Fault::new("capture_error", e))?;
        let dom_tree_ref = self
            .blob(
                &ctx.session.owner_run,
                &id,
                "dom",
                tree_bytes,
                "application/json",
            )
            .await?;
        let visual = nodes
            .iter()
            .filter(|n| {
                matches!(n.tag.as_str(), "canvas" | "img" | "video") && n.visible && n.in_viewport
            })
            .map(|n| n.bbox.w * n.bbox.h)
            .sum::<f64>()
            > viewport.w * viewport.h * 0.4;
        if started.elapsed() > Duration::from_secs(3)
            && !missing.iter().any(|m| m == "capture_budget")
        {
            missing.push("capture_budget".into());
        }
        let snapshot = Snapshot {
            snapshot_id: id.clone(),
            tab_id: ctx.tab.id.clone(),
            url: bounded(&redact(&frames[0].url), 4096),
            title: bounded(&redact(&title), 1024),
            load_state: if report.criteria_met.navigation {
                "complete"
            } else {
                "loading"
            }
            .into(),
            screenshot_ref,
            dom_tree_ref,
            elements,
            text,
            omitted,
            partial: !missing.is_empty(),
            missing,
            wait: report,
            untrusted: true,
            vision_recommended: options.screenshot
                || (visual && nodes.iter().filter(|n| n.role != "generic").count() < 20),
            document_epoch: ctx.tab.document_epoch,
        };
        let invalidated = book.invalidation;
        book.saved.insert(
            id.clone(),
            Saved {
                snapshot: snapshot.clone(),
                run: ctx.session.owner_run.clone(),
                tab: ctx.tab.id.clone(),
                keys,
                fingerprints,
                invalidated,
                target: tracker.target.clone(),
                session: tracker.session.clone(),
            },
        );
        book.current = Some(id);
        Ok(snapshot)
    }
}
fn replace_strings(v: &Value, secret: &str) -> Value {
    match v {
        Value::String(s) => json!(s.replace(secret, "[redacted]")),
        Value::Array(a) => json!(
            a.iter()
                .map(|v| replace_strings(v, secret))
                .collect::<Vec<_>>()
        ),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), replace_strings(v, secret)))
                .collect(),
        ),
        _ => v.clone(),
    }
}
async fn capture_raw(
    ctx: &Context,
    sid: &str,
    metrics: &Value,
    extent: &str,
    viewport: Rect,
) -> Result<Value> {
    ctx.client
        .send(Method::PageBringToFront, json!({}), Some(sid))
        .await?;
    let content = &metrics["cssContentSize"];
    let height = if extent == "fullpage" {
        content["height"].as_f64().unwrap_or(viewport.h).min(16384.)
    } else {
        viewport.h
    };
    let clip = json!({"x":metrics["cssLayoutViewport"]["pageX"].as_f64().unwrap_or(0.),"y":if extent=="fullpage"{0.}else{metrics["cssLayoutViewport"]["pageY"].as_f64().unwrap_or(0.)},"width":viewport.w,"height":height,"scale":1});
    let value=ctx.client.send(Method::PageCaptureScreenshot,json!({"format":"jpeg","quality":60,"captureBeyondViewport":extent=="fullpage","clip":clip}),Some(sid)).await?;
    Ok(value)
}
async fn sanitize_image(
    value: Value,
    metrics: &Value,
    extent: &str,
    viewport: Rect,
    masks: Vec<Rect>,
) -> Result<Vec<u8>> {
    let height = if extent == "fullpage" {
        metrics["cssContentSize"]["height"]
            .as_f64()
            .unwrap_or(viewport.h)
            .min(16384.)
    } else {
        viewport.h
    };
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(
            value["data"]
                .as_str()
                .ok_or_else(|| Fault::new("capture_error", "JPEG absent"))?,
        )
        .map_err(|e| Fault::new("capture_error", e))?;
    let fullpage = extent == "fullpage";
    let page_y = metrics["cssLayoutViewport"]["pageY"].as_f64().unwrap_or(0.);
    tokio::task::spawn_blocking(move || {
        let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
            .map_err(|e| Fault::new("capture_error", e))?;
        let mut image = image::imageops::resize(
            &decoded.to_rgb8(),
            viewport.w as u32,
            height as u32,
            image::imageops::FilterType::Triangle,
        );
        for mask in masks {
            let x = mask.x.max(0.) as u32;
            let y = (mask.y + if fullpage { page_y } else { 0. }).max(0.) as u32;
            for py in y..(y.saturating_add(mask.h.ceil() as u32)).min(image.height()) {
                for px in x..(x.saturating_add(mask.w.ceil() as u32)).min(image.width()) {
                    image.put_pixel(px, py, image::Rgb([0, 0, 0]));
                }
            }
        }
        let mut output = vec![];
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 60)
            .encode_image(&image)
            .map_err(|e| Fault::new("capture_error", e))?;
        Ok(output)
    })
    .await
    .map_err(|e| Fault::new("capture_error", e))?
}
