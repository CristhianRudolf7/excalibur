//! Background review: activity histogram in the user's zone, yielding and resuming by batch,
//! single-scope prompts, the 25 % compaction guard, enriched keys and learned relation types.
mod common;
use chrono::TimeZone;
use common::*;
use excalibur_memory::{testing::*, *};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn lima() -> chrono_tz::Tz {
    "America/Lima".parse().unwrap()
}
fn local(day: u32, hour: u32) -> i64 {
    lima()
        .with_ymd_and_hms(2026, 10, day, hour, 0, 0)
        .unwrap()
        .timestamp()
}

#[tokio::test]
async fn schedule_starts_light_and_then_uses_the_quietest_local_window() {
    let f = fixture().await;
    let budget = ReviewBudget {
        estimated_hours: 2,
        ..Default::default()
    };
    for day in 20..24 {
        f.store
            .record_activity("owner", local(day, 9), "run")
            .await
            .unwrap();
    }
    // Four active days, 03:45 local, last activity long ago: the light nightly pass is due.
    let light = f
        .store
        .plan_review("owner", lima(), &budget, local(25, 3) + 45 * 60)
        .await
        .unwrap();
    assert_eq!(
        (
            light.mode,
            light.window_start_hour,
            light.window_minute,
            light.due_now
        ),
        (ScheduleMode::Light, 3, 30, true)
    );
    let busy = local(23, 3) + 40 * 60;
    f.store
        .record_activity("owner", busy - 600, "run")
        .await
        .unwrap();
    assert!(
        !f.store
            .plan_review("owner", lima(), &budget, busy)
            .await
            .unwrap()
            .due_now,
        "needs two idle hours"
    );

    // Fifth active day; activity everywhere except 13:00-15:00 local.
    for day in 24..29 {
        for hour in (0..24).filter(|h| !(13..15).contains(h)) {
            f.store
                .record_activity("owner", local(day, hour), "message")
                .await
                .unwrap();
        }
    }
    let plan = f
        .store
        .plan_review("owner", lima(), &budget, local(29, 13) + 600)
        .await
        .unwrap();
    assert_eq!(
        (plan.mode, plan.window_start_hour, plan.window_hours),
        (ScheduleMode::Histogram, 13, 2)
    );
    assert!(plan.due_now && plan.active_days >= 5);
    assert_eq!(plan.histogram[13], 0);
    // Weekly recomputation: within a week the stored window is reused.
    for hour in [13, 14] {
        f.store
            .record_activity("owner", local(29, hour), "message")
            .await
            .unwrap();
    }
    assert_eq!(
        f.store
            .plan_review("owner", lima(), &budget, local(29, 16))
            .await
            .unwrap()
            .window_start_hour,
        13
    );
}

fn fact_per_turn(request: &ExtractionRequest) -> ExtractionOutput {
    let facts = request
        .turns
        .iter()
        .filter(|t| t.role == "user")
        .map(|t| ExtractedFact {
            message_id: t.message_id.clone(),
            kind: MemoryKind::Fact,
            statement: format!("Noted: {}", t.text),
            entities: vec![],
            attribute: None,
            valid_from: None,
            expires_at: None,
            temporal_change: false,
            confidence: None,
            importance: None,
            relations: vec![],
        })
        .collect();
    ExtractionOutput {
        facts,
        tokens_used: 100,
    }
}

struct StopAfter {
    extractor: ScriptedExtractor,
    flag: Arc<AtomicBool>,
}
impl Extractor for StopAfter {
    fn extract<'a>(
        &'a self,
        request: &'a ExtractionRequest,
    ) -> BoxFuture<'a, std::result::Result<ExtractionOutput, String>> {
        // The user becomes active while the first batch is processed.
        self.flag.store(true, Ordering::SeqCst);
        self.extractor.extract(request)
    }
}

