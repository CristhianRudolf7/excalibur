//! Host-bound conversation catalogs with independent, durable source-turn policy.
use crate::{groups::CatalogScope, runtime::SkillRuntime};
use excalibur_store::Priority;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationBinding {
    pub principal: String,
    pub conversation: String,
    pub scope: String,
    pub source: String,
}
impl ConversationBinding {
    pub fn run_key(&self) -> String {
        key("master-skill-turn-v1", self)
    }
    fn catalog_key(&self) -> String {
        key(
            "master-skill-catalog-v1",
            &(&self.principal, &self.conversation, &self.scope),
        )
    }
}
fn key(domain: &str, value: &impl Serialize) -> String {
    let encoded = serde_json::to_vec(&(domain, value)).expect("typed skill binding serializes");
    format!("{domain}:{:x}", Sha256::digest(encoded))
}
impl SkillRuntime {
    /// The caller verifies this binding against its accepted durable source.
    /// No text or model arguments can select the principal or original scope.
    pub async fn bind_conversation_turn(
        &self,
        binding: &ConversationBinding,
        profile: CatalogScope,
        literal_source: &str,
    ) -> Result<String, String> {
        if [
            &binding.principal,
            &binding.conversation,
            &binding.scope,
            &binding.source,
        ]
        .iter()
        .any(|v| v.is_empty())
        {
            return Err("empty conversation skill binding".into());
        }
        let run = binding.run_key();
        let encoded = serde_json::to_string(binding).map_err(|e| e.to_string())?;
        if self.read(&run).await?.is_some() {
            let (lookup, expected) = (run.clone(), encoded.clone());
            self.db
                .read(excalibur_store::ReadLane::Interactive, move |c| {
                    let previous: String = c.query_row(
                        "SELECT binding FROM skill_conversation_bindings WHERE run=?1",
                        [lookup],
                        |r| r.get(0),
                    )?;
                    if previous != expected {
                        return Err(excalibur_store::Error::Invalid(
                            "conversation skill binding changed",
                        ));
                    }
                    Ok(())
                })
                .await
                .map_err(|e| e.to_string())?;
            return Ok(run);
        }
        let catalog = binding.catalog_key();
        self.bind(&catalog, &binding.scope, profile, "").await?;
        // The immutable catalog session is never loaded. Each source starts from
        // that original snapshot, not from another source's active restrictions.
        let (_, session, _) = self
            .read(&catalog)
            .await?
            .ok_or("missing frozen conversation catalog")?;
        let index = self
            .indexes
            .get(&binding.scope)
            .ok_or("skill scope unavailable")?
            .clone();
        let query = literal_source.to_owned();
        let counter = self.counter.clone();
        let (body, suggestions) = tokio::task::spawn_blocking(move || {
            let suggestions = session
                .suggestions(&index, &query, &counter.model)
                .into_iter()
                .map(|h| format!("{}: matched {}", h.name, h.matched_terms.join(", ")))
                .collect::<Vec<_>>();
            Ok::<_, String>((
                session.save()?,
                serde_json::to_string(&suggestions).map_err(|e| e.to_string())?,
            ))
        })
        .await
        .map_err(|e| e.to_string())??;
        let hash = format!("{:x}", Sha256::digest(body.as_bytes()));
        let (run_owned, scope) = (run.clone(), binding.scope.clone());
        self.db.write(Priority::Journal, move |c| {
            c.execute_batch("CREATE TABLE IF NOT EXISTS skill_conversation_bindings(run TEXT PRIMARY KEY,binding TEXT NOT NULL UNIQUE)")?;
            c.execute("INSERT OR IGNORE INTO skill_conversation_bindings VALUES(?1,?2)", (&run_owned, &encoded))?;
            let previous:String=c.query_row("SELECT binding FROM skill_conversation_bindings WHERE run=?1",[&run_owned],|r|r.get(0))?;
            if previous != encoded {return Err(excalibur_store::Error::Invalid("conversation skill binding changed"));}
            c.execute("INSERT OR IGNORE INTO skill_run_sessions VALUES(?1,?2,?3,?4,?5)",(&run_owned,&scope,&body,&hash,&suggestions))?;
            Ok(())
        }).await.map_err(|e|e.to_string())?;
        Ok(run)
    }
}
