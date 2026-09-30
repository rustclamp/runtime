//! `AppRunner::run` joins SIGHUP (reload hook) and SIGTERM (shutdown). Own binary:
//! signals are process-wide.

#![cfg(all(feature = "signal", unix))]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use rustclamp_core::{ApplicationId, ProcessId};
use rustclamp_runtime::app::AppRunner;
use rustclamp_runtime::tokio_runtime::reload_signal;

fn signal(name: &str) {
    let pid = std::process::id().to_string();
    assert!(
        std::process::Command::new("kill")
            .args([name, &pid])
            .status()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn sighup_reloads_and_sigterm_stops() {
    let reloads = Arc::new(AtomicUsize::new(0));
    let seen = reloads.clone();
    // Documented pattern: install the stream first, then reload from a service.
    let mut reload = reload_signal().unwrap();
    let app = AppRunner::new(ApplicationId::new("a"), ProcessId::new("p")).service(
        "reload",
        move |token| async move {
            loop {
                tokio::select! {
                    Some(()) = reload.recv() => { seen.fetch_add(1, Ordering::SeqCst); }
                    () = token.cancelled() => return Ok(()),
                }
            }
        },
    );
    let run = tokio::spawn(app.run());
    // run() installs the shutdown handlers on its first poll; give the task time to get there.
    tokio::time::sleep(Duration::from_millis(100)).await;
    signal("-HUP");
    tokio::time::timeout(Duration::from_secs(5), async {
        while reloads.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("SIGHUP was not delivered to the reload service");
    signal("-TERM");
    assert_eq!(run.await.unwrap(), Ok(()));
    assert_eq!(reloads.load(Ordering::SeqCst), 1);
}