#[tokio::test]
async fn review_yields_to_the_user_resumes_from_its_batch_and_never_mixes_scopes() {
    let f = fixture().await;
    for i in 0..5 {
        message(
            &f.db,
            &format!("a{i}"),
            "ca",
            "project:a",
            "user",
            &format!("alpha detail {i}"),
            NOW - 3600 + i,
        )
        .await;
        message(
            &f.db,
            &format!("b{i}"),
            "cb",
            "project:b",
            "user",
            &format!("beta detail {i}"),
            NOW - 3600 + i,
        )
        .await;
    }
    let active = Arc::new(AtomicBool::new(false));
    let roles = ConsolidationRoles {
        extractor: Arc::new(StopAfter {
            extractor: ScriptedExtractor::new(fact_per_turn),
            flag: active.clone(),
        }),
        compactor: None,
    };
    let budget = ReviewBudget {
        batch_size: 2,
        ..Default::default()
    };
    let scopes = vec!["project:a".to_string(), "project:b".to_string()];
    let first = f
        .store
        .run_review("owner", &scopes, &roles, &budget, active.as_ref(), NOW)
        .await
        .unwrap();
    assert_eq!(first.state, ReviewState::Yielded);
    assert_eq!(
        first.report["scopes"]["project:a"]["turns"], 2,
        "one batch, then yield"
    );
    active.store(false, Ordering::SeqCst);
    let calm = ConsolidationRoles {
        extractor: Arc::new(ScriptedExtractor::new(fact_per_turn)),
        compactor: None,
    };
    let second = f
        .store
        .run_review("owner", &scopes, &calm, &budget, active.as_ref(), NOW + 60)
        .await
        .unwrap();
    assert_eq!(
        (second.run_id.as_str(), second.state),
        (first.run_id.as_str(), ReviewState::Completed)
    );
    assert_eq!(second.report["scopes"]["project:a"]["turns"], 5);
    assert_eq!(second.report["scopes"]["project:b"]["admitted"], 5);
    for (scope, scopes) in f.store.review_prompt_scopes(&first.run_id).await.unwrap() {
        assert_eq!(scopes, vec![scope]);
    }
    let report = f
        .store
        .review_report(&grant(&["project:a"]), &first.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report["state"], "completed");
    // Enriched keys: the extracted fact also points back to its original turn.
    let found = search(
        &f.store,
        &grant(&["project:a"]),
        "Noted alpha detail 3",
        NOW + 60,
    )
    .await;
    assert!(
        found
            .items
            .iter()
            .any(|i| i.id == "a3" && i.kind == ItemKind::Message)
    );
    assert!(found.items.iter().all(|i| i.scope == "project:a"));
}

#[tokio::test]
async fn compaction_rejects_rewrites_that_drop_more_than_a_quarter() {
    for (keep, rejected) in [(2usize, true), (3, false)] {
        let f = fixture().await;
        let g = grant(&["general"]);
        for i in 0..4 {
            f.store
                .doc_edit(
                    &g,
                    &DocName::Notebook,
                    DocEdit::Append {
                        heading_path: vec!["Log".into()],
                        scope: Some("general".into()),
                        text: format!("entry {i}"),
                    },
                    EditContext {
                        actor: "r".into(),
                        provenance: None,
                        explicit_user_request: false,
                    },
                    NOW,
                )
                .await
                .unwrap();
        }
        let roles = ConsolidationRoles {
            extractor: Arc::new(ScriptedExtractor::new(|_| ExtractionOutput::default())),
            compactor: Some(Arc::new(KeepFirstCompactor { keep })),
        };
        let idle = AtomicBool::new(false);
        let out = f
            .store
            .run_review(
                "owner",
                &["general".into()],
                &roles,
                &ReviewBudget::default(),
                &idle,
                NOW,
            )
            .await
            .unwrap();
        assert_eq!(
            out.report["scopes"]["general"]["compaction"]["rejected"], rejected,
            "{}",
            out.report
        );
        let DocReadResult::Index { headings } = f
            .store
            .doc_read(&g, &DocName::Notebook, DocSelector::Index)
            .await
            .unwrap()
        else {
            panic!()
        };
        let timeline = f.store.notebook_timeline(&g, "general").await.unwrap();
        if rejected {
            assert_eq!((headings.len(), timeline.len()), (4, 0));
        } else {
            assert_eq!(
                (headings.len(), timeline.len()),
                (4, 1),
                "3 kept + compiled truth; removed entry kept in the timeline"
            );
            assert_eq!(timeline[0].1, "entry 3");
        }
    }
}

