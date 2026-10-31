//! Durable context sources; the embedding host controls instruction authority.
use crate::{Error, Journal, Result, append_row};
use excalibur_store::{BlobHash, Priority};
use serde_json::Value;

impl Journal {
    /// Archive a canonical tool observation as a referenced blob, including small
    /// results. The host must sanitize content before calling this method.
    pub async fn archive_context_observation(
        &self,
        run: &str,
        key: &str,
        content: &Value,
    ) -> Result<(u64, BlobHash)> {
        self.archive_context_observation_text(run, key, &serde_json::to_string(content)?)
            .await
    }
    /// Archive the exact original JSON bytes, preserving key order and whitespace.
    /// Validation proves the observation is JSON without canonicalizing its text.
    pub async fn archive_context_observation_text(
        &self,
        run: &str,
        key: &str,
        content: &str,
    ) -> Result<(u64, BlobHash)> {
        if key.is_empty() || key.len() > 1024 {
            return Err(Error::Invalid("context observation key invalid"));
        }
        if content.len() > 16 * 1024 * 1024 {
            return Err(Error::Invalid("context observation exceeds 16 MiB"));
        }
        let _: Value = serde_json::from_str(content)?;
        let bytes = content.as_bytes().to_vec();
        let blobs = self.db.blobs();
        let prepared = tokio::task::spawn_blocking(move || blobs.prepare(&bytes))
            .await
            .map_err(|_| Error::Invalid("context observation preparation interrupted"))??;
        let (run, key) = (run.to_owned(), key.to_owned());
        Ok(self
            .db
            .write_with_blob(prepared, Priority::Journal, move |conn, hash| {
                let sequence = append_row(
                    conn,
                    &run,
                    "context_observation_archived",
                    &key,
                    None,
                    Some(hash.as_str()),
                )?;
                Ok((sequence, hash.clone()))
            })
            .await?)
    }
}

impl Journal {
    /// Retrieve only an observation archived by this run. Blob hashes are content
    /// addresses and never grant access to another run's observations.
    pub async fn context_observation(&self, run: &str, hash: &BlobHash) -> Result<Value> {
        let (run, selected) = (run.to_owned(), hash.as_str().to_owned());
        // A purge may have redacted the observation under a new content address.
        let resolved = self.db.read(excalibur_store::ReadLane::Interactive, move |c| {
            let resolved: String = c.query_row("SELECT coalesce((SELECT new_hash FROM journal_payload_redirects WHERE run_id=?1 AND old_hash=?2),?2)", excalibur_store::rusqlite::params![run, selected], |r| r.get(0))?;
            let allowed: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM journal WHERE run_id=?1 AND kind='context_observation_archived' AND payload_blob=?2 AND state='confirmed')", excalibur_store::rusqlite::params![run, resolved], |r| r.get(0))?;
            Ok(allowed.then_some(resolved))
        }).await?;
        let Some(resolved) = resolved else {
            return Err(Error::Invalid("observation is not archived in this run"));
        };
        let resolved: BlobHash = resolved.parse()?;
        Ok(serde_json::from_slice(
            &self.db.read_blob(resolved, 16 * 1024 * 1024).await?,
        )?)
    }
}
