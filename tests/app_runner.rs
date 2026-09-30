//! App runner: lifecycle ordering, startup failure unwinding, drain timeout, shutdown token.

#![cfg(feature = "tokio")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustclamp_core::{
    ApplicationId, Drain, Initialize, LifecycleContext, Module, ModuleId, ProcessId, Ready, Start,
    Stop,
};
use rustclamp_runtime::app::{AppRunner, Part, Phase, RunError};

const APP: ApplicationId = ApplicationId::new("runtime.test.app");
const PROCESS: ProcessId = ProcessId::new("runtime.test.app.process");

type Log = Arc<Mutex<Vec<String>>>;

macro_rules! probe {
    ($name:ident, $id:literal) => {
        struct $name {
            log: Log,
            fail_at: Option<&'static str>,
        }

        impl Module for $name {
            const ID: ModuleId = ModuleId::new($id);
        }

        impl $name {
            fn step(&self, phase: &'static str) -> Result<(), String> {
                self.log.lock().unwrap().push(format!("{}.{phase}", $id));
                if self.fail_at.and_then(|at| at.strip_prefix('!')) == Some(phase) {
                    panic!("{phase} panicked");
                }
                if self.fail_at == Some(phase) {
                    Err(format!("{phase} broke"))
                } else {
                    Ok(())
                }
            }
        }

        impl Initialize for $name {
            type Error = String;
            fn initialize(&mut self, _: &LifecycleContext) -> Result<(), String> {
                self.step("initialize")
            }
        }
        impl Start for $name {
            type Error = String;
            fn start(&mut self, _: &LifecycleContext) -> Result<(), String> {
                self.step("start")
            }
        }
        impl Ready for $name {
            type Error = String;
            fn ready(&mut self, _: &LifecycleContext) -> Result<(), String> {
                self.step("ready")
            }
        }
        impl Drain for $name {
            type Error = String;
            fn drain(&mut self, _: &LifecycleContext) -> Result<(), String> {
                self.step("drain")
            }
        }
        impl Stop for $name {
            type Error = String;
            fn stop(&mut self, _: &LifecycleContext) -> Result<(), String> {
                self.step("stop")
            }
        }
    };
}

probe!(Db, "db");
probe!(Users, "users");

fn part<M>(module: M) -> Part<M>
where
    M: Module + Send + 'static + Initialize + Start + Ready + Drain + Stop,
    <M as Initialize>::Error: std::fmt::Display,
    <M as Start>::Error: std::fmt::Display,
    <M as Ready>::Error: std::fmt::Display,
    <M as Drain>::Error: std::fmt::Display,
    <M as Stop>::Error: std::fmt::Display,
{
    Part::new(module)
        .initialize()
        .start()
        .ready()
        .drain()
        .stop()
}

fn runner(log: &Log, users_fail_at: Option<&'static str>) -> AppRunner {
    AppRunner::new(APP, PROCESS)
        .part(part(Db {
            log: log.clone(),
            fail_at: None,
        }))
        .part(part(Users {
            log: log.clone(),
            fail_at: users_fail_at,
        }))
}

/// Failures as comparable `(phase, name, message)` triples.
fn flat(error: &RunError) -> Vec<(Phase, &'static str, &str)> {
    error
        .failures
        .iter()
        .map(|f| (f.phase, f.name, f.message.as_str()))
        .collect()
}

/// Logs `svc.dropped` when the service future (and everything it holds) is dropped.
struct DropGuard(Log);

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.0.lock().unwrap().push("svc.dropped".into());
    }
}

fn entries(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

#[tokio::test]
async fn phases_run_in_order_and_unwind_in_reverse() {
    let log = Log::default();
    let service_log = log.clone();
    let result = runner(&log, None)
        .service("svc", move |token| async move {
            service_log.lock().unwrap().push("svc.run".into());
            token.cancelled().await;
            service_log.lock().unwrap().push("svc.cancelled".into());
            Ok(())
        })
        .run_until(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
        })
        .await;

    assert_eq!(result, Ok(()));
    assert_eq!(
        entries(&log),
        [
            "db.initialize",
            "users.initialize",
            "db.start",
            "users.start",
            "db.ready",
            "users.ready",
            "svc.run",
            "svc.cancelled",
            "users.drain",
            "db.drain",
            "users.stop",
            "db.stop",
        ]
    );
}

#[tokio::test]
async fn start_failure_stops_initialized_parts_without_running_services() {
    let log = Log::default();
    let service_log = log.clone();
    let error = runner(&log, Some("start"))
        .service("svc", move |_| async move {
            service_log.lock().unwrap().push("svc.run".into());
            Ok(())
        })
        .run_until(std::future::pending())
        .await
        .unwrap_err();

    assert_eq!(flat(&error), [(Phase::Start, "users", "start broke")]);
    assert_eq!(
        entries(&log),
        [
            "db.initialize",
            "users.initialize",
            "db.start",
            "users.start",
            "users.stop",
            "db.stop",
        ]
    );
}

