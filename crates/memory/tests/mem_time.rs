//! Suite `mem-time`: bitemporal answers, read-time expiry and relative dates in the user's zone.
mod common;
use chrono::TimeZone;
use common::*;
use excalibur_memory::*;

fn utc(y: i32, m: u32, d: u32) -> i64 {
    chrono::Utc
        .with_ymd_and_hms(y, m, d, 12, 0, 0)
        .unwrap()
        .timestamp()
}

#[tokio::test]
async fn true_at_and_known_at_answer_differently() {
    let f = fixture().await;
    let g = grant(&["general"]);
    let mut lima = input("general", MemoryKind::Fact, "Ana lives in Lima");
    lima.entities = vec!["Ana".into()];
    lima.attribute = Some("residence".into());
    lima.valid_from = Some(utc(2026, 1, 1));
    let first = write(&f.store, &g, lima, utc(2026, 1, 5)).await;
    let mut cusco = input("general", MemoryKind::Fact, "Ana now lives in Cusco");
    cusco.entities = vec!["Ana".into()];
    cusco.attribute = Some("residence".into());
    cusco.valid_from = Some(utc(2026, 2, 15));
    let second = write(&f.store, &g, cusco, utc(2026, 3, 10)).await;
    assert_eq!(second.decision, AdmissionDecision::Contradiction);
    assert_eq!(second.status, MemoryStatus::Active);
    let old = f.store.memory(&g, &first.memory_id).await.unwrap().unwrap();
    assert_eq!(old.status, MemoryStatus::Superseded);
    assert_eq!(old.valid_to, Some(utc(2026, 2, 15)));

    let now = utc(2026, 10, 30);
    let true_on = search(
        &f.store,
        &g,
        "Where did Ana live, what was true on 1 March 2026?",
        now,
    )
    .await;
    assert_eq!(
        true_on.analysis.time_mode,
        TimeMode::ValidAt(
            chrono_tz::UTC
                .with_ymd_and_hms(2026, 3, 2, 0, 0, 0)
                .unwrap()
                .timestamp()
                - 1
        )
    );
    assert_eq!(ids(&true_on).first(), Some(&second.memory_id));
    assert!(!ids(&true_on).contains(&first.memory_id));
    let believed = search(
        &f.store,
        &g,
        "Where did we believe Ana lived on 1 March 2026?",
        now,
    )
    .await;
    assert!(matches!(believed.analysis.time_mode, TimeMode::KnownAt(_)));
    assert_eq!(ids(&believed).first(), Some(&first.memory_id));
    assert!(!ids(&believed).contains(&second.memory_id));
    let current = search(&f.store, &g, "Where does Ana live?", now).await;
    assert_eq!(ids(&current), vec![second.memory_id.clone()]);
    let history = search(&f.store, &g, "Where did Ana live before? history", now).await;
    assert!(ids(&history).contains(&first.memory_id) && ids(&history).contains(&second.memory_id));
}

#[tokio::test]
async fn expiry_applies_at_read_time_before_the_daily_review() {
    let f = fixture().await;
    let g = grant(&["general"]);
    let mut temp = input(
        "general",
        MemoryKind::Commitment,
        "Office door code is 4417 this week",
    );
    temp.expires_at = Some(NOW - 60);
    let written = write(&f.store, &g, temp, NOW - 3 * DAY).await;
    assert!(
        search(&f.store, &g, "office door code", NOW)
            .await
            .items
            .is_empty()
    );
    let row = f
        .store
        .memory(&g, &written.memory_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.status,
        MemoryStatus::Active,
        "row untouched until consolidation"
    );
    let mut historical = RetrievalQuery::new("office door code");
    historical.time_mode = Some(TimeMode::Historical);
    assert_eq!(
        ids(&f.store.retrieve(&g, historical, NOW).await.unwrap()),
        vec![written.memory_id.clone()]
    );
    assert_eq!(f.store.expire("owner", "general", NOW).await.unwrap(), 1);
    let row = f
        .store
        .memory(&g, &written.memory_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, MemoryStatus::Expired);
    assert_eq!(row.valid_to, Some(NOW - 60));
    let states: Vec<String> = f
        .store
        .transitions(&g, &written.memory_id)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.1)
        .collect();
    assert_eq!(states, vec!["candidate", "active", "expired"]);
    assert_eq!(row.sources, vec![SourceRef::message("msg-src")]);
}

#[test]
fn relative_dates_use_the_user_time_zone() {
    let lima: chrono_tz::Tz = "America/Lima".parse().unwrap();
    // Friday 2026-10-30 03:00 UTC is still Thursday 29 in Lima.
    let now = chrono::Utc
        .with_ymd_and_hms(2026, 10, 30, 3, 0, 0)
        .unwrap()
        .timestamp();
    let local = |y, m, d| lima.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap().timestamp();
    assert_eq!(
        parse_interval("¿qué pasó ayer?", lima, now),
        Some((local(2026, 10, 28), local(2026, 10, 29)))
    );
    assert_eq!(
        parse_interval("what did I do last week", lima, now),
        Some((local(2026, 10, 19), local(2026, 10, 26)))
    );
    assert_eq!(
        parse_interval("la semana pasada", lima, now),
        parse_interval("last week", lima, now)
    );
    assert_eq!(
        parse_interval("en marzo", lima, now),
        Some((local(2026, 3, 1), local(2026, 4, 1)))
    );
    assert_eq!(
        parse_interval("in December", lima, now),
        Some((local(2025, 12, 1), local(2026, 1, 1)))
    );
    assert_eq!(
        parse_interval("el 1 de marzo de 2026", lima, now),
        Some((local(2026, 3, 1), local(2026, 3, 2)))
    );
    assert_eq!(
        parse_interval("hace 3 días", lima, now),
        Some((local(2026, 10, 26), local(2026, 10, 27)))
    );
    assert_eq!(
        parse_interval("2026-02-14", lima, now),
        Some((local(2026, 2, 14), local(2026, 2, 15)))
    );
    assert_eq!(parse_interval("no date here", lima, now), None);
}

#[tokio::test]
async fn temporal_list_finds_events_and_messages_in_the_interval() {
    let f = fixture().await;
    let g = grant(&["general"]);
    let mut event = input(
        "general",
        MemoryKind::Event,
        "Quarterly sync with the vendor",
    );
    event.valid_from = Some(NOW - DAY);
    let e = write(&f.store, &g, event, NOW - DAY).await;
    let mut old = input(
        "general",
        MemoryKind::Event,
        "Quarterly sync with the auditors",
    );
    old.valid_from = Some(NOW - 40 * DAY);
    let o = write(&f.store, &g, old, NOW - 40 * DAY).await;
    message(
        &f.db,
        "m-y",
        "c",
        "general",
        "user",
        "notes from the sync",
        NOW - DAY + 100,
    )
    .await;
    let result = search(&f.store, &g, "what happened yesterday?", NOW).await;
    assert!(result.analysis.interval.is_some());
    assert!(ids(&result).contains(&e.memory_id) && ids(&result).contains(&"m-y".to_string()));
    assert!(!ids(&result).contains(&o.memory_id));
    let quarterly = search(&f.store, &g, "quarterly sync yesterday", NOW).await;
    assert_eq!(ids(&quarterly).first(), Some(&e.memory_id));
    assert!(
        !ids(&quarterly).contains(&o.memory_id),
        "dated record outside the interval is filtered"
    );
}
