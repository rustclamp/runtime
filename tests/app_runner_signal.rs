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
    let app = AppRunner::new(ApplicationId::new("a"), ProcessId::new("p"))
        .service("svc", |token| async move {
            token.cancelled().await;
            Ok(())
        })
        .on_reload(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        });
    let run = tokio::spawn(app.run());
    // Handlers are installed on the first poll; give the task time to get there.
    tokio::time::sleep(Duration::from_millis(100)).await;
    signal("-HUP");
    while reloads.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    signal("-TERM");
    assert_eq!(run.await.unwrap(), Ok(()));
    assert_eq!(reloads.load(Ordering::SeqCst), 1);
}
