mod common;
use common::*;
use excalibur_engine::{DurableSink, Engine, Status};
use excalibur_journal::{Event, Journal};
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use excalibur_types::runtime::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
struct Sink {
    fail_intent: AtomicBool,
    seen: Mutex<Vec<Event>>,
}
impl DurableSink for Sink {
    fn record<'a>(&'a self, _: &'a str, event: &'a Event) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if event.kind == "tool_intent" && self.fail_intent.swap(false, Ordering::SeqCst) {
                return Err("synthetic export interruption".into());
            }
            let mut seen = self.seen.lock().unwrap();
            if let Some(previous) = seen.iter().find(|e| e.sequence == event.sequence) {
                assert_eq!(previous, event);
            } else {
                seen.push(event.clone());
            }
            Ok(())
        })
    }
}
#[tokio::test]
async fn projection_failure_prevents_effect_and_recovery_has_no_event_gaps_or_duplicate_effects() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let journal = Journal::new(db.clone()).await.unwrap();
    let sink = Arc::new(Sink {
        fail_intent: AtomicBool::new(true),
        seen: Mutex::new(vec![]),
    });
    let engine = Engine::new(
        journal.clone(),
        model(temp.path(), false, 1),
        Arc::new(Effects {
            root: temp.path().into(),
            effect: Effect::Idempotent,
            journal: journal.clone(),
        }),
    )
    .with_durable_sink(sink.clone());
    let run = engine
        .create("projection-fixture", spec(false, 5))
        .await
        .unwrap();
    let paused = engine.run(&run).await.unwrap();
    assert_eq!(paused.status, Status::Paused);
    assert_eq!(count(&temp.path().join("effects")), 0);
    assert_eq!(count(&temp.path().join("models")), 1);
    engine.resume(&run).await.unwrap();
    assert_eq!(engine.run(&run).await.unwrap().status, Status::Completed);
    assert_eq!(count(&temp.path().join("effects")), 1);
    assert_eq!(count(&temp.path().join("models")), 2);
    engine.flush_events(&run).await.unwrap();
    let expected = journal.replay(&run, 0).await.unwrap();
    assert_eq!(*sink.seen.lock().unwrap(), expected);
    let recovered = Arc::new(Sink {
        fail_intent: AtomicBool::new(false),
        seen: Mutex::new(vec![]),
    });
    let reread = engine.clone().with_durable_sink(recovered.clone());
    reread.flush_events(&run).await.unwrap();
    reread.flush_events(&run).await.unwrap();
    let expected = journal.replay(&run, 0).await.unwrap();
    assert_eq!(*recovered.seen.lock().unwrap(), expected);
    db.shutdown().await.unwrap();
}