#[tokio::test]
async fn segments_close_by_inactivity_and_extraction_is_scoped() {
    let f = fixture().await;
    let g = grant(&["general"]).with_conversation("c1");
    message(
        &f.db,
        "m1",
        "c1",
        "general",
        "user",
        "My sister Lucia studies chemistry",
        NOW - 2000,
    )
    .await;
    message(
        &f.db,
        "m2",
        "c1",
        "general",
        "tool",
        "Web page says Lucia prefers tea",
        NOW - 1900,
    )
    .await;
    assert!(f.store.segment_idle(&g, "c1", NOW).await.unwrap());
    assert!(!f.store.segment_idle(&g, "c1", NOW - 1000).await.unwrap());
    let extractor = ScriptedExtractor::new(|r| ExtractionOutput {
        facts: r
            .turns
            .iter()
            .map(|t| ExtractedFact {
                message_id: t.message_id.clone(),
                kind: MemoryKind::Preference,
                statement: t.text.clone(),
                entities: vec!["Lucia".into()],
                attribute: None,
                valid_from: None,
                expires_at: None,
                temporal_change: false,
                confidence: None,
                importance: None,
                relations: vec![],
            })
            .chain([ExtractedFact {
                message_id: "not-in-batch".into(),
                kind: MemoryKind::Fact,
                statement: "forged".into(),
                entities: vec![],
                attribute: None,
                valid_from: None,
                expires_at: None,
                temporal_change: false,
                confidence: None,
                importance: None,
                relations: vec![],
            }])
            .collect(),
        tokens_used: 5,
    });
    let admitted = f
        .store
        .close_segment(&g, "general", &extractor, NOW)
        .await
        .unwrap();
    assert_eq!(admitted.len(), 2);
    let tool = f.store.memory(&g, &admitted[1]).await.unwrap().unwrap();
    assert_eq!(
        (tool.trust, tool.kind),
        (Trust::UntrustedExternal, MemoryKind::Fact)
    );
    assert_eq!(
        f.store
            .memory(&g, &admitted[0])
            .await
            .unwrap()
            .unwrap()
            .trust,
        Trust::User
    );
    assert!(
        f.store
            .close_segment(&g, "general", &extractor, NOW)
            .await
            .unwrap()
            .is_empty(),
        "watermark prevents reprocessing"
    );
}

#[tokio::test]
async fn relation_types_are_learned_after_repeated_needs_and_activated_by_use() {
    let f = fixture().await;
    let g = grant(&["lab"]);
    for i in 0..5 {
        let out = f
            .store
            .relate(
                &g,
                "lab",
                &format!("compound {i}"),
                "synthesizes",
                "catalyst",
                None,
                "m",
                NOW - i64::from(i) * DAY,
            )
            .await
            .unwrap();
        assert!(matches!(out, RelateOutcome::NeedRecorded { .. }));
    }
    assert_eq!(
        f.store
            .consolidate_relation_types("owner", NOW)
            .await
            .unwrap(),
        vec!["synthesizes".to_string()]
    );
    let kind =
        |types: Vec<RelationType>| types.into_iter().find(|t| t.name == "synthesizes").unwrap();
    assert_eq!(
        kind(f.store.relation_types(&g).await.unwrap()).state,
        "proposed"
    );
    for i in 0..5 {
        assert!(matches!(
            f.store
                .relate(
                    &g,
                    "lab",
                    &format!("compound {i}"),
                    "synthesizes",
                    "catalyst",
                    None,
                    "m",
                    NOW
                )
                .await
                .unwrap(),
            RelateOutcome::Linked { .. }
        ));
    }
    f.store
        .consolidate_relation_types("owner", NOW)
        .await
        .unwrap();
    let learned = kind(f.store.relation_types(&g).await.unwrap());
    assert_eq!(
        (learned.state.as_str(), learned.origin.as_str()),
        ("active", "learned")
    );
    f.store
        .merge_relation_type(&g, "synthesizes", "part_of", NOW)
        .await
        .unwrap();
    let RelateOutcome::Linked { relation, .. } = f
        .store
        .relate(&g, "lab", "x", "synthesizes", "y", None, "m", NOW)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(relation, "part_of");
}
