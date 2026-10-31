//! Purge contract (§4.7, §4.8): exhaustive marker search, tombstones, backup notice,
//! idempotent replay, journal/telemetry integrity and scope isolation.
mod common;
use common::*;
use excalibur_memory::MemoryPurgeParticipant;
use excalibur_retention::{
    BACKUP_NOTICE_CODE, Error, MAX_BACKUP_AGE_DAYS, PurgeReason, PurgeRequest, PurgeService,
    REDACTION_MARK, SubjectKind,
};
use excalibur_store::{ReadLane, rusqlite};
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purge_leaves_no_marker_in_any_store_and_keeps_a_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::open(dir.path()).await;
    let planted = plant(&world, SCOPE, "a").await;
    let before_events = world.journal.replay(&planted.run, 0).await.unwrap();
    world.main.backup(NOW).await.unwrap();

    let before = scan(dir.path(), &MARKERS);
    for marker in MARKERS {
        assert!(
            before.iter().any(|h| h.ends_with(marker)),
            "{marker} not planted: {before:#?}"
        );
    }
    for needle in [
        "excalibur.db:journal:",
        "excalibur.db:messages:",
        "excalibur.db:memories:",
        "excalibur.db:doc_chunks:",
        "excalibur.db:memory_keys:",
        "excalibur.db:notifications:",
        "excalibur.db:test_inbox:",
        "telemetry.db:events:",
        "telemetry.db:telemetry_runs:",
        "blob:",
        "file:",
    ] {
        assert!(
            before.iter().any(|h| h.starts_with(needle)),
            "precondition: no planted copy at {needle}: {before:#?}"
        );
    }
    let fts_before = fts_hits(dir.path(), &MARKERS);
    for index in [
        "messages_fts",
        "messages_trigram",
        "memories_fts",
        "doc_chunks_fts",
        "memory_keys_fts",
    ] {
        assert!(
            fts_before
                .iter()
                .any(|h| h.starts_with(&format!("{index}:"))),
            "{index}: {fts_before:?}"
        );
    }
    assert!(probe_present(dir.path()));

    let service = world.service().await;
    let mut receipts = Vec::new();
    for request in planted.requests(SCOPE) {
        receipts.push(service.purge(request).await.unwrap());
    }

    let after = scan(dir.path(), &MARKERS);
    assert!(after.is_empty(), "purged content still present: {after:#?}");
    assert!(fts_hits(dir.path(), &MARKERS).is_empty());
    assert!(
        !probe_present(dir.path()),
        "deleted vector bytes remain in vec0 chunks"
    );
    println!(
        "exhaustive search: before={} locations, after=0; raw page/WAL bytes after purge: {:?}",
        before.len(),
        raw_file_hits(dir.path(), &MARKERS)
    );

    for (request, receipt) in planted.requests(SCOPE).iter().zip(&receipts) {
        assert_eq!(receipt.operation_key, request.operation_key);
        assert_eq!(receipt.tombstone.kind, request.kind);
        assert_eq!(receipt.tombstone.subject_id, request.subject_id);
        assert_eq!(receipt.tombstone.scope, SCOPE);
        assert_eq!(receipt.tombstone.reason, PurgeReason::UserRequest);
        assert!(receipt.tombstone.id.starts_with("tmb_"));
        assert_eq!(
            service
                .tombstone(SCOPE, request.kind, &request.subject_id)
                .await
                .unwrap()
                .as_ref(),
            Some(&receipt.tombstone)
        );
        assert_eq!(receipt.notice.code, BACKUP_NOTICE_CODE);
        assert_eq!(receipt.notice.max_backup_age_days, MAX_BACKUP_AGE_DAYS);
        assert!(receipt.notice.message.contains("backups"));
        assert!(receipt.telemetry_covered);
        assert!(receipt.hygiene.secure_delete && !receipt.hygiene.vacuum);
        assert!(receipt.hygiene.main_wal_truncated);
        assert_eq!(receipt.hygiene.telemetry_wal_truncated, Some(true));
        // Replaying the same key returns the stored receipt without new work.
        assert_eq!(&service.purge(request.clone()).await.unwrap(), receipt);
        let mut other_key = request.clone();
        other_key.operation_key.push_str(":again");
        assert_eq!(&service.purge(other_key).await.unwrap(), receipt);
    }
    let (conversation, document, memory) = (
        &receipts[0].counts,
        &receipts[1].counts,
        &receipts[2].counts,
    );
    println!(
        "counts conversation={conversation:?}\ncounts document={document:?}\ncounts memory={memory:?}"
    );
    assert!(conversation.rows_deleted >= 5, "{conversation:?}");
    assert!(
        conversation.rows_nulled >= 1,
        "bystander reply_to is cleared"
    );
    assert!(conversation.journal_events_redacted >= 1 && conversation.telemetry_rows_redacted >= 1);
    assert!(conversation.participant_rows_deleted >= 1 && conversation.blobs_removed >= 1);
    assert!(conversation.fts_rebuilt >= 2 && conversation.runs_reexported >= 1);
    assert!(document.blobs_removed >= 2 && document.journal_events_redacted >= 1);
    assert!(memory.vectors_deleted >= 1 && memory.edges_deleted == 1);
    assert!(memory.participant_rows_deleted >= 1);

    // Tombstones carry identifiers and codes only.
    let tombstone_columns: Vec<String> = world
        .main
        .read(ReadLane::Interactive, |c| {
            let mut s = c.prepare("SELECT name FROM pragma_table_info('retention_tombstones')")?;
            Ok(s.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
        .unwrap();
    assert_eq!(
        tombstone_columns,
        [
            "id",
            "principal_id",
            "scope_id",
            "subject_kind",
            "subject_id",
            "reason",
            "purged_at"
        ]
    );
    let leftovers: (i64, i64, String) = world
        .main
        .read(ReadLane::Interactive, |c| {
            Ok((
                c.query_row("SELECT count(*) FROM retention_purge_needles", [], |r| {
                    r.get(0)
                })?,
                c.query_row("SELECT count(*) FROM retention_tombstones", [], |r| {
                    r.get(0)
                })?,
                c.query_row("PRAGMA quick_check", [], |r| r.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(leftovers, (0, 3, "ok".into()));
    let vectors: (i64, i64) = world
        .main
        .read(ReadLane::Interactive, |c| {
            let tables: Vec<String> = c
                .prepare(
                    "SELECT name FROM sqlite_schema WHERE type='table' AND sql LIKE '%USING vec0%'",
                )?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let mut rows = 0;
            for table in tables {
                rows += c.query_row(
                    &format!("SELECT count(*) FROM \"{table}\" WHERE scope_id=?1"),
                    [SCOPE],
                    |r| r.get::<_, i64>(0),
                )?;
            }
            Ok((
                rows,
                c.query_row(
                    "SELECT count(*) FROM memory_vector_objects WHERE scope_id=?1",
                    [SCOPE],
                    |r| r.get(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(vectors, (0, 0), "no vector of the purged scope remains");

    // An unrelated message survives with its broken reference cleared.
    let bystander = planted.bystander_message.clone();
    let reply: (String, Option<String>) = world
        .main
        .read(ReadLane::Interactive, move |c| {
            Ok(c.query_row(
                "SELECT text,reply_to FROM messages WHERE id=?1",
                [bystander],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        reply,
        ("An unrelated bystander message that stays.".into(), None)
    );

    // Journal structure and replay are preserved; payloads are redacted in place.
    let after_events = world.journal.replay(&planted.run, 0).await.unwrap();
    assert_eq!(
        before_events
            .iter()
            .map(|e| (e.sequence, e.kind.clone(), e.key.clone()))
            .collect::<Vec<_>>(),
        after_events
            .iter()
            .map(|e| (e.sequence, e.kind.clone(), e.key.clone()))
            .collect::<Vec<_>>()
    );
    assert!(
        after_events
            .iter()
            .any(|e| e.payload.to_string().contains(REDACTION_MARK))
    );
    assert_eq!(
        world.journal.state(&planted.run).await.unwrap().0,
        "completed"
    );
    assert_eq!(
        world.journal.replay(&planted.run, 0).await.unwrap(),
        after_events
    );
    world
        .journal
        .set_state(&planted.run, "completed", None, "done")
        .await
        .unwrap();
    // Re-sealed hashes: reconciling the redacted journal and re-observing events agree.
    world
        .telemetry
        .reconcile(&world.journal, &planted.run)
        .await
        .unwrap();
    let run = planted.run.clone();
    let observed: Vec<(u64, String)> = world
        .main
        .read(ReadLane::Interactive, move |c| {
            let mut s = c.prepare("SELECT sequence,payload_hash FROM task_observed_events WHERE run_id=?1 ORDER BY sequence")?;
            Ok(s.query_map([run], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
        })
        .await
        .unwrap();
    assert_eq!(observed.len(), after_events.len());
    for ((sequence, hash), event) in observed.iter().zip(&after_events) {
        assert_eq!(*sequence, event.sequence);
        assert_eq!(
            hash,
            &sha256(&serde_json::to_vec(event).unwrap()),
            "sequence {sequence}"
        );
    }

    // The notice is honest: the pre-purge backup still holds the content.
    let backups = dir.path().join("backups").join("excalibur.db");
    let mut found_in_backup = false;
    for entry in std::fs::read_dir(&backups).unwrap().flatten() {
        let bytes = std::fs::read(entry.path()).unwrap().to_ascii_lowercase();
        found_in_backup |= bytes
            .windows(MEMORY_MARK.len())
            .any(|w| w == MEMORY_MARK.as_bytes());
    }
    assert!(
        found_in_backup,
        "backup copy should retain data until rotation"
    );
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purge_never_crosses_scope_or_principal() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::open(dir.path()).await;
    let a = plant(&world, SCOPE, "a").await;
    let b = plant(&world, OTHER, "b").await;
    let service = world.service().await;
    // Wrong scope or wrong principal: indistinguishable from a missing subject.
    for request in b.requests(SCOPE).into_iter().chain(a.requests(OTHER)) {
        assert!(matches!(service.purge(request).await, Err(Error::NotFound)));
    }
    let intruder = PurgeService::open(
        "intruder",
        world.main.clone(),
        Some(world.telemetry.clone()),
    )
    .await
    .unwrap()
    .with_participant(Arc::new(MemoryPurgeParticipant));
    for request in a.requests(SCOPE) {
        assert!(matches!(
            intruder.purge(request).await,
            Err(Error::NotFound)
        ));
    }
    let mut conflicting = a.requests(SCOPE)[0].clone();
    for request in a.requests(SCOPE) {
        service.purge(request).await.unwrap();
    }
    conflicting.subject_id = b.conversation.clone();
    conflicting.scope = OTHER.into();
    assert!(matches!(
        service.purge(conflicting).await,
        Err(Error::KeyConflict)
    ));
    assert!(
        service
            .tombstone(OTHER, SubjectKind::Memory, &a.memory)
            .await
            .unwrap()
            .is_none()
    );

    // Every scope-B subject and its derived rows survive untouched.
    let (b2, scope) = (b.clone(), OTHER.to_string());
    let survivors: (String, i64, i64, String, i64, i64) = world
        .main
        .read(ReadLane::Interactive, move |c| {
            Ok((
                c.query_row("SELECT text FROM messages WHERE id=?1", [&b2.messages[0]], |r| r.get(0))?,
                c.query_row("SELECT count(*) FROM doc_chunks WHERE document_id=?1", [&b2.document], |r| r.get(0))?,
                c.query_row("SELECT count(*) FROM memory_vector_objects WHERE scope_id=?1", [&scope], |r| r.get(0))?,
                c.query_row("SELECT content FROM notifications WHERE scope_id=?1", [&scope], |r| r.get(0))?,
                c.query_row("SELECT count(*) FROM messages_fts f JOIN messages m ON m.rowid=f.rowid WHERE messages_fts MATCH ?1 AND m.scope_id=?2", [CONVERSATION_MARK, &scope], |r| r.get(0))?,
                c.query_row("SELECT count(*) FROM memory_keys WHERE scope_id=?1", [&scope], |r| r.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(survivors.0, conversation_text("b"));
    assert!(survivors.1 >= 1 && survivors.2 >= 3 && survivors.5 == 1);
    assert_eq!(survivors.3, format!("Reminder: {}", memory_text("b")));
    assert!(survivors.4 >= 1);
    let memory = world
        .memory
        .memory(&grant(&[OTHER]), &b.memory)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(memory.statement, memory_text("b"));
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_schema_requires_its_participant_and_memory_store_delegates() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::open(dir.path()).await;
    let planted = plant(&world, SCOPE, "a").await;
    let bare = PurgeService::open(PRINCIPAL, world.main.clone(), None)
        .await
        .unwrap();
    let request = PurgeRequest {
        operation_key: "bare".into(),
        scope: SCOPE.into(),
        kind: SubjectKind::Memory,
        subject_id: planted.memory.clone(),
        reason: PurgeReason::UserRequest,
        requested_at: NOW,
    };
    assert!(matches!(
        bare.purge(request).await,
        Err(Error::ParticipantMissing("memory"))
    ));
    world
        .memory
        .purge_memory(&grant(&[SCOPE]), &planted.memory, NOW)
        .await
        .unwrap();
    let receipt = bare
        .receipt(&format!("memory:{}", planted.memory))
        .await
        .unwrap()
        .unwrap();
    assert!(
        !receipt.telemetry_covered,
        "the memory store path has no telemetry handle"
    );
    assert!(
        world
            .memory
            .memory(&grant(&[SCOPE]), &planted.memory)
            .await
            .unwrap()
            .is_none()
    );
    let left = scan(dir.path(), &[MEMORY_MARK]);
    assert!(
        left.iter()
            .all(|h| h.starts_with("telemetry.db") || h.contains("/telemetry/")),
        "only telemetry copies may remain without a telemetry handle: {left:#?}"
    );
    world.shutdown().await;
}
