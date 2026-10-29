use crate::*;
use std::io::Write;
#[derive(Clone, Debug, Serialize, serde::Deserialize, PartialEq)]
pub struct ExportManifest {
    pub run_id: String,
    pub last_cursor: u64,
    pub files: std::collections::BTreeMap<String, String>,
}
/// A complete JSON object plus newline is serialized before each write. An I/O
/// interruption may truncate the last line; readers recover complete lines only.
pub struct JsonlSink<W: Write> {
    writer: W,
    last_cursor: u64,
    failed: bool,
}
impl<W: Write> JsonlSink<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            last_cursor: 0,
            failed: false,
        }
    }
    pub fn after(writer: W, cursor: u64) -> Self {
        Self {
            writer,
            last_cursor: cursor,
            failed: false,
        }
    }
    pub fn write_event(&mut self, event: &EventEnvelope) -> Result<bool> {
        if self.failed {
            return Err(Error::Invalid(
                "JSONL sink interrupted; reconnect with last confirmed cursor",
            ));
        }
        if event.cursor <= self.last_cursor {
            return Ok(false);
        }
        let mut bytes = serde_json::to_vec(event)?;
        bytes.push(b'\n');
        self.failed = true;
        self.writer.write_all(&bytes)?;
        self.writer.flush()?;
        self.failed = false;
        self.last_cursor = event.cursor;
        Ok(true)
    }
    pub fn into_inner(self) -> W {
        self.writer
    }
    pub fn cursor(&self) -> u64 {
        self.last_cursor
    }
}
impl Telemetry {
    pub async fn export_run(&self, run: &str) -> Result<ExportManifest> {
        let _serial = self.inner.export_serial.lock().await;
        let (metadata, snapshot, finalized, mut events) = self.run_data(run).await?;
        events.retain(|e| e.run_id == run);
        let last_cursor = events.iter().map(|e| e.cursor).max().unwrap_or(0);
        let mut ordered = events.clone();
        ordered.sort_by_key(|e| (e.source.clone(), e.sequence));
        let resources = ordered
            .iter()
            .filter(|e| e.source == "resource")
            .map(|e| serde_json::from_value(e.payload.clone()))
            .collect::<std::result::Result<Vec<resources::ResourceSample>, _>>()?;
        let metrics = metrics::aggregate(
            run,
            &metadata,
            &ordered,
            snapshot.as_ref(),
            resources::ResourceSummary::from_samples(&resources),
        );
        let trajectory = atif::trajectory(run, &metadata, &ordered, &metrics, finalized)?;
        let context = atif::agent_context(&metadata, &metrics);
        let mut files = std::collections::BTreeMap::new();
        let redactor = self.inner.redactor.clone();
        files.insert(
            "metadata.json".to_owned(),
            serde_json::to_vec_pretty(&sanitize(&redactor, &serde_json::to_value(&metadata)?))?,
        );
        files.insert("events.jsonl".to_owned(), lines(&events, &redactor)?);
        files.insert("journal.jsonl".into(),lines(&ordered.iter().filter(|e|e.source=="journal").map(|e|json!({"sequence":e.sequence,"kind":e.journal_kind,"key":e.key,"payload":e.payload})).collect::<Vec<_>>(),&redactor)?);
        files.insert("resources.jsonl".into(), lines(&resources, &redactor)?);
        files.insert(
            "trajectory.json".into(),
            serde_json::to_vec_pretty(&redactor.redact_json(&trajectory))?,
        );
        files.insert(
            "metrics.json".into(),
            serde_json::to_vec_pretty(&redactor.redact_json(&serde_json::to_value(&metrics)?))?,
        );
        files.insert(
            "agent-context.json".into(),
            serde_json::to_vec_pretty(&redactor.redact_json(&context))?,
        );
        if self.inner.export_otel {
            files.insert(
                "otel.json".into(),
                serde_json::to_vec_pretty(
                    &redactor.redact_json(&crate::otel::export(run, &metadata, &ordered, &metrics)),
                )?,
            );
        }
        let directory = self.inner.directory.join("runs").join(run);
        let id = run.to_owned();
        let primary = self
            .inner
            .db
            .read(ReadLane::Interactive, move |c| {
                Ok(c.query_row(
                    "SELECT value=?1 FROM telemetry_settings WHERE name='primary_run'",
                    [id],
                    |r| r.get::<_, bool>(0),
                )?)
            })
            .await?;
        let primary_directory = self.inner.directory.clone();
        let artifact_root = self.inner.artifact_root.clone();
        let run_copy = run.to_owned();
        let manifest = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&directory)?;
            let mut hashes = std::collections::BTreeMap::new();
            for (name, bytes) in files {
                atomic(&directory.join(&name), &bytes)?;
                if primary {
                    let destination = if name == "trajectory.json" || name == "agent-context.json" {
                        artifact_root.join(&name)
                    } else {
                        primary_directory.join(&name)
                    };
                    std::fs::create_dir_all(destination.parent().unwrap())?;
                    atomic(&destination, &bytes)?;
                }
                hashes.insert(name, digest(&bytes));
            }
            let manifest = ExportManifest {
                run_id: run_copy,
                last_cursor,
                files: hashes,
            };
            atomic(
                &directory.join("manifest.json"),
                &serde_json::to_vec_pretty(&manifest)?,
            )?;
            if primary {
                atomic(
                    &primary_directory.join("manifest.json"),
                    &serde_json::to_vec_pretty(&manifest)?,
                )?;
            }
            Ok::<_, Error>(manifest)
        })
        .await
        .map_err(|_| Error::Invalid("telemetry export interrupted"))??;
        let id = run.to_owned();
        let text = serde_json::to_string(&manifest)?;
        self.inner.db.write(Priority::Background,move|c|{c.execute("INSERT INTO telemetry_exports(run_id,last_cursor,manifest) VALUES(?1,?2,?3) ON CONFLICT(run_id) DO UPDATE SET last_cursor=excluded.last_cursor,manifest=excluded.manifest",rusqlite::params![id,last_cursor,text])?;Ok(())}).await?;
        Ok(manifest)
    }
}
fn lines<S: Serialize>(rows: &[S], redactor: &Redactor) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for row in rows {
        serde_json::to_writer(
            &mut bytes,
            &redactor.redact_json(&serde_json::to_value(row)?),
        )?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}
fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or(Error::Invalid("artifact parent missing"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| Error::Io(e.error))?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Decode complete newline-terminated records. A final unterminated line is
/// deliberately ignored; malformed complete records are errors, never skipped.
pub fn decode_jsonl_prefix(bytes: &[u8]) -> Result<Vec<EventEnvelope>> {
    let Some(end) = bytes.iter().rposition(|b| *b == b'\n') else {
        return Ok(Vec::new());
    };
    bytes[..end]
        .split(|b| *b == b'\n')
        .map(|line| serde_json::from_slice(line).map_err(Error::from))
        .collect()
}