#[tokio::test]
async fn initialize_failure_does_not_stop_the_failing_part() {
    let log = Log::default();
    let error = runner(&log, Some("initialize"))
        .run_until(std::future::pending())
        .await
        .unwrap_err();

    assert_eq!(error.failures[0].phase, Phase::Initialize);
    assert_eq!(
        entries(&log),
        ["db.initialize", "users.initialize", "db.stop"]
    );
}

#[tokio::test]
async fn stop_failure_is_reported_and_does_not_skip_earlier_parts() {
    let log = Log::default();
    let error = runner(&log, Some("stop"))
        .run_until(std::future::ready(()))
        .await
        .unwrap_err();

    assert_eq!(error.failures[0].phase, Phase::Stop);
    assert_eq!(entries(&log).last().unwrap(), "db.stop");
}

#[tokio::test]
async fn service_that_ignores_shutdown_is_aborted_after_drain_timeout() {
    let log = Log::default();
    let guard = DropGuard(log.clone());
    let result = runner(&log, None)
        .drain_timeout(Duration::from_millis(50))
        .service("stuck", move |_| async move {
            let _guard = guard;
            std::future::pending().await
        })
        .run_until(std::future::ready(()))
        .await;

    let error = result.unwrap_err();
    assert_eq!(error.failures.len(), 1);
    assert_eq!(error.failures[0].phase, Phase::Drain);
    assert_eq!(error.failures[0].name, "stuck");
    // The aborted service releases its handles before any part stops.
    let log = entries(&log);
    let dropped = log.iter().position(|e| e == "svc.dropped").unwrap();
    let stopped = log.iter().position(|e| e == "users.stop").unwrap();
    assert!(dropped < stopped, "{log:?}");
    assert_eq!(log.last().unwrap(), "db.stop");
}

#[tokio::test]
async fn cancelling_the_shutdown_token_stops_the_app() {
    let log = Log::default();
    let app = runner(&log, None).service("svc", |token| async move {
        token.cancelled().await;
        Ok(())
    });
    let token = app.shutdown_token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();
    });

    assert_eq!(app.run_until(std::future::pending()).await, Ok(()));
    assert_eq!(entries(&log).last().unwrap(), "db.stop");
}

#[tokio::test]
async fn failing_service_shuts_down_the_rest_and_is_reported() {
    let log = Log::default();
    let error = runner(&log, None)
        .service("boom", |_| async { Err("crashed".to_string()) })
        .service("other", |token| async move {
            token.cancelled().await;
            Ok(())
        })
        .run_until(std::future::pending())
        .await
        .unwrap_err();

    assert_eq!(flat(&error), [(Phase::Service, "boom", "crashed")]);
    assert_eq!(entries(&log).last().unwrap(), "db.stop");
}

#[tokio::test]
async fn ready_failure_unwinds_without_draining() {
    let log = Log::default();
    let error = runner(&log, Some("ready"))
        .run_until(std::future::pending())
        .await
        .unwrap_err();

    assert_eq!(flat(&error), [(Phase::Ready, "users", "ready broke")]);
    let log = entries(&log);
    assert!(!log.iter().any(|e| e.ends_with(".drain")), "{log:?}");
    assert_eq!(&log[log.len() - 2..], ["users.stop", "db.stop"]);
}

#[tokio::test]
async fn panicking_hook_is_a_failure_and_stops_still_run() {
    let log = Log::default();
    let error = runner(&log, Some("!start"))
        .run_until(std::future::pending())
        .await
        .unwrap_err();

    assert_eq!(
        flat(&error),
        [(Phase::Start, "users", "panicked: start panicked")]
    );
    assert_eq!(&entries(&log)[4..], ["users.stop", "db.stop"]);
}

#[tokio::test]
async fn panicking_service_is_reported_by_name() {
    let log = Log::default();
    let error = runner(&log, None)
        .service("crasher", |_| async { panic!("service blew up") })
        .run_until(std::future::pending())
        .await
        .unwrap_err();

    assert_eq!(error.failures.len(), 1);
    assert_eq!(error.failures[0].phase, Phase::Service);
    assert_eq!(error.failures[0].name, "crasher");
    assert_eq!(entries(&log).last().unwrap(), "db.stop");
}

#[tokio::test]
async fn clean_early_exit_is_an_unexpected_exit() {
    let log = Log::default();
    let error = runner(&log, None)
        .service("quitter", |_| async { Ok(()) })
        .run_until(std::future::pending())
        .await
        .unwrap_err();

    assert_eq!(
        flat(&error),
        [(Phase::Service, "quitter", "unexpected exit")]
    );
    assert_eq!(entries(&log).last().unwrap(), "db.stop");
}

#[tokio::test]
async fn failing_drain_is_reported_and_stop_still_runs() {
    let log = Log::default();
    let error = runner(&log, Some("drain"))
        .run_until(std::future::ready(()))
        .await
        .unwrap_err();

    assert_eq!(flat(&error), [(Phase::Drain, "users", "drain broke")]);
    assert_eq!(
        &entries(&log)[6..],
        ["users.drain", "db.drain", "users.stop", "db.stop"]
    );
}
