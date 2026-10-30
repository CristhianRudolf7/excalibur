//! Host service boundaries: transient work is retried, panics and writer death
//! initiate controlled shutdown. Diagnostics never include workload payloads.
use excalibur_store::{Database, StoreEvent};
use futures::FutureExt;
use std::{future::Future, panic::AssertUnwindSafe, sync::Arc, time::Duration};
use tokio::{
    runtime::Handle,
    sync::{Notify, watch},
    task::JoinHandle,
};
pub fn supervise<F, Fut>(
    runtime: &Handle,
    name: &'static str,
    shutdown: Arc<Notify>,
    mut stop: watch::Receiver<bool>,
    mut iteration: F,
) -> JoinHandle<Result<(), String>>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    runtime.spawn(async move {
        let mut failures = 0u32;
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            let outcome = tokio::select! {
                _ = stop.changed() => return Ok(()),
                outcome = AssertUnwindSafe(async { iteration().await }).catch_unwind() => outcome,
            };
            match outcome {
                Ok(Ok(())) => failures = 0,
                Ok(Err(_)) => {
                    failures = failures.saturating_add(1);
                    eprintln!("daemon service {name}: iteration unavailable; retrying");
                }
                Err(_) => {
                    shutdown.notify_one();
                    return Err(format!("daemon service {name} panicked"));
                }
            }
            let delay =
                Duration::from_secs(1u64.checked_shl(failures.min(5)).unwrap_or(30).min(30));
            tokio::select! {_=stop.changed()=>return Ok(()),_=tokio::time::sleep(delay)=>{}}
        }
    })
}
pub fn supervise_writer(
    runtime: &Handle,
    db: Database,
    shutdown: Arc<Notify>,
    mut stop: watch::Receiver<bool>,
) -> JoinHandle<Result<(), String>> {
    let mut events = db.subscribe_events();
    runtime.spawn(async move{
        loop{
            if db.is_failed(){shutdown.notify_one();return Err("principal_writer_failed".into());}
            tokio::select!{
                _=stop.changed()=>return Ok(()),
                event=events.recv()=>match event {
                    Ok(StoreEvent::WriterFailed)=>{shutdown.notify_one();return Err("principal_writer_failed".into());},
                    Err(tokio::sync::broadcast::error::RecvError::Closed)=>{shutdown.notify_one();return Err("principal_writer_events_closed".into());},
                    _=>{}
                }
            }
        }
    })
}
