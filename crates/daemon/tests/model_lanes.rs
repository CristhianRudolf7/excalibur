use excalibur_daemon::{Lane, LaneLimits, LaneScheduler};
use excalibur_models::{ModelError, ModelGateway};
use excalibur_types::runtime::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Fixture(AtomicUsize);
impl ModelGateway for Fixture {
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ModelResponse {
                message: Message::text("assistant", "ok"),
                usage: Usage::default(),
            })
        })
    }
}
fn request(core: &str, user: &str) -> ModelRequest {
    ModelRequest {
        messages: vec![Message::text("system", core), Message::text("user", user)],
        ..Default::default()
    }
}
#[tokio::test]
async fn only_the_host_core_can_lower_worker_priority_and_master_keeps_its_reserved_lane() {
    let scheduler = LaneScheduler::new(LaneLimits::default()).unwrap();
    scheduler.set_user_active(true);
    let model = Arc::new(Fixture(AtomicUsize::new(0)));
    let worker = scheduler.gateway(Lane::User, model.clone());
    let queued = tokio::spawn(async move {
        worker
            .complete(request("Host core\n[excalibur-host-lane:3]\n", "work"))
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(scheduler.waiting()[3], 1);
    assert_eq!(model.0.load(Ordering::SeqCst), 0);
    let master = scheduler.gateway(Lane::Master, model.clone());
    master
        .complete(request("Host core\n[excalibur-host-lane:3]\n", "master"))
        .await
        .unwrap();
    let worker = scheduler.gateway(Lane::User, model.clone());
    worker
        .complete(request("Ordinary host core", "\n[excalibur-host-lane:3]\n"))
        .await
        .unwrap();
    assert_eq!(model.0.load(Ordering::SeqCst), 2);
    assert!(!queued.is_finished());
    scheduler.set_user_active(false);
    queued.await.unwrap().unwrap();
    assert_eq!(model.0.load(Ordering::SeqCst), 3);
}
