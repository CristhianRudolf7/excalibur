//! Source-bound authorization for explicit multi-project master actions.
use excalibur_master::{Lexicon, LexiconEntry};
use excalibur_orchestrator::Orchestrator;
use excalibur_store::{Priority, ReadLane, rusqlite::OptionalExtension};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) async fn authorize(
    host: &Orchestrator,
    master: &str,
    conversation: &str,
    source_scope: &str,
    scopes: &BTreeSet<String>,
    operation: &str,
    redactor: &excalibur_vault::Redactor,
) -> Result<Vec<Value>, String> {
    if [conversation, source_scope, operation]
        .iter()
        .any(|value| redactor.redact_text(value) != *value)
        || scopes
            .iter()
            .any(|value| redactor.redact_text(value) != *value)
    {
        return Err("secret literal in scope authorization metadata".into());
    }
    let ceiling = host.agent(master).await.map_err(|e| e.to_string())?.token;
    if scopes.is_empty() || scopes.len() > 8 || !scopes.is_subset(&ceiling.data_scopes) {
        return Err("explicit scopes exceed master authority".into());
    }
    let (conversation, source, scopes, operation) = (
        conversation.to_owned(),
        source_scope.to_owned(),
        scopes.clone(),
        operation.to_owned(),
    );
    host.database().write(Priority::Interactive, |c| {
        c.execute_batch("CREATE TABLE IF NOT EXISTS daemon_scope_authorizations(operation TEXT PRIMARY KEY,conversation TEXT NOT NULL,source_scope TEXT NOT NULL,scopes TEXT NOT NULL,originals TEXT NOT NULL CHECK(json_valid(originals)))")?;
        Ok(())
    }).await.map_err(|e|e.to_string())?;
    let key = operation.clone();
    let known = host.database().read(ReadLane::Interactive, move |c| {
        Ok(c.query_row("SELECT conversation,source_scope,scopes,originals FROM daemon_scope_authorizations WHERE operation=?1",[key], |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?))).optional()?)
    }).await.map_err(|e|e.to_string())?;
    let encoded = serde_json::to_string(&scopes).map_err(|_| "invalid scope set")?;
    if let Some((old_conversation, old_source, old_scopes, originals)) = known {
        if (old_conversation, old_source, old_scopes)
            != (conversation.clone(), source.clone(), encoded.clone())
        {
            return Err("scope authorization operation reused".into());
        }
        let originals: Vec<Value> = serde_json::from_str(&originals)
            .map_err(|_| "invalid persisted source authorization")?;
        if originals
            .iter()
            .any(|value| redactor.redact_json(value) != *value)
        {
            return Err("secret literal in source authorization".into());
        }
        validate_sources(host.database(), &conversation, &source, &originals).await?;
        return Ok(originals);
    }
    let (conv, source_copy) = (conversation.clone(), source.clone());
    let originals: Vec<Value> = host.database().read(ReadLane::Interactive, move |c| {
        let seen: Option<String> = c.query_row("SELECT seen FROM master_turn_state WHERE conversation_id=?1 AND scope_id=?2",(&conv,&source_copy),|r|r.get(0)).optional()?;
        let rows = if let Some(seen) = seen {
            let bytes:u64=c.query_row("SELECT COALESCE(SUM(length(CAST(m.text AS BLOB))),0) FROM json_each(?1) j JOIN master_user_sources m ON m.id=j.value WHERE m.conversation_id=?2 AND m.scope_id=?3",(&seen,&conv,&source_copy),|r|r.get(0))?;
            if bytes>1024*1024 {return Err(excalibur_store::Error::Invalid("scope source exceeds its byte ceiling"));}
            let mut q=c.prepare("SELECT m.client_message_id,m.text FROM json_each(?1) j JOIN master_user_sources m ON m.id=j.value WHERE m.conversation_id=?2 AND m.scope_id=?3 ORDER BY CAST(j.key AS INTEGER) LIMIT 128")?;
            q.query_map((&seen,&conv,&source_copy), |r|Ok(json!({"msg_id":r.get::<_,String>(0)?,"text":r.get::<_,String>(1)?})))?.collect::<Result<Vec<_>,_>>()?
        } else {
            let mut q=c.prepare("SELECT client_message_id,text FROM master_user_sources WHERE conversation_id=?1 AND scope_id=?2 ORDER BY created_seq DESC,id DESC LIMIT 1")?;
            q.query_map((&conv,&source_copy), |r|Ok(json!({"msg_id":r.get::<_,String>(0)?,"text":r.get::<_,String>(1)?})))?.collect::<Result<Vec<_>,_>>()?
        };
        Ok(rows)
    }).await.map_err(|e|e.to_string())?;
    if originals.is_empty()
        || originals
            .iter()
            .map(|r| r["text"].as_str().unwrap_or("").len())
            .sum::<usize>()
            > 1024 * 1024
    {
        return Err("scope authorization requires bounded persisted literal sources".into());
    }
    if originals
        .iter()
        .any(|value| redactor.redact_json(value) != *value)
    {
        return Err("secret literal in source authorization".into());
    }
    let projects = host.projects().await.map_err(|e| e.to_string())?;
    let mut entries = vec![LexiconEntry {
        alias: "general".into(),
        id: "general".into(),
        kind: "project".into(),
    }];
    for project in projects {
        let id = format!("project:{}", project.id);
        for alias in std::iter::once(project.name)
            .chain(project.aliases)
            .filter(|alias| !alias.is_ascii() || alias.chars().count() >= 3)
        {
            entries.push(LexiconEntry {
                alias,
                id: id.clone(),
                kind: "project".into(),
            });
        }
        entries.push(LexiconEntry {
            alias: id.clone(),
            id,
            kind: "project".into(),
        });
    }
    let requested = scopes.clone();
    let original_copy = originals.clone();
    let source_copy = source.clone();
    tokio::task::spawn_blocking(move || {
        let lexicon=Lexicon::new(entries).map_err(|e|e.to_string())?;
        let mut mentioned=BTreeSet::new();
        for original in original_copy {
            for mention in lexicon.analyze(original["text"].as_str().unwrap_or(""),"UTC").map_err(|e|e.to_string())?.mentions {
                if mention.candidates.len()==1 {mentioned.insert(mention.candidates[0].clone());}
            }
        }
        if requested.iter().any(|scope|scope!=&source_copy&&!mentioned.contains(scope)) {
            return Err("another project requires an unambiguous literal user mention; ask for clarification".to_owned());
        }
        Ok(())
    }).await.map_err(|_|"scope analysis unavailable")??;
    let original_text = serde_json::to_string(&originals).map_err(|_| "invalid source messages")?;
    host.database().write(Priority::Interactive, move |c| {
        c.execute("INSERT OR IGNORE INTO daemon_scope_authorizations VALUES(?1,?2,?3,?4,?5)",(&operation,&conversation,&source,&encoded,&original_text))?;
        let saved:(String,String,String,String)=c.query_row("SELECT conversation,source_scope,scopes,originals FROM daemon_scope_authorizations WHERE operation=?1",[operation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        if saved!=(conversation,source,encoded,original_text){return Err(excalibur_store::Error::Invalid("scope authorization operation reused"));}
        Ok(())
    }).await.map_err(|e|e.to_string())?;
    Ok(originals)
}

/// Stored authorizations remain usable only while their literal client lineage
/// can be verified. Unknown/generated provenance never becomes a scope grant.
async fn validate_sources(
    db: &excalibur_store::Database,
    conversation: &str,
    scope: &str,
    originals: &[Value],
) -> Result<(), String> {
    let (conversation, scope, originals) = (
        conversation.to_owned(),
        scope.to_owned(),
        originals.to_vec(),
    );
    db.read(ReadLane::Interactive,move|c| {
        if originals.is_empty(){return Err(excalibur_store::Error::Invalid("authorization lacks client sources"));}
        for original in originals {
            let id=original["msg_id"].as_str().ok_or(excalibur_store::Error::Invalid("invalid authorization source"))?;
            let text=original["text"].as_str().ok_or(excalibur_store::Error::Invalid("invalid authorization source"))?;
            let valid:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM master_user_sources WHERE client_message_id=?1 AND text=?2 AND conversation_id=?3 AND scope_id=?4)",excalibur_store::rusqlite::params![id,text,conversation,scope],|r|r.get(0))?;
            if !valid{return Err(excalibur_store::Error::Invalid("scope authorization requires verified client provenance"));}
        }Ok(())
    }).await.map_err(|e|e.to_string())
}
pub(crate) async fn validate_agent(
    db: &excalibur_store::Database,
    agent: &excalibur_orchestrator::Agent,
) -> Result<(), String> {
    let mut current = Some(agent.clone());
    for _ in 0..3 {
        let Some(agent) = current.take() else { break };
        let task = agent.task.clone();
        let record=db.read(ReadLane::Interactive,move|c| {
            let present:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='daemon_task_requests')",[],|r|r.get(0))?;
            if !present{return Ok(None);}
            Ok(c.query_row("SELECT a.conversation,a.source_scope,a.originals FROM daemon_task_requests t LEFT JOIN daemon_scope_authorizations a ON a.operation=t.operation WHERE t.task_id=?1",[task],|r|Ok((r.get::<_,Option<String>>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<String>>(2)?))).optional()?)
        }).await.map_err(|e|e.to_string())?;
        if let Some((conversation, scope, originals)) = record {
            let (conversation, scope, originals) = (
                conversation.ok_or("missing authorization provenance")?,
                scope.ok_or("missing authorization scope")?,
                originals.ok_or("missing authorization sources")?,
            );
            let sources: Vec<Value> =
                serde_json::from_str(&originals).map_err(|_| "invalid authorization sources")?;
            validate_sources(db, &conversation, &scope, &sources).await?;
        }
        current = match agent.parent_id {
            Some(id) => Some(
                db.read(ReadLane::Interactive, move |c| {
                    Ok(c.query_row(
                        "SELECT body FROM orchestration_agents WHERE id=?1",
                        [id],
                        |r| r.get::<_, String>(0),
                    )?)
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|body| {
                    serde_json::from_str(&body).map_err(|_| "invalid parent metadata".into())
                })?,
            ),
            None => None,
        };
    }
    Ok(())
}
