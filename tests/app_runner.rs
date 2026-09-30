//! App runner: lifecycle ordering, startup failure unwinding, drain timeout, shutdown token.

#![cfg(feature = "tokio")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustclamp_core::{
    ApplicationId, Drain, Initialize, LifecycleContext, Module, ModuleId, ProcessId, Ready, Start,
    Stop,
};
use rustclamp_runtime::app::{AppRunner, Failure, Part, Phase};

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

    assert_eq!(
        error.failures,
        [Failure {
            phase: Phase::Start,
            name: "users",
            message: "start broke".into()
        }]
    );
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
    let result = runner(&log, None)
        .drain_timeout(Duration::from_millis(50))
        .service("stuck", |_| std::future::pending())
        .run_until(std::future::ready(()))
        .await;

    let failures = result.unwrap_err().failures;
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].phase, Phase::Drain);
    // Parts still drain and stop after the timeout.
    assert_eq!(entries(&log).last().unwrap(), "db.stop");
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

    assert_eq!(
        error.failures,
        [Failure {
            phase: Phase::Service,
            name: "boom",
            message: "crashed".into()
        }]
    );
    assert_eq!(entries(&log).last().unwrap(), "db.stop");
}
