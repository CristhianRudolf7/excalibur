use super::*;
impl Observation {
    async fn saved(&self, ctx: &Context, id: &str) -> Result<Saved> {
        let mut tracker = Some(self.tracker(ctx));
        while let Some(t) = tracker {
            if let Some(saved) = t.book.lock().await.saved.get(id).cloned() {
                if saved.run != ctx.session.owner_run || saved.tab != ctx.tab.id {
                    return Err(Fault::new("denied", "snapshot tab"));
                }
                return Ok(saved);
            }
            tracker = t.previous.clone();
        }
        Err(Fault::new(
            "stale_ref",
            if id.starts_with(&self.generation) {
                "unknown_snapshot"
            } else {
                "browser_restarted"
            },
        ))
    }
    pub(crate) async fn subtree(
        &self,
        ctx: &Context,
        id: &str,
        reference: Option<&str>,
    ) -> Result<Value> {
        let saved = self.saved(ctx, id).await?;
        let bytes = self
            .store()
            .await?
            .get_blob(
                saved
                    .snapshot
                    .dom_tree_ref
                    .sha256
                    .parse()
                    .map_err(|e| Fault::new("storage_error", e))?,
                128 * 1024 * 1024,
            )
            .await
            .map_err(|e| Fault::new("storage_error", e))?;
        let nodes: Vec<TreeNode> =
            serde_json::from_slice(&bytes).map_err(|e| Fault::new("storage_error", e))?;
        let root = reference
            .map(|r| {
                nodes
                    .iter()
                    .position(|n| n.reference.as_deref() == Some(r))
                    .ok_or_else(|| Fault::new("unknown_ref", r))
            })
            .transpose()?;
        let selected = nodes
            .iter()
            .filter(|n| {
                root.is_none_or(|r| {
                    let mut i = Some(n.index);
                    for _ in 0..nodes.len() {
                        let Some(index) = i else { break };
                        if index == r {
                            return true;
                        }
                        i = nodes.get(index).and_then(|n| n.parent);
                    }
                    false
                })
            })
            .collect::<Vec<_>>();
        Ok(
            json!({"snapshot_id":id,"url":saved.snapshot.url,"nodes":selected,"untrusted":true,"source_blob":saved.snapshot.dom_tree_ref}),
        )
    }
    pub(crate) async fn validate(&self, ctx: &Context, id: &str, reference: &str) -> Result<Value> {
        let saved = match self.saved(ctx, id).await {
            Ok(saved) => saved,
            Err(error) if error.kind == "stale_ref" => {
                return Ok(
                    json!({"status":"stale_ref","reason":error.detail,"ref":reference,"snapshot_id":id}),
                );
            }
            Err(e) => return Err(e),
        };
        let key = saved
            .keys
            .get(reference)
            .ok_or_else(|| Fault::new("unknown_ref", reference))?;
        let tracker = self.tracker(ctx);
        let current = tracker.book.lock().await.current.clone();
        let stale = |reason: &str| json!({"status":"stale_ref","reason":reason,"ref":reference,"snapshot_id":id,"current_snapshot_id":current});
        if saved.target != tracker.target || saved.session != tracker.session {
            return Ok(stale("browser_restarted"));
        }
        let book = tracker.book.lock().await;
        if book.invalidation != saved.invalidated {
            return Ok(stale(book.reason.as_deref().unwrap_or("takeover")));
        }
        if book.current.as_deref() != Some(id) {
            let mut result = stale("superseded");
            if let Some(current) = book.current.as_ref().and_then(|id| book.saved.get(id))
                && let Some((r, _)) = current.keys.iter().find(|(_, k)| *k == key)
            {
                result["remapped_ref"] = json!(r);
                result["current_snapshot_id"] = json!(book.current);
            }
            return Ok(result);
        }
        drop(book);
        let frames = self.frames(ctx).await?;
        let Some(frame) = frames.iter().find(|f| f.id == key.frame_id) else {
            return Ok(stale("frame_gone"));
        };
        if frame.loader != key.loader_id {
            return Ok(stale("navigated"));
        }
        let resolved = ctx
            .client
            .send_with_timeout(
                Method::DOMResolveNode,
                json!({"backendNodeId":key.backend_node_id}),
                Some(&key.session_id),
                Duration::from_millis(500),
            )
            .await;
        let Ok(resolved) = resolved else {
            return Ok(stale("node_gone"));
        };
        let Some(object) = resolved["object"]["objectId"].as_str() else {
            return Ok(stale("node_gone"));
        };
        let live=ctx.client.send_with_timeout(Method::RuntimeCallFunctionOn,json!({"objectId":object,"functionDeclaration":"function(){return {connected:this.isConnected,replaced:!!this.id&&!!this.ownerDocument.getElementById(this.id)&&this.ownerDocument.getElementById(this.id)!==this,tag:this.tagName?.toLowerCase()}}","returnByValue":true}),Some(&key.session_id),Duration::from_millis(500)).await;
        let _ = ctx
            .client
            .send_with_timeout(
                Method::RuntimeReleaseObject,
                json!({"objectId":object}),
                Some(&key.session_id),
                Duration::from_millis(500),
            )
            .await;
        let Ok(live) = live else {
            return Ok(stale("node_gone"));
        };
        if live["result"]["value"]["connected"] != true {
            return Ok(stale(if live["result"]["value"]["replaced"] == true {
                "changed"
            } else {
                "node_detached"
            }));
        }
        let ax = ctx
            .client
            .send_with_timeout(
                Method::AccessibilityGetPartialAXTree,
                json!({"backendNodeId":key.backend_node_id,"fetchRelatives":false}),
                Some(&key.session_id),
                Duration::from_millis(500),
            )
            .await?;
        let node = ax["nodes"].as_array().and_then(|n| {
            n.iter()
                .find(|n| n["backendDOMNodeId"].as_u64() == Some(key.backend_node_id))
        });
        if let Some(node) = node {
            let role = node["role"]["value"].as_str().unwrap_or("");
            let name = node["name"]["value"].as_str().unwrap_or("");
            let tag = live["result"]["value"]["tag"].as_str().unwrap_or("");
            if saved.fingerprints.get(reference) != Some(&tree::fingerprint(role, name, tag)) {
                return Ok(stale("changed"));
            }
        }
        Ok(json!({"status":"valid","key":key,"ref":reference,"snapshot_id":id}))
    }
    pub(crate) async fn emulate(&self, ctx: &Context, p: &Value) -> Result<Value> {
        let preset = p["preset"].as_str().unwrap_or("desktop");
        let (w, h, d, m) = match preset {
            "small" => (360, 740, 3., true),
            "tablet" => (768, 1024, 1., false),
            "desktop" => (1280, 900, 1., false),
            "large" => (1440, 900, 1., false),
            "custom" => (
                p["width"].as_u64().unwrap_or(0),
                p["height"].as_u64().unwrap_or(0),
                p["device_scale_factor"].as_f64().unwrap_or(1.),
                p["mobile"].as_bool().unwrap_or(false),
            ),
            _ => return Err(Fault::new("invalid_params", "viewport preset")),
        };
        if w == 0 || w > 4096 || h == 0 || h > 16384 || !(0.5..=4.).contains(&d) {
            return Err(Fault::new("invalid_params", "viewport bounds"));
        }
        if let Some(ua) = p.get("user_agent") {
            let ua = ua
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 1024 && !s.contains(['\r', '\n']))
                .ok_or_else(|| Fault::new("invalid_params", "user_agent"))?;
            if ctx.session.profile != "ephemeral" {
                return Err(Fault::new("denied", "persistent user agent change"));
            }
            ctx.client
                .send_with_timeout(
                    Method::NetworkSetUserAgentOverride,
                    json!({"userAgent":ua}),
                    ctx.tab.cdp_session.as_deref(),
                    Duration::from_millis(1000),
                )
                .await?;
        }
        ctx.client
            .send_with_timeout(
                Method::EmulationSetDeviceMetricsOverride,
                json!({"width":w,"height":h,"deviceScaleFactor":d,"mobile":m}),
                ctx.tab.cdp_session.as_deref(),
                Duration::from_millis(1000),
            )
            .await?;
        self.invalidate(&ctx.tab.id, "changed").await;
        Ok(json!({"width":w,"height":h,"device_scale_factor":d,"mobile":m}))
    }
}
