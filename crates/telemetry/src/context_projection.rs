//! A context receipt references its recoverable principal-journal snapshot. The
//! telemetry store must not duplicate the session's history and tool catalogue.
use crate::Event;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{self, Write};

struct HashWriter(Sha256);
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
/// Same serialization/hash as the existing source identity, without allocating
/// another full serialized session merely to compute its SHA-256.
pub(crate) fn payload_hash(payload: &Value) -> serde_json::Result<String> {
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, payload)?;
    Ok(format!("{:x}", writer.0.finalize()))
}
pub(crate) fn receipt(run: &str, event: &Event, source_hash: &str) -> Value {
    json!({
        "receipt":event.payload["receipt"],
        "history_cursor":event.payload["history_cursor"],
        "instruction_digest":event.payload["instruction_digest"],
        "source":{
            "kind":"principal_journal_event",
            "run_id":run,
            "sequence":event.sequence,
            "event_kind":event.kind,
            "event_key":event.key,
            "payload_sha256":source_hash,
            "payload_encoding":"serde_json",
        },
    })
}
