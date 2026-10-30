use crate::*;
use excalibur_engine::Status;
impl Orchestrator {
    pub(crate) async fn ready_order(&self) -> Result<Vec<Agent>> {
        let mut grouped = BTreeMap::<(u8, String), std::collections::VecDeque<Agent>>::new();
        for a in self.active_agents().await? {
            if let Some(run) = &a.run_id
                && matches!(
                    self.inner.engine.load(run).await?.status,
                    Status::Ready | Status::Running | Status::Verifying | Status::Recovering
                )
            {
                grouped
                    .entry((
                        a.delegation.as_ref().map(|d| d.priority).unwrap_or(1),
                        a.scope.clone(),
                    ))
                    .or_default()
                    .push_back(a);
            }
        }
        let last = self.inner.last_scope.lock().unwrap().clone();
        let mut ready = Vec::new();
        while !grouped.is_empty() {
            let priority = grouped
                .keys()
                .next()
                .map(|(priority, _)| *priority)
                .unwrap_or(0);
            let mut keys = grouped
                .keys()
                .filter(|(p, _)| *p == priority)
                .cloned()
                .collect::<Vec<_>>();
            if let Some(last) = &last
                && let Some(pos) = keys.iter().position(|(_, scope)| scope == last)
            {
                let len = keys.len();
                keys.rotate_left((pos + 1) % len);
            }
            for key in keys {
                if let Some(queue) = grouped.get_mut(&key)
                    && let Some(agent) = queue.pop_front()
                {
                    ready.push(agent);
                }
                if grouped.get(&key).is_some_and(|q| q.is_empty()) {
                    grouped.remove(&key);
                }
            }
        }
        Ok(ready)
    }
    /// Reap finished tasks and admit ready work without occupying a worker for waiting parents.
    pub async fn tick(&self) -> Result<Vec<String>> {
        self.reap().await?;
        for a in self.active_agents().await? {
            if a.run_id.is_some() {
                self.deliver_mailbox(&a.id).await?;
            }
        }
        self.enforce_directive_deadlines(now()).await?;
        let _gate = self.inner.gate.lock().await;
        let high = self
            .inner
            .db
            .read(ReadLane::Interactive, |c| {
                Ok(c.query_row(
                    "SELECT high FROM orchestration_resource_state WHERE id=1",
                    [],
                    |r| r.get::<_, bool>(0),
                )?)
            })
            .await?;
        if high {
            return Ok(vec![]);
        }
        let host = self
            .authority()
            .ok_or_else(|| invalid("host RunAuthority required before dispatch"))?;
        let ready = self.ready_order().await?;
        let mut handles = self.inner.running.lock().await;
        let mut dispatched = vec![];
        for a in ready {
            if handles.len() >= self.inner.limits.running
                || (handles.len() as u64)
                    .saturating_add(1)
                    .saturating_mul(self.inner.limits.estimated_run_bytes)
                    > self.inner.limits.ram_bytes
            {
                break;
            }
            if handles.contains_key(&a.id) {
                continue;
            }
            let run = a.run_id.clone().ok_or_else(|| invalid("run missing"))?;
            if let Err(error) = host.bind(&a).await {
                self.inner
                    .engine
                    .block(&run, &format!("host runtime binding rejected: {error}"))
                    .await?;
                let state = self.inner.engine.load(&run).await?;
                self.emit_stop_notice(&a, &state).await?;
                continue;
            }

            let engine = self.inner.engine.clone();
            let id = a.id.clone();
            let wake = self.inner.clone();
            let runtime = self
                .inner
                .runtime
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(tokio::runtime::Handle::current);
            handles.insert(
                id.clone(),
                runtime.spawn(async move {
                    let result = engine.run(&run).await;
                    wake.wake.notify_waiters();
                    result
                }),
            );
            *self.inner.last_scope.lock().unwrap() = Some(a.scope);
            dispatched.push(id);
        }
        Ok(dispatched)
    }
    pub async fn reap(&self) -> Result<usize> {
        let finished = {
            let mut handles = self.inner.running.lock().await;
            let ids = handles
                .iter()
                .filter(|(_, h)| h.is_finished())
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| handles.remove(&id).map(|h| (id, h)))
                .collect::<Vec<_>>()
        };
        let n = finished.len();
        for (id, handle) in finished {
            let a = self.agent(&id).await?;
            let run = a.run_id.as_ref().ok_or_else(|| invalid("run missing"))?;
            match handle.await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    self.inner
                        .engine
                        .fail(run, &format!("execution error: {e}"))
                        .await?;
                }
                Err(e) => {
                    self.inner
                        .engine
                        .fail(run, &format!("execution task panicked: {e}"))
                        .await?;
                }
            }
            let state = self.inner.engine.load(run).await?;
            self.emit_stop_notice(&a, &state).await?;
            if terminal(&state.status)
                && let Some(parent) = a.parent_id.as_ref()
            {
                let p = self.agent(parent).await?;
                if let Some(parent_run) = p.run_id {
                    excalibur_engine::settle_child_budget(
                        self.inner.engine.journal(),
                        &parent_run,
                        &a.id,
                    )
                    .await?;
                }
            }
        }
        Ok(n)
    }
    async fn emit_stop_notice(&self, a: &Agent, state: &excalibur_engine::Snapshot) -> Result<()> {
        let Some(class) = stop_notice_class(&state.status, state.reason.as_deref()) else {
            return Ok(());
        };
        let run = a.run_id.as_ref().ok_or_else(|| invalid("run missing"))?;
        let summary = json!({"state":label(&state.status),"reason":state.reason,"final_text":state.final_text,"step":state.step});
        if let Some(parent) = &a.parent_id {
            self.send_message(
                &format!("completion:{run}:{}", state.last_sequence),
                &a.id,
                parent,
                "completion",
                summary.clone(),
                "normal",
            )
            .await?;
        }
        self.notify(
            &format!("state:{run}:{}", state.last_sequence),
            &a.id,
            class,
            &serde_json::to_string(&summary)?,
            now(),
        )
        .await?;
        Ok(())
    }
    /// The daemon feeds actual RSS every second. Time is explicit for deterministic tests.
    pub async fn resource_sample(&self, rss: u64, at: i64) -> Result<ResourceAction> {
        let _control = self.inner.gate.lock().await;
        let threshold = self.inner.limits.ram_bytes;
        let (high, _resume) = self
            .inner
            .db
            .write(Priority::Interactive, move |c| {
                let (was, below): (bool, Option<i64>) = c.query_row(
                    "SELECT high,below_since FROM orchestration_resource_state WHERE id=1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let (high, below, resume) = if rss > threshold {
                    (true, None, false)
                } else if was && rss < threshold.saturating_mul(80) / 100 {
                    let since = below.unwrap_or(at);
                    if at - since >= 60 {
                        (false, None, true)
                    } else {
                        (true, Some(since), false)
                    }
                } else {
                    (was, None, false)
                };
                c.execute(
                    "UPDATE orchestration_resource_state SET high=?1,below_since=?2 WHERE id=1",
                    rusqlite::params![high, below],
                )?;
                Ok((high, resume))
            })
            .await?;
        let mut action = ResourceAction {
            admit: !high,
            unload_idle: high,
            ..Default::default()
        };
        let host = self.authority();
        if high && let Some(host) = &host {
            host.unload_idle().await.map_err(Error::Authority)?;
        }
        let mut all = self.active_agents().await?;
        all.sort_by_key(|a| {
            std::cmp::Reverse(a.delegation.as_ref().map(|d| d.priority).unwrap_or(0))
        });
        if high {
            for a in &all {
                if let Some(run) = &a.run_id {
                    let state = self.inner.engine.load(run).await?;
                    if matches!(state.status, Status::Running | Status::Verifying)
                        && self.inner.engine.pause_if_active(run, "resource").await?
                    {
                        self.set_pause_owner(&a.id, Some("resource".into()), true)
                            .await?;
                        action.pause.push(a.id.clone());
                    }
                }
            }
            if rss > threshold.saturating_mul(120) / 100
                && let Some(a) = all.iter().find(|a| action.pause.contains(&a.id))
                && let Some(run) = &a.run_id
                && let Some(host) = host
                && host
                    .kill_largest_tool(run)
                    .await
                    .map_err(Error::Authority)?
            {
                action.last_resort = Some(a.id.clone());
            }
        } else if !high {
            drop(_control);
            for a in all.into_iter().rev() {
                if self.paused_by(&a.id).await?.as_deref() == Some("resource")
                    && let Some(run) = &a.run_id
                    && self.inner.engine.load(run).await?.status == Status::Paused
                {
                    if self.inner.engine.resume_if_reason(run, "resource").await? {
                        self.clear_pause_owner_if(&a.id, "resource").await?;
                        action.resume.push(a.id);
                    }
                    break;
                }
            }
        }
        Ok(action)
    }
    pub async fn request_wait_for_children(&self, agent: &str) -> Result<()> {
        let a = self.agent(agent).await?;
        if a.depth >= 2 {
            return Err(invalid("workers have no children to wait for"));
        }
        let id = a.id;
        self.inner.db.write(Priority::Journal,move|c|{c.execute("INSERT INTO orchestration_waits VALUES(?1,1) ON CONFLICT(agent_id) DO UPDATE SET requested=1",[id])?;Ok(())}).await?;
        Ok(())
    }
    pub async fn apply_child_wait(&self, agent: &str) -> Result<bool> {
        let a = self.agent(agent).await?;
        let id = a.id.clone();
        let requested = self
            .inner
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT requested FROM orchestration_waits WHERE agent_id=?1",
                    [id],
                    |r| r.get::<_, bool>(0),
                )
                .optional()?
                .unwrap_or(false))
            })
            .await?;
        if !requested {
            return Ok(false);
        }
        let mut active = false;
        for child in self
            .list_agents()
            .await?
            .into_iter()
            .filter(|c| c.parent_id.as_deref() == Some(&a.id))
        {
            if let Some(run) = child.run_id
                && !terminal(&self.inner.engine.load(&run).await?.status)
            {
                active = true;
                break;
            }
        }
        if active {
            if let Some(run) = a.run_id
                && self.inner.engine.load(&run).await?.status == Status::Running
            {
                self.inner
                    .engine
                    .wait(&run, excalibur_engine::WaitReason::Children)
                    .await?;
                return Ok(true);
            }
        } else {
            self.clear_child_wait(&a.id).await?;
        }
        Ok(false)
    }
    pub(crate) async fn clear_child_wait(&self, agent: &str) -> Result<()> {
        let id = agent.to_owned();
        self.inner
            .db
            .write(Priority::Journal, move |c| {
                c.execute("DELETE FROM orchestration_waits WHERE agent_id=?1", [id])?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    pub async fn shutdown(&self) -> Result<()> {
        let agents = self.active_agents().await?;
        for a in &agents {
            if let Some(run) = &a.run_id {
                let s = self.inner.engine.load(run).await?;
                if matches!(
                    s.status,
                    Status::Running | Status::Verifying | Status::Recovering
                ) {
                    let _control = self.inner.gate.lock().await;
                    if self.inner.engine.pause_if_active(run, "shutdown").await? {
                        self.set_pause_owner(&a.id, Some("shutdown".into()), false)
                            .await?;
                    }
                }
            }
        }
        let handles = std::mem::take(&mut *self.inner.running.lock().await);
        for (_, handle) in handles {
            let _ = handle.await;
        }
        Ok(())
    }
}

fn stop_notice_class(status: &Status, reason: Option<&str>) -> Option<&'static str> {
    match status {
        Status::Blocked | Status::Failed => Some("urgent"),
        Status::Paused => {
            let reason = reason.unwrap_or_default();
            Some(
                if reason == "approval required"
                    || reason == "budget"
                    || reason.starts_with("budget ")
                    || reason.starts_with("budget:")
                    || reason.starts_with("wall time budget")
                    || reason == "tool budget"
                {
                    "urgent"
                } else {
                    "normal"
                },
            )
        }
        Status::Waiting if matches!(reason, Some("user" | "approval")) => Some("urgent"),
        Status::Completed | Status::CompletedWithPending | Status::Cancelled => Some("normal"),
        _ => None,
    }
}
