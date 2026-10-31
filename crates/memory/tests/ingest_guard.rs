//! The document library runs every file through the shared ingestion guard: the built-in guard
//! detects types by content, and a configured `ContentGuard` adds the scanner hook and the
//! durable quarantine without any change to `library_ingest` callers.
mod common;
use common::*;
use excalibur_memory::{
    ingest::{ContentGuard, Quarantine, SignatureScanner, eicar_test_string},
    *,
};
use std::sync::Arc;

fn req(media_type: &str, bytes: &[u8]) -> IngestRequest {
    IngestRequest {
        scope: "general".into(),
        title: "Upload".into(),
        media_type: media_type.into(),
        origin: None,
        bytes: bytes.to_vec(),
        document_id: None,
        library: false,
    }
}

#[tokio::test]
async fn default_library_guard_detects_types_by_content() {
    let f = fixture().await;
    let g = grant(&["general"]);
    let ok = f
        .store
        .library_ingest(&g, req("text/plain", b"Quarterly revenue grew."), NOW)
        .await
        .unwrap();
    assert_eq!(ok.chunks, 1);
    let mut elf = b"\x7FELF\x02\x01\x01".to_vec();
    elf.resize(64, 0);
    for media in ["application/pdf", "audio/mpeg", "application/octet-stream"] {
        let r = f.store.library_ingest(&g, req(media, &elf), NOW).await;
        assert!(
            matches!(&r, Err(Error::Rejected(m)) if m.starts_with("executable")),
            "{media}: {r:?}"
        );
    }
    let r = f
        .store
        .library_ingest(&g, req("image/jpeg", b"%PDF-1.7"), NOW)
        .await;
    assert!(
        matches!(&r, Err(Error::Rejected(m)) if m.starts_with("type_mismatch")),
        "{r:?}"
    );
}

#[tokio::test]
async fn configured_guard_scans_and_quarantines_rejected_uploads() {
    let dir = tempfile::tempdir().unwrap();
    let quarantine = Arc::new(Quarantine::open(dir.path().join("quarantine")).unwrap());
    let guard = ContentGuard::default()
        .with_scanner(Arc::new(SignatureScanner::eicar()))
        .with_quarantine(quarantine.clone());
    let mut f = fixture().await;
    f.store = f.store.clone().with_library(LibraryServices {
        guard: Arc::new(guard),
        ..Default::default()
    });
    let g = grant(&["general"]);

    let mut sample = b"Meeting notes. ".to_vec();
    sample.extend_from_slice(&eicar_test_string());
    let r = f
        .store
        .library_ingest(&g, req("text/plain", &sample), NOW)
        .await;
    assert!(
        matches!(&r, Err(Error::Rejected(m)) if m.starts_with("scanner_hit")),
        "{r:?}"
    );
    let records = quarantine.records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].reason_code, "scanner_hit");
    assert_eq!(records[0].declared_type, "text/plain");
    assert_eq!(
        std::fs::read(quarantine.object_path(&records[0].sha256)).unwrap(),
        sample
    );

    // Clean text still flows through extraction and indexing.
    let ok = f
        .store
        .library_ingest(&g, req("text/markdown", b"# Notes\n\nAll clear."), NOW)
        .await
        .unwrap();
    assert!(ok.chunks >= 1);
    assert_eq!(quarantine.records().unwrap().len(), 1);
}
