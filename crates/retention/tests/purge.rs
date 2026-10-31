//! Purge contract (§4.7, §4.8): exhaustive marker search, tombstones, backup notice,
//! idempotent replay, journal/telemetry integrity, provenance-only redaction and scope
//! isolation. Each `rp*` test is a regression for an audit finding on ba66a17.
mod common;
use common::*;
use excalibur_memory::MemoryPurgeParticipant;
use excalibur_retention::{
    BACKUP_NOTICE_CODE, Error, MAX_BACKUP_AGE_DAYS, PurgeBoundary, PurgeReason, PurgeService,
    REDACTION_MARK, SubjectKind,
};
use excalibur_store::rusqlite;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

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
        "excalibur.db:evidence:",
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
        assert_eq!(receipt.counts.runs_linked, 1, "{:?}", receipt.counts);
        assert_eq!(receipt.counts.hash_mismatches, 0);
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
    assert!(conversation.runs_reexported >= 1 && conversation.task_hashes_resealed >= 1);
    assert!(conversation.telemetry_offsets_resealed >= 1);
    assert!(document.blobs_removed >= 2 && document.journal_events_redacted >= 1);
    assert!(memory.vectors_deleted >= 1 && memory.edges_deleted == 1);
    assert!(memory.participant_rows_deleted >= 1);

    let tombstone_columns: Vec<String> = world
        .read(|c| {
            let mut s = c.prepare("SELECT name FROM pragma_table_info('retention_tombstones')")?;
            Ok(s.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await;
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
    let leftovers: (i64, i64, String, i64) = world
        .read(|c| {
            Ok((
                c.query_row("SELECT count(*) FROM retention_purge_needles", [], |r| {
                    r.get(0)
                })?,
                c.query_row("SELECT count(*) FROM retention_tombstones", [], |r| r.get(0))?,
                c.query_row("PRAGMA quick_check", [], |r| r.get(0))?,
                c.query_row("SELECT count(*) FROM retention_purge_events WHERE old_offset_hash IS NOT NULL OR new_offset_hash IS NOT NULL", [], |r| r.get(0))?,
            ))
        })
        .await;
    assert_eq!(leftovers, (0, 3, "ok".into(), 0));
    let vectors: (i64, i64) = world
        .read(|c| {
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
        .await;
    assert_eq!(vectors, (0, 0), "no vector of the purged scope remains");

    let bystander = planted.bystander_message.clone();
    let reply: (String, Option<String>) = world
        .read(move |c| {
            Ok(c.query_row(
                "SELECT text,reply_to FROM messages WHERE id=?1",
                [bystander],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await;
    assert_eq!(
        reply,
        ("An unrelated bystander message that stays.".into(), None)
    );

    // RP-2: the linked run still replays; structure is unchanged; payloads stay JSON.
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
    world
        .journal
        .set_state(&planted.run, "completed", None, "done")
        .await
        .unwrap();
    world
        .telemetry
        .reconcile(&world.journal, &planted.run)
        .await
        .unwrap();
    let run = planted.run.clone();
    let observed: Vec<(u64, String)> = world
        .read(move |c| {
            let mut s = c.prepare("SELECT sequence,payload_hash FROM task_observed_events WHERE run_id=?1 ORDER BY sequence")?;
            Ok(s.query_map([run], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await;
    assert_eq!(observed.len(), after_events.len());
    for ((sequence, hash), event) in observed.iter().zip(&after_events) {
        assert_eq!(*sequence, event.sequence);
        assert_eq!(
            hash,
            &sha256(&serde_json::to_vec(event).unwrap()),
            "sequence {sequence}"
        );
    }
    // RP-5: the archived observation still resolves by the hash the session holds.
    let recalled = world
        .journal
        .context_observation(&planted.run, &planted.observation.parse().unwrap())
        .await
        .unwrap();
    assert!(
        recalled["recalled"]
            .as_str()
            .unwrap()
            .contains(REDACTION_MARK)
    );

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

/// RP-1 and RP-7: other scopes' and unlinked runs stay byte-identical even when they
/// share whole sentences with the purged subjects; foreign keys answer `NotFound`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rp1_other_scopes_and_unlinked_runs_are_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::open(dir.path()).await;
    let a = plant(&world, SCOPE, "a").await;
    let b = plant(&world, OTHER, "b").await;
    // A legacy run with no provenance that quotes A's exact texts.
    let legacy = finished_run(
        &world,
        "legacy",
        &[conversation_text("a"), document_text("a"), memory_text("a")],
    )
    .await;
    {
        let legacy = legacy.clone();
        world
            .sql(move |c| {
                c.execute("INSERT INTO evidence(id,run_id,result,code_version) VALUES('evd_legacy',?1,?2,'abc123')", rusqlite::params![legacy, format!("{SHARED_DOCUMENT}: {}", conversation_text("a"))])?;
                Ok(())
            })
            .await;
    }
    let (before_b, before_legacy) = (
        run_snapshot(&world, &b.run).await,
        run_snapshot(&world, &legacy).await,
    );
    assert!(before_b.len() >= 8 && before_legacy.len() >= 3);
    let service = world.service().await;
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
    for request in a.requests(SCOPE) {
        service.purge(request).await.unwrap();
    }
    assert_eq!(
        run_snapshot(&world, &b.run).await,
        before_b,
        "scope B run changed"
    );
    assert_eq!(
        run_snapshot(&world, &legacy).await,
        before_legacy,
        "unlinked legacy run changed"
    );
    // A purged subject's key from another scope or principal is indistinguishable.
    let mut foreign = a.requests(SCOPE)[0].clone();
    foreign.scope = OTHER.into();
    assert!(matches!(service.purge(foreign).await, Err(Error::NotFound)));
    assert!(matches!(
        intruder.purge(a.requests(SCOPE)[0].clone()).await,
        Err(Error::NotFound)
    ));
    let mut conflicting = a.requests(SCOPE)[0].clone();
    conflicting.subject_id = a.document.clone();
    conflicting.kind = SubjectKind::Document;
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
    let memory = world
        .memory
        .memory(&grant(&[OTHER]), &b.memory)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(memory.statement, memory_text("b"));
    world.shutdown().await;
}

/// RP-2 and RP-3: a message that is serialized JSON or a run ID never produces invalid
/// JSON or rewrites identifiers; every run still replays and the purge completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rp2_json_and_identifier_texts_keep_every_payload_valid() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::open(dir.path()).await;
    let structured = r#"{"alpha":1,"beta":2,"gamma":3}"#.to_string();
    let unrelated = finished_run(&world, "unrelated", &[structured.clone()]).await;
    let linked = world
        .journal
        .create_run("linked", json!({"note": "start"}))
        .await
        .unwrap();
    world
        .journal
        .append(
            &linked,
            "tool_call",
            "call:1",
            json!({"arguments": {"alpha":1,"beta":2,"gamma":3}, "echo": structured, "child": unrelated}),
        )
        .await
        .unwrap();
    world
        .journal
        .set_state(&linked, "completed", None, "done")
        .await
        .unwrap();
    {
        let (linked, unrelated, structured) =
            (linked.clone(), unrelated.clone(), structured.clone());
        world
            .sql(move |c| {
                c.execute("INSERT INTO conversations(id,active_scope,scope_id,principal_id) VALUES('cnv_json',?1,?1,?2)", [SCOPE, PRINCIPAL])?;
                c.execute("INSERT INTO messages(id,conversation_id,text,scope_id,principal_id) VALUES('msg_json','cnv_json',?1,?2,?3)", [structured.as_str(), SCOPE, PRINCIPAL])?;
                c.execute("INSERT INTO messages(id,conversation_id,text,scope_id,principal_id) VALUES('msg_runid','cnv_json',?1,?2,?3)", [unrelated.as_str(), SCOPE, PRINCIPAL])?;
                c.execute("INSERT INTO memory_run_bindings(run_id,principal_id,scopes,primary_scope,conversation_id,role,bound_at) VALUES(?1,?2,'[\"general\"]','general','cnv_json','worker',1)", [linked.as_str(), PRINCIPAL])?;
                c.execute("CREATE TABLE IF NOT EXISTS task_observed_events(run_id TEXT NOT NULL,sequence INTEGER NOT NULL,payload_hash TEXT NOT NULL,PRIMARY KEY(run_id,sequence))", [])?;
                Ok(())
            })
            .await;
    }
    for run in [&linked, &unrelated] {
        let events = world.journal.replay(run, 0).await.unwrap();
        let run = run.clone();
        world
            .sql(move |c| {
                for event in &events {
                    c.execute(
                        "INSERT INTO task_observed_events VALUES(?1,?2,?3)",
                        rusqlite::params![
                            run,
                            event.sequence,
                            sha256(&serde_json::to_vec(event).unwrap())
                        ],
                    )?;
                }
                Ok(())
            })
            .await;
    }
    let before_unrelated = run_snapshot(&world, &unrelated).await;
    let service = world.service().await;
    let receipt = service
        .purge(request(
            SCOPE,
            SubjectKind::Conversation,
            "cnv_json",
            "json",
        ))
        .await
        .unwrap();
    assert!(
        receipt.counts.structured_matches >= 1,
        "{:?}",
        receipt.counts
    );
    let flags = service.flags("json").await.unwrap();
    assert!(
        flags.iter().any(|(code, _)| code == "structured_match"),
        "{flags:?}"
    );
    let events = world.journal.replay(&linked, 0).await.unwrap();
    let call = &events
        .iter()
        .find(|e| e.kind == "tool_call")
        .unwrap()
        .payload;
    assert_eq!(call["arguments"], json!({"alpha":1,"beta":2,"gamma":3}));
    assert_eq!(call["echo"], REDACTION_MARK);
    assert_eq!(call["child"], REDACTION_MARK, "string leaves are redacted");
    let run_ids: Vec<String> = world
        .read(|c| {
            Ok(c.prepare("SELECT run_id FROM journal")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await;
    assert!(
        run_ids.iter().all(|r| !r.contains(REDACTION_MARK)),
        "IDs are never rewritten"
    );
    assert_eq!(run_snapshot(&world, &unrelated).await, before_unrelated);
    world.journal.replay(&unrelated, 0).await.unwrap();
    assert!(service.resume_pending().await.unwrap().is_empty());
    world.shutdown().await;
}

/// RP-3: a failing purge reports its error and does not block later pending purges.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rp3_resume_pending_isolates_failures() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::open(dir.path()).await;
    let planted = plant(&world, SCOPE, "a").await;
    let requests = planted.requests(SCOPE);
    for request in [requests[0].clone(), requests[1].clone()] {
        let service = world.service().await.with_observer(Arc::new(|b| {
            if b == PurgeBoundary::Swept {
                panic!("interrupted after sweep")
            }
        }));
        let joined = tokio::spawn(async move { service.purge(request).await }).await;
        assert!(joined.unwrap_err().is_panic());
    }
    // The conversation now has an active linked run; the document does not.
    let active = world.journal.create_run("active", json!({})).await.unwrap();
    let service = world.service().await;
    service
        .link_run(SubjectKind::Conversation, &planted.conversation, &active)
        .await
        .unwrap();
    let outcomes = service.resume_pending().await.unwrap();
    assert_eq!(outcomes.len(), 2);
    let conversation = outcomes
        .iter()
        .find(|o| o.operation_key == requests[0].operation_key)
        .unwrap();
    assert!(
        conversation.result.as_ref().unwrap_err().contains("active"),
        "{:?}",
        conversation.result
    );
    let document = outcomes
        .iter()
        .find(|o| o.operation_key == requests[1].operation_key)
        .unwrap();
    assert!(document.result.is_ok(), "{:?}", document.result);
    let stuck: (String, Option<String>) = world
        .read({
            let key = requests[0].operation_key.clone();
            move |c| {
                Ok(c.query_row(
                    "SELECT phase,last_error FROM retention_purges WHERE operation_key=?1",
                    [key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            }
        })
        .await;
    assert_eq!(stuck.0, "swept");
    assert!(stuck.1.unwrap().starts_with("active_run:"));
    // Finishing the run unblocks it; nothing was deleted while it was active.
    assert!(
        world
            .read({
                let id = planted.conversation.clone();
                move |c| {
                    Ok(c.query_row(
                        "SELECT count(*) FROM conversations WHERE id=?1",
                        [id],
                        |r| r.get::<_, i64>(0),
                    )?)
                }
            })
            .await
            == 1
    );
    world
        .journal
        .set_state(&active, "cancelled", None, "stop")
        .await
        .unwrap();
    let outcomes = service.resume_pending().await.unwrap();
    assert!(outcomes.iter().all(|o| o.result.is_ok()), "{outcomes:?}");
    world.shutdown().await;
}

/// RP-6: an active linked run fences the purge before anything changes, and rows written
/// after the sweep are covered (journal and telemetry) when the purge resumes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rp6_active_runs_fence_and_late_rows_are_covered() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::open(dir.path()).await;
    let planted = plant(&world, SCOPE, "a").await;
    let active = world.journal.create_run("busy", json!({})).await.unwrap();
    let service = world.service().await;
    service
        .link_run(SubjectKind::Memory, &planted.memory, &active)
        .await
        .unwrap();
    let memory_request = planted.requests(SCOPE)[2].clone();
    assert!(matches!(
        service.purge(memory_request.clone()).await,
        Err(Error::ActiveRun(run)) if run == active
    ));
    assert!(
        service
            .receipt(&memory_request.operation_key)
            .await
            .unwrap()
            .is_none()
    );
    world
        .journal
        .set_state(&active, "completed", None, "end")
        .await
        .unwrap();
    // Interrupt the conversation purge after the sweep, then write late copies.
    let interrupted = world.service().await.with_observer(Arc::new(|b| {
        if b == PurgeBoundary::Swept {
            panic!("interrupted after sweep")
        }
    }));
    let conversation = planted.requests(SCOPE)[0].clone();
    let joined = tokio::spawn(async move { interrupted.purge(conversation).await }).await;
    assert!(joined.unwrap_err().is_panic());
    let late = world
        .journal
        .append(
            &planted.run,
            "late_note",
            "late:1",
            json!({"text": conversation_text("a")}),
        )
        .await
        .unwrap();
    assert!(late > 0);
    world
        .telemetry
        .reconcile(&world.journal, &planted.run)
        .await
        .unwrap();
    for outcome in service.resume_pending().await.unwrap() {
        outcome.result.unwrap();
    }
    for request in planted.requests(SCOPE) {
        service.purge(request).await.unwrap();
    }
    let hits = scan(dir.path(), &MARKERS);
    assert!(hits.is_empty(), "{hits:#?}");
    world
        .telemetry
        .reconcile(&world.journal, &planted.run)
        .await
        .unwrap();
    world.shutdown().await;
}

/// RP-9: a hash that no longer matched the pre-purge content is never re-sealed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rp9_tampered_hashes_are_flagged_not_resealed() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::open(dir.path()).await;
    let planted = plant(&world, SCOPE, "a").await;
    let tampered = "0".repeat(64);
    {
        let (run, tampered) = (planted.run.clone(), tampered.clone());
        world
            .sql(move |c| {
                c.execute("UPDATE task_observed_events SET payload_hash=?2 WHERE run_id=?1 AND sequence=1", [run.as_str(), tampered.as_str()])?;
                Ok(())
            })
            .await;
    }
    {
        let (run, tampered) = (planted.run.clone(), tampered.clone());
        world
            .telemetry
            .database()
            .write(excalibur_store::Priority::Normal, move |c| {
                c.execute_batch("DROP TRIGGER telemetry_offsets_no_update")?;
                c.execute("UPDATE telemetry_offsets SET payload_hash=?2 WHERE run_id=?1 AND source='journal' AND sequence=1", [run.as_str(), tampered.as_str()])?;
                c.execute_batch("CREATE TRIGGER telemetry_offsets_no_update BEFORE UPDATE ON telemetry_offsets BEGIN SELECT RAISE(ABORT,'telemetry offsets are append-only'); END;")?;
                Ok(())
            })
            .await
            .unwrap();
    }
    let service = world.service().await;
    let receipt = service
        .purge(planted.requests(SCOPE)[0].clone())
        .await
        .unwrap();
    assert!(receipt.counts.hash_mismatches >= 2, "{:?}", receipt.counts);
    let flags = service
        .flags(&planted.requests(SCOPE)[0].operation_key)
        .await
        .unwrap();
    let location = format!("{}:1", planted.run);
    assert!(
        flags.contains(&("task_hash_mismatch".into(), location.clone())),
        "{flags:?}"
    );
    assert!(
        flags.contains(&("telemetry_hash_mismatch".into(), location)),
        "{flags:?}"
    );
    let run = planted.run.clone();
    let kept: String = world
        .read(move |c| {
            Ok(c.query_row(
                "SELECT payload_hash FROM task_observed_events WHERE run_id=?1 AND sequence=1",
                [run],
                |r| r.get(0),
            )?)
        })
        .await;
    assert_eq!(kept, tampered);
    world.shutdown().await;
}

/// RP-4: base deletion runs in bounded commands with FTS5 secure-delete, without a
/// full index rebuild; unrelated index entries keep working.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rp4_base_deletion_is_bounded_and_incremental() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::open(dir.path()).await;
    world
        .sql(|c| {
            c.execute("INSERT INTO conversations(id,active_scope,scope_id,principal_id) VALUES('cnv_big','general','general',?1)", [PRINCIPAL])?;
            c.execute("INSERT INTO conversations(id,active_scope,scope_id,principal_id) VALUES('cnv_keep','general','general',?1)", [PRINCIPAL])?;
            for n in 0..300 {
                c.execute("INSERT INTO messages(id,conversation_id,text,scope_id,principal_id) VALUES(?1,'cnv_big',?2,'general',?3)", rusqlite::params![format!("msg_big_{n}"), format!("{CONVERSATION_MARK} bulk line number {n} of the long thread"), PRINCIPAL])?;
            }
            c.execute("INSERT INTO messages(id,conversation_id,text,scope_id,principal_id) VALUES('msg_keep','cnv_keep','zephyrkeepmark stays searchable','general',?1)", [PRINCIPAL])?;
            Ok(())
        })
        .await;
    let batches = Arc::new(AtomicUsize::new(0));
    let counter = batches.clone();
    let service = world.service().await.with_observer(Arc::new(move |b| {
        if b == PurgeBoundary::BaseDeleteBatch {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    }));
    let receipt = service
        .purge(request(SCOPE, SubjectKind::Conversation, "cnv_big", "big"))
        .await
        .unwrap();
    assert!(
        batches.load(Ordering::SeqCst) >= 5,
        "300 messages need several commands"
    );
    assert!(receipt.counts.base_batches >= 6, "{:?}", receipt.counts);
    assert!(fts_hits(dir.path(), &[CONVERSATION_MARK]).is_empty());
    assert!(scan(dir.path(), &[CONVERSATION_MARK]).is_empty());
    let kept: (i64, Option<i64>) = world
        .read(|c| {
            Ok((
                c.query_row(
                    "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'zephyrkeepmark'",
                    [],
                    |r| r.get(0),
                )?,
                c.query_row(
                    "SELECT v FROM messages_fts_config WHERE k='secure-delete'",
                    [],
                    |r| r.get(0),
                )
                .ok(),
            ))
        })
        .await;
    assert_eq!(kept.0, 1, "other rows stay indexed");
    assert_eq!(
        kept.1,
        Some(0),
        "FTS5 secure-delete is restored after the purge"
    );
    world.shutdown().await;
}

/// RP-8: `purge_memory` resumes a purge interrupted after the memory row was deleted,
/// and covers telemetry once the host supplied its handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rp8_memory_store_purge_resumes_and_covers_telemetry() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::open(dir.path()).await;
    let planted = plant(&world, SCOPE, "a").await;
    let bare = PurgeService::open(PRINCIPAL, world.main.clone(), None)
        .await
        .unwrap();
    assert!(matches!(
        bare.purge(request(SCOPE, SubjectKind::Memory, &planted.memory, "bare"))
            .await,
        Err(Error::ParticipantMissing("memory"))
    ));
    let key = format!("memory:{}", planted.memory);
    let interrupted = world.service().await.with_observer(Arc::new(|b| {
        if b == PurgeBoundary::BaseDeleted {
            panic!("crash after base deletion")
        }
    }));
    let req = request(SCOPE, SubjectKind::Memory, &planted.memory, &key);
    let joined = tokio::spawn(async move { interrupted.purge(req).await }).await;
    assert!(joined.unwrap_err().is_panic());
    assert!(
        world
            .memory
            .memory(&grant(&[SCOPE]), &planted.memory)
            .await
            .unwrap()
            .is_none()
    );
    world
        .memory
        .purge_memory(&grant(&[SCOPE]), &planted.memory, NOW)
        .await
        .unwrap();
    let receipt = bare.receipt(&key).await.unwrap().unwrap();
    assert!(receipt.telemetry_covered);
    assert!(scan(dir.path(), &[MEMORY_MARK]).is_empty());
    // A caller without the scope cannot resume or probe it.
    assert!(matches!(
        world
            .memory
            .purge_memory(&grant(&[OTHER]), &planted.memory, NOW)
            .await,
        Err(excalibur_memory::Error::NotFound(_))
    ));
    let outcomes = world.memory.resume_pending_purges(PRINCIPAL).await.unwrap();
    assert!(outcomes.is_empty());
    world.shutdown().await;
}
