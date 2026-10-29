use crate::*;
use excalibur_store::rusqlite::params;
use std::collections::{BTreeMap, BTreeSet};
impl TaskStore {
    /// Close against fresh local proof and a matching durable independent job.
    /// A fully host-exempt milestone can pass an empty verifier job reference.
    pub async fn close_milestone(
        &self,
        scope: &str,
        id: &str,
        expected_version: u64,
        root_run: &str,
        verifier_job: &str,
    ) -> Result<Value> {
        self.check_scope(scope)?;
        let task = self
            .task_for_run(root_run)
            .await?
            .ok_or_else(|| fail("milestone root has no task binding"))?;
        if task.scope_id != scope || task.root_run_id.as_deref() != Some(root_run) {
            return Err(fail("milestone requires current task root"));
        }
        let _guard = self.mutation_lock.write().await;
        let milestone = self
            .db
            .read(ReadLane::Interactive, {
                let (scope, id, principal) = (
                    scope.to_owned(),
                    id.to_owned(),
                    self.policy.principal_id.clone(),
                );
                move |c| {
                    ops::load(c, "milestone", &scope, &id, principal.as_deref())
                        .map_err(|e| excalibur_store::Error::Configuration(e.to_string()))
                }
            })
            .await?;
        if milestone["version"].as_u64() != Some(expected_version) {
            return Err(fail(format!("version conflict: current={milestone}")));
        }
        let ids: Vec<String> = serde_json::from_value(milestone["criteria"].clone())?;
        if ids.is_empty() {
            return Err(fail("milestone has no closure criteria"));
        }
        let (_, criteria) = self.requirements_and_criteria(&task.id).await?;
        let mut waived = BTreeSet::new();
        let mut definitions = BTreeMap::new();
        for id in &ids {
            let criterion = criteria
                .iter()
                .find(|c| &c.id == id)
                .ok_or_else(|| fail("milestone criterion does not belong to task"))?;
            let revision = self.criterion_revision(id).await?;
            let assessment = self.assess_criterion(criterion, revision).await?;
            match assessment.state {
                CriterionState::Satisfied => {}
                CriterionState::NotApplicable => {
                    waived.insert(id.clone());
                }
                _ => {
                    return Err(fail(format!(
                        "milestone criterion {id} lacks fresh passing evidence"
                    )));
                }
            }
            let mut normalized = criterion.clone();
            normalized
                .checks
                .retain(|c| !matches!(c.definition, crate::CheckDefinition::Inspection));
            definitions.insert(id.clone(), serde_json::to_value(normalized)?);
        }
        let journal = excalibur_journal::Journal::new(self.db.clone()).await?;
        let events = journal.replay(root_run, 0).await?;
        let expected_sequence = events.last().map(|e| e.sequence).unwrap_or(0);
        let mut pending = BTreeSet::new();
        for event in events {
            if let Some(id) = event.payload["id"].as_str() {
                match event.kind.as_str() {
                    "directive_received" => {
                        pending.insert(id.to_owned());
                    }
                    "directive_applied" => {
                        pending.remove(id);
                    }
                    _ => {}
                }
            }
        }
        if !pending.is_empty() {
            return Err(fail(
                "instructions are awaiting application before milestone closure",
            ));
        }
        let (scope, id, root, job, principal) = (
            scope.to_owned(),
            id.to_owned(),
            root_run.to_owned(),
            verifier_job.to_owned(),
            self.policy.principal_id.clone(),
        );
        self.db.write(Priority::Journal,move|c|{
            let sequence:u64=c.query_row("SELECT COALESCE(MAX(sequence),0) FROM journal WHERE run_id=?1",[&root],|r|r.get(0))?;
            if sequence!=expected_sequence{return Err(excalibur_store::Error::Invalid("execution changed while closing milestone"));}
            let mut input=Value::Null;let mut verdict=None;let mut job_revision=None;
            if waived.len()!=ids.len(){
                let(source,proof,revision):(String,String,u64)=c.query_row("SELECT input,verdict,instruction_revision FROM task_verifier_jobs WHERE id=?1 AND root_run_id=?2 AND task_id=?3 AND state='completed'",params![job,root,task.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
                input=serde_json::from_str(&source).map_err(|_|excalibur_store::Error::Invalid("verifier input corrupt"))?;
                let proof:crate::VerificationVerdict=serde_json::from_str(&proof).map_err(|_|excalibur_store::Error::Invalid("verifier verdict corrupt"))?;
                if !proof.request_covered||!proof.uncovered_requests.is_empty()||proof.verifier_run_id.is_empty()||proof.verifier_run_id==root{return Err(excalibur_store::Error::Invalid("verifier did not cover milestone work independently"));}
                verdict=Some(proof);job_revision=Some(revision);
            }
            let mut versions=serde_json::Map::new();let mut revision=job_revision.unwrap_or(0);
            for criterion in &ids{
                let row=ops::load(c,"criterion",&scope,criterion,principal.as_deref()).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;
                let current_revision=row["instruction_revision"].as_u64().unwrap_or(0);revision=current_revision;
                if waived.contains(criterion){
                    let(exempt_version,exempt_revision,justification,actor):(u64,u64,String,String)=c.query_row("SELECT criterion_version,instruction_revision,justification,actor FROM task_criterion_exemptions WHERE criterion_id=?1",[criterion],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
                    if Some(exempt_version)!=row["version"].as_u64()||exempt_revision!=current_revision||justification.trim().is_empty(){return Err(excalibur_store::Error::Invalid("milestone exemption is stale"));}
                    versions.insert(criterion.clone(),json!({"version":exempt_version,"instruction_revision":exempt_revision,"exemption":{"justification":justification,"actor":actor}}));continue;
                }
                if job_revision!=Some(current_revision){return Err(excalibur_store::Error::Invalid("milestone instructions changed after verification"));}
                if !verdict.as_ref().is_some_and(|v|v.criteria.iter().any(|v|&v.criterion_id==criterion&&v.passed)){return Err(excalibur_store::Error::Invalid("verdict does not pass all milestone criteria"));}
                let source=input["criteria"].as_array().and_then(|v|v.iter().find(|v|v["id"]==*criterion)).ok_or(excalibur_store::Error::Invalid("verifier did not inspect milestone criterion"))?;
                if source!=&definitions[criterion]{return Err(excalibur_store::Error::Invalid("criterion definition changed after independent verification"));}
                versions.insert(criterion.clone(),json!({"version":row["version"],"instruction_revision":current_revision}));
            }
            for req in input["requirements"].as_array().into_iter().flatten(){let id=req["id"].as_str().ok_or(excalibur_store::Error::Invalid("verifier requirement id missing"))?;let actual:u64=c.query_row("SELECT version FROM requirements WHERE id=?1",[id],|r|r.get(0))?;if req["version"].as_u64()!=Some(actual){return Err(excalibur_store::Error::Invalid("requirement changed after milestone verification"));}}
            let version_map=Value::Object(versions).to_string();let job=(!job.is_empty()&&verdict.is_some()).then_some(job);
            let previous:Option<(u64,Option<String>,String)>=c.query_row("SELECT version,verifier_job_id,criterion_versions FROM task_milestone_closures WHERE milestone_id=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            if previous==Some((expected_version,job.clone(),version_map.clone())){return Ok(json!({"id":id,"version":expected_version,"state":"done","verifier_job_id":job}));}
            let n=c.execute("UPDATE milestones SET state='done',version=version+1,updated_at=unixepoch() WHERE id=?1 AND version=?2",params![id,expected_version])?;if n!=1{return Err(excalibur_store::Error::Invalid("milestone version conflict"));}
            c.execute("INSERT INTO task_milestone_closures VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(milestone_id) DO UPDATE SET version=excluded.version,verifier_job_id=excluded.verifier_job_id,criterion_versions=excluded.criterion_versions,root_run_id=excluded.root_run_id,instruction_revision=excluded.instruction_revision,closed_at=excluded.closed_at",params![id,expected_version+1,job,version_map,root,revision,excalibur_store::unix_seconds()])?;
            ops::save_version(c,"milestone",&scope,&id,principal.as_deref()).map_err(|e|excalibur_store::Error::Configuration(e.to_string()))?;
            Ok(json!({"id":id,"version":expected_version+1,"state":"done","verifier_job_id":job}))
        }).await.map_err(Error::from)
    }
}
