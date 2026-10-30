//! Content-based admission for downloaded attachments. Scanner implementations are trusted host code.
use crate::{Error, Result, Security};
use excalibur_store::{BlobOwner, Store};
use serde_json::{Value, json};
pub trait FileScanner: Send + Sync {
    fn scan(&self, bytes: &[u8], mime: &str) -> std::result::Result<(), String>;
}
pub fn content_type(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"%PDF-") {
        "application/pdf"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"PK\x03\x04") {
        "application/zip"
    } else if std::str::from_utf8(bytes).is_ok() {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}
impl Security {
    pub async fn admit_download(
        &self,
        run: &str,
        operation: &str,
        bytes: Vec<u8>,
        file_name: &str,
        scanner: Option<&dyn FileScanner>,
        now: i64,
    ) -> Result<Value> {
        if bytes.len() > 200 * 1024 * 1024
            || bytes.starts_with(b"MZ")
            || bytes.starts_with(b"\x7fELF")
            || bytes.starts_with(b"#!")
        {
            return Err(Error::Denied(
                "file remains quarantined: size or executable content".into(),
            ));
        }
        if self.redactor.redact_bytes(&bytes) != bytes {
            return Err(Error::Denied(
                "file remains quarantined: secret content".into(),
            ));
        }
        let mime = content_type(&bytes);
        if let Some(scanner) = scanner {
            scanner.scan(&bytes, mime).map_err(|_| {
                Error::Denied("file remains quarantined: scanner rejected content".into())
            })?;
        }
        self.mark_untrusted(run, "browser_download", now).await?;
        self.audit(json!({"kind":"download_file_control","run":run,"operation_id":operation,"mime":mime,"size":bytes.len(),"scanner":if scanner.is_some(){"passed"}else{"unavailable"}})).await?;
        let size = bytes.len();
        let blob = self
            .database()
            .put_blob(
                bytes,
                BlobOwner {
                    kind: "browser_download".into(),
                    id: format!("{run}:{operation}"),
                    slot: "attachment".into(),
                },
            )
            .await?;
        Ok(
            json!({"blob":blob.as_str(),"mime":mime,"size":size,"file_name":file_name,"quarantine_status":"admitted","scanner":if scanner.is_some(){"passed"}else{"unavailable"},"untrusted":true}),
        )
    }
}
