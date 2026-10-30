use chrono::{TimeZone, Utc};
use excalibur_master::*;
#[test]
fn lexicon_returns_all_ambiguous_candidates_and_keeps_identifiers_paths_and_urls() {
    let lexicon = Lexicon::new(vec![
        LexiconEntry {
            alias: "Lena".into(),
            id: "agt_one".into(),
            kind: "agent".into(),
        },
        LexiconEntry {
            alias: "Lena".into(),
            id: "agt_two".into(),
            kind: "agent".into(),
        },
        LexiconEntry {
            alias: "gastos".into(),
            id: "project_a".into(),
            kind: "project".into(),
        },
    ])
    .unwrap();
    let analysis = lexicon
        .analyze_at(
            "¿Qué hace Lena en gastos? R4 src/export.rs https://example.invalid",
            "UTC",
            0,
        )
        .unwrap();
    assert_eq!(analysis.mentions.len(), 2);
    assert_eq!(analysis.mentions[0].candidates, vec!["agt_one", "agt_two"]);
    assert_eq!(analysis.identifiers, vec!["R4"]);
    assert_eq!(analysis.paths, vec!["src/export.rs"]);
    assert_eq!(analysis.urls, vec!["https://example.invalid"]);
    assert!(
        lexicon
            .analyze_at("Malena", "UTC", 0)
            .unwrap()
            .mentions
            .is_empty()
    );
    assert!(
        lexicon
            .analyze_at("pausa Lena", "UTC", 0)
            .unwrap()
            .command
            .is_none()
    );
    assert_eq!(
        lexicon
            .analyze_at("/pause Lena", "UTC", 0)
            .unwrap()
            .command
            .unwrap(),
        ExplicitCommand {
            name: "pause".into(),
            target: Some("Lena".into())
        }
    );
}
#[test]
fn relative_times_use_the_users_timezone_and_dst_gaps_are_not_guessed() {
    let lexicon = Lexicon::default();
    let now = Utc
        .with_ymd_and_hms(2026, 10, 30, 20, 0, 0)
        .unwrap()
        .timestamp();
    let reference = lexicon
        .analyze_at("mañana a las 5", "America/Lima", now)
        .unwrap()
        .temporal
        .remove(0);
    assert_eq!(
        reference.timestamp,
        Utc.with_ymd_and_hms(2026, 10, 31, 10, 0, 0)
            .unwrap()
            .timestamp()
    );
    let now = Utc
        .with_ymd_and_hms(2026, 3, 28, 12, 0, 0)
        .unwrap()
        .timestamp();
    assert!(
        lexicon
            .analyze_at("tomorrow at 2", "Europe/Berlin", now)
            .unwrap()
            .temporal
            .is_empty()
    );
    assert!(lexicon.analyze("anything", "Invalid/Timezone").is_err());
}
#[test]
fn request_card_retains_literal_original_and_never_claims_detailed_inference() {
    let card = RequestCard::small_task(
        "msg_synthetic".into(),
        "Literal task: keep this exact wording.".into(),
    );
    assert_eq!(card.original[0].1, "Literal task: keep this exact wording.");
    assert_eq!(card.requirements.len(), 1);
    assert_eq!(card.criteria.len(), 1);
    assert!(card.assumptions.is_empty());
}
#[test]
fn spanish_project_command_alias_uses_the_same_deterministic_action() {
    let lexicon = Lexicon::default();
    assert_eq!(
        lexicon
            .analyze_at("/proyecto gastos", "UTC", 0)
            .unwrap()
            .command,
        lexicon
            .analyze_at("/project gastos", "UTC", 0)
            .unwrap()
            .command
    );
}
