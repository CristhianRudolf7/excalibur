use crate::*;
use excalibur_engine::Status;
impl Orchestrator {
    pub(crate) async fn ready_order(&self) -> Result<Vec<Agent>> {
        let active = self
            .inner
            .running
            .lock()
            .await
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut grouped = BTreeMap::<(u8, String), std::collections::VecDeque<Agent>>::new();
        for a in self.active_agents().await? {
            if !active.contains(&a.id)
                && let Some(run) = &a.run_id
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
        let mut service = self.inner.fair_service.lock().unwrap().clone();
        let mut ready = Vec::new();
        while !grouped.is_empty() {
            let priority = grouped.keys().next().unwrap().0;
            let key = grouped
                .keys()
                .filter(|(p, _)| *p == priority)
                .min_by(|a, b| {
                    let sa = *service.get(*a).unwrap_or(&0);
                    let sb = *service.get(*b).unwrap_or(&0);
                    let wa = u64::from(*self.inner.limits.project_weights.get(&a.1).unwrap_or(&1));
                    let wb = u64::from(*self.inner.limits.project_weights.get(&b.1).unwrap_or(&1));
                    sa.saturating_mul(wb)
                        .cmp(&sb.saturating_mul(wa))
                        .then_with(|| a.cmp(b))
                })
                .unwrap()
                .clone();
            if let Some(queue) = grouped.get_mut(&key)
                && let Some(agent) = queue.pop_front()
            {
                ready.push(agent);
                *service.entry(key.clone()).or_default() += 1;
            }
            if grouped.get(&key).is_some_and(|queue| queue.is_empty()) {
                grouped.remove(&key);
            }
        }
        Ok(ready)
    }
    /// Reap finished tasks and admit ready work without occupying a worker for waiting parents.
    pub async fn tick(&self) -> Result<Vec<String>> {
        self.reap().await?;
        // A watchdog storage failure must not stop control servicing or admission.
        if let Err(error) = self.watchdog_runs(now()).await {
            eprintln!("run watchdog pass failed: {error}");
        }
        for a in self.active_agents().await? {
            if a.run_id.is_some() {
                self.deliver_mailbox(&a.id).await?;
            }
        }
        self.enforce_directive_deadlines(now()).await?;
        self.service_question_deadlines(now()).await?;
        let _gate = self.inner.gate.lock().await;
        let recovery = self.recovery().status(now()).await?;
        if recovery.recovering || recovery.safe_mode {
            return Ok(vec![]);
        }
        self.resume_safe_mode_owned().await?;
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
        let capacity = host.admission_capacity();
        let process_limit = self.inner.limits.process_limit.min(capacity.process_limit);
        let provider_limit = self
            .inner
            .limits
            .provider_concurrency
            .min(capacity.provider_limit);
        let mut reserved_processes = 0usize;
        let mut parent_slots = BTreeMap::<String, usize>::new();
        let admitted = self
            .inner
            .db
            .read(ReadLane::Interactive, |c| {
                let mut q = c.prepare("SELECT agent_id FROM orchestration_admissions")?;
                Ok(q.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<std::collections::BTreeSet<_>, _>>()?)
            })
            .await?;
        for agent in self.active_agents().await? {
            if agent.depth == 2
                && admitted.contains(&agent.id)
                && let Some(parent) = agent.parent_id
            {
                *parent_slots.entry(parent).or_default() += 1;
            }
        }

        for id in handles.keys() {
            let agent = self.agent(id).await?;
            if agent.token.processes {
                reserved_processes = reserved_processes
                    .saturating_add(self.inner.limits.estimated_processes_per_run);
            }
        }
        reserved_processes = reserved_processes.max(capacity.processes_active);
        let mut reserved_provider = handles.len().max(capacity.provider_active);
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
            if reserved_provider >= provider_limit {
                break;
            }
            let process_cost = if a.token.processes {
                self.inner.limits.estimated_processes_per_run
            } else {
                0
            };
            if reserved_processes.saturating_add(process_cost) > process_limit {
                continue;
            }
            if a.depth == 2
                && a.parent_id.as_ref().is_some_and(|parent| {
                    parent_slots
                        .get(parent)
                        .copied()
                        .unwrap_or(0)
                        .saturating_sub(usize::from(admitted.contains(&a.id)))
                        >= self.inner.limits.children_per_parent
                })
            {
                continue;
            }
            let run = a.run_id.clone().ok_or_else(|| invalid("run missing"))?;
            if !self.recovery().track_admission(&run).await? {
                continue;
            }
            if let Err(error) = host.bind(&a).await {
                self.inner
                    .engine
                    .block(&run, &format!("host runtime binding rejected: {error}"))
                    .await?;
                let state = self.inner.engine.load(&run).await?;
                self.emit_stop_notice(&a, &state).await?;
                continue;
            }

            let admitted_id = a.id.clone();
            self.inner
                .db
                .write(Priority::Journal, move |c| {
                    c.execute(
                        "INSERT OR IGNORE INTO orchestration_admissions VALUES(?1,unixepoch())",
                        [admitted_id],
                    )?;
                    Ok(())
                })
                .await?;
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
            reserved_processes = reserved_processes.saturating_add(process_cost);
            reserved_provider = reserved_provider.saturating_add(1);
            if a.depth == 2
                && !admitted.contains(&a.id)
                && let Some(parent) = &a.parent_id
            {
                *parent_slots.entry(parent.clone()).or_default() += 1;
            }
            *self
                .inner
                .fair_service
                .lock()
                .unwrap()
                .entry((
                    a.delegation.as_ref().map(|d| d.priority).unwrap_or(1),
                    a.scope.clone(),
                ))
                .or_default() += 1;
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
            let task = handle.id();
            let outcome = handle.await;
            // Matched by exact task identity, independent of any decision the
            // watchdog or a user control recorded while the task was stopping.
            let aborted_by_watchdog = self
                .inner
                .watchdog_aborted_tasks
                .lock()
                .unwrap()
                .remove(&task);
            let watchdog_abort =
                aborted_by_watchdog && matches!(&outcome, Err(e) if e.is_cancelled());
            match outcome {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    self.inner
                        .engine
                        .fail(run, &format!("execution error: {e}"))
                        .await?;
                }
                // A driver aborted by the stall watchdog, after its durable
                // pause/block CAS and abort receipt, is not a run failure.
                // Any other cancellation still fails the run.
                Err(_) if watchdog_abort => {}
                Err(e) => {
                    self.inner
                        .engine
                        .fail(run, &format!("execution task panicked: {e}"))
                        .await?;
                }
            }
            self.finalize_agent_stop(&a).await?;
        }
        Ok(n)
    }
    pub(crate) async fn finalize_agent_stop(&self, a: &Agent) -> Result<()> {
        let run = a.run_id.as_ref().ok_or_else(|| invalid("run missing"))?;
        let state = self.inner.engine.load(run).await?;
        self.emit_stop_notice(a, &state).await?;
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
        Ok(())
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
                        self.emit_stop_notice(a, &self.inner.engine.load(run).await?)
                            .await?;
                        break;
                    }
                }
            }
            if action.pause.is_empty()
                && rss > threshold.saturating_mul(120) / 100
                && let Some(host) = host
            {
                for a in &all {
                    if self.paused_by(&a.id).await?.as_deref() == Some("resource")
                        && let Some(run) = &a.run_id
                        && self.inner.engine.load(run).await?.status == Status::Paused
                        && host
                            .kill_largest_tool(run)
                            .await
                            .map_err(Error::Authority)?
                    {
                        action.last_resort = Some(a.id.clone());
                        break;
                    }
                }
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
        let mut failure = None;
        let agents = match self.active_agents().await {
            Ok(agents) => agents,
            Err(error) => {
                failure = Some(error);
                Vec::new()
            }
        };
        for agent in agents {
            let Some(run) = &agent.run_id else { continue };
            let result = async {
                let state = self.inner.engine.load(run).await?;
                if matches!(
                    state.status,
                    Status::Running | Status::Verifying | Status::Recovering
                ) {
                    let _guard = self.inner.gate.lock().await;
                    if self.inner.engine.pause_if_active(run, "shutdown").await? {
                        self.set_pause_owner(&agent.id, Some("shutdown".into()), false)
                            .await?;
                    }
                }
                Ok::<_, Error>(())
            }
            .await;
            if let Err(error) = result {
                failure.get_or_insert(error);
            }
        }
        let handles = std::mem::take(&mut *self.inner.running.lock().await);
        for (_, handle) in handles {
            if failure.is_some() {
                handle.abort();
            }
            let _ = handle.await;
        }
        if let Some(error) = failure {
            return Err(error);
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
