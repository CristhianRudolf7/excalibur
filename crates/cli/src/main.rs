use excalibur_cli::{HELP, model, parse};
use excalibur_eval::Execution;
use std::io::{self, Write};
fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|a| a == "daemon") {
        let code = excalibur_daemon::command::run(&args[1..]);
        std::process::exit(code);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime initialization");
    let code = runtime.block_on(execute());
    std::process::exit(code);
}
async fn execute() -> i32 {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.as_slice() == ["--version"] {
        println!(
            "excalibur {} ({})",
            env!("CARGO_PKG_VERSION"),
            option_env!("EXCALIBUR_BUILD_COMMIT").unwrap_or("unknown commit")
        );
        return 0;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return 0;
    }
    let mut command = match parse(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "{}",
                excalibur_vault::Redactor::default().redact_text(&e.to_string())
            );
            return 1;
        }
    };
    let (gateway, redactor) = match model(&mut command, |name| std::env::var(name).ok()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "{}",
                excalibur_vault::Redactor::default().redact_text(&e.to_string())
            );
            return 1;
        }
    };
    let execution = match Execution::prepare(command.options, gateway, redactor).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "{}",
                excalibur_vault::Redactor::default().redact_text(&e.to_string())
            );
            return 1;
        }
    };
    let telemetry = execution.telemetry.clone();
    let telemetry_lifecycle = telemetry.clone();
    let (done, mut finished) = tokio::sync::watch::channel(false);
    let json = command.json;
    let output = tokio::spawn(async move {
        if !json {
            return Ok::<(), String>(());
        }
        let mut subscription = telemetry.subscribe(0);
        let mut cursor = 0;
        loop {
            tokio::select! {
             event=subscription.next()=>{let event=event.map_err(|_|"event subscription failed")?;cursor=event.cursor;write_event(&event)?;},
             _=finished.changed()=>{loop{let events=telemetry.events_after(cursor,256).await.map_err(|_|"event drain failed")?;if events.is_empty(){return Ok(());}for event in events{cursor=event.cursor;write_event(&event)?;}}}
            }
        }
    });
    let (cancel, receiver) = tokio::sync::watch::channel(false);
    #[cfg(unix)]
    let (mut interrupt, mut terminate) = match (
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()),
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()),
    ) {
        (Ok(interrupt), Ok(terminate)) => (interrupt, terminate),
        _ => {
            eprintln!("signal handlers could not be installed");
            return 1;
        }
    };
    let signals = tokio::spawn(async move {
        #[cfg(unix)]
        {
            tokio::select! {_=interrupt.recv()=>{},_=terminate.recv()=>{}}
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        let _ = cancel.send(true);
    });
    let result = execution.run(Some(receiver)).await;
    signals.abort();
    let _ = done.send(true);
    let _ = output.await;
    let _ = telemetry_lifecycle.shutdown().await;
    match result {
        Ok(outcome) => {
            if json {
                println!("{}", serde_json::json!({"type":"result","result":outcome}));
            } else {
                if let Some(text) = &outcome.final_output {
                    println!("{text}");
                }
                if let Some(reason) = &outcome.reason {
                    eprintln!("{reason}");
                }
                eprintln!("Logs: {}", outcome.logs_dir.display());
            }
            outcome.exit_code
        }
        Err(e) => {
            eprintln!(
                "{}",
                excalibur_vault::Redactor::default().redact_text(&e.to_string())
            );
            1
        }
    }
}
fn write_event(event: &excalibur_telemetry::EventEnvelope) -> Result<(), String> {
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, event).map_err(|_| "event output failed")?;
    output
        .write_all(b"\n")
        .and_then(|_| output.flush())
        .map_err(|_| "event output failed".into())
}
