//! Tests for awaitable cancellation and shutdown signals.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use rustclamp_runtime::{CancellationToken, TaskContext};

#[derive(Default)]
struct CountingWaker(AtomicUsize);

impl Wake for CountingWaker {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn waker() -> (Arc<CountingWaker>, Waker) {
    let counter = Arc::new(CountingWaker::default());
    (counter.clone(), Waker::from(counter))
}

#[test]
fn cancel_wakes_a_pending_waiter_without_an_executor() {
    let token = CancellationToken::default();
    let (counter, waker) = waker();
    let mut context = Context::from_waker(&waker);
    let mut cancelled = pin!(token.cancelled());

    assert!(cancelled.as_mut().poll(&mut context).is_pending());
    token.cancel();
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    assert!(cancelled.as_mut().poll(&mut context).is_ready());
}

#[test]
fn a_child_context_completes_when_its_parent_is_cancelled() {
    let parent = TaskContext::new();
    let child = parent.child(None).child(None);
    let (counter, waker) = waker();
    let mut context = Context::from_waker(&waker);
    let mut cancelled = pin!(child.cancelled());

    assert!(cancelled.as_mut().poll(&mut context).is_pending());
    parent.cancellation().cancel();
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    assert_eq!(cancelled.as_mut().poll(&mut context), Poll::Ready(()));
}

#[test]
fn dropped_waiters_are_not_woken() {
    let token = CancellationToken::default();
    let (counter, waker) = waker();
    let mut context = Context::from_waker(&waker);
    for _ in 0..1000 {
        let mut cancelled = pin!(token.cancelled());
        assert!(cancelled.as_mut().poll(&mut context).is_pending());
    }
    token.cancel();
    assert_eq!(counter.0.load(Ordering::SeqCst), 0);
}

#[test]
fn an_already_cancelled_token_is_ready_at_once() {
    let token = CancellationToken::default();
    token.cancel();
    let (_, waker) = waker();
    let mut context = Context::from_waker(&waker);
    assert!(pin!(token.cancelled()).poll(&mut context).is_ready());
}

#[cfg(feature = "tokio")]
#[test]
fn spawn_async_stops_as_soon_as_the_parent_is_cancelled() {
    use std::time::{Duration, Instant};

    use rustclamp_runtime::TaskExit;
    use rustclamp_runtime::tokio_runtime::TokioRuntime;

    let runtime = TokioRuntime::managed().unwrap();
    let parent = TaskContext::new();
    let task = runtime.spawn_async(parent.child(None), |_| async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(())
    });
    std::thread::sleep(Duration::from_millis(20));
    let started = Instant::now();
    parent.cancellation().cancel();
    let exit = runtime.block_on(task.wait(None));
    assert_eq!(exit, TaskExit::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[cfg(all(feature = "signal", unix))]
#[test]
fn sigterm_resolves_the_shutdown_signal() {
    use std::time::Duration;

    use rustclamp_runtime::tokio_runtime::{ShutdownSignal, TokioRuntime, shutdown_signal};

    let runtime = TokioRuntime::managed().unwrap();
    let signal = runtime.block_on(async {
        // Installed eagerly: the signal below is caught even though nothing polled yet.
        let waiting = tokio::spawn(shutdown_signal().unwrap());
        let status = std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        tokio::time::timeout(Duration::from_secs(5), waiting).await
    });
    assert_eq!(signal.unwrap().unwrap(), ShutdownSignal::Terminate);
}

#[cfg(all(feature = "signal", unix))]
fn kill(signal: &str) {
    let status = std::process::Command::new("kill")
        .args([signal, &std::process::id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(all(feature = "signal", unix))]
#[test]
fn sighup_resolves_the_reload_signal_more_than_once() {
    use std::time::Duration;

    use rustclamp_runtime::tokio_runtime::{TokioRuntime, reload_signal};

    let runtime = TokioRuntime::managed().unwrap();
    let got = runtime.block_on(async {
        let mut reload = reload_signal().unwrap();
        let mut got = 0;
        for _ in 0..2 {
            kill("-HUP");
            let next = tokio::time::timeout(Duration::from_secs(5), reload.recv()).await;
            got += usize::from(next == Ok(Some(())));
        }
        got
    });
    assert_eq!(got, 2);
}

#[cfg(all(feature = "signal", unix))]
#[test]
fn sync_wait_returns_on_sigterm() {
    use rustclamp_runtime::tokio_runtime::{ShutdownSignal, wait_for_shutdown};

    // Handlers exist only once wait_for_shutdown installs them; send after a delay.
    let sender = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(500));
        kill("-TERM");
    });
    assert_eq!(wait_for_shutdown().unwrap(), ShutdownSignal::Terminate);
    sender.join().unwrap();
}
