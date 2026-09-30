//! Feature-gated smoke tests for Tokio task execution and runtime adoption.

#![cfg(feature = "tokio")]

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use rustclamp_core::ProcessId;
use rustclamp_runtime::tokio_runtime::TokioRuntime;
use rustclamp_runtime::{
    FailurePolicy, Supervision, Supervisor, TaskContext, TaskDefinition, TaskExit, TaskKind,
};
use tokio::runtime::Builder;

const PROCESS: ProcessId = ProcessId::new("runtime.test.tokio");

fn task() -> TaskDefinition {
    TaskDefinition::new(
        PROCESS,
        "tokio-task",
        TaskKind::Finite,
        true,
        Some(Duration::from_secs(2)),
        FailurePolicy::Shutdown,
        |_| Ok(()),
    )
}

#[test]
fn managed_tokio_runtime_uses_the_public_supervisor_contract() {
    let runtime = TokioRuntime::managed().unwrap();
    assert_eq!(
        Supervisor::new(runtime).supervise(&task(), &TaskContext::new()),
        Supervision::Completed
    );
}

#[test]
fn existing_tokio_runtime_can_be_adopted_without_transfer_of_ownership() {
    let existing = Builder::new_multi_thread().enable_all().build().unwrap();
    let adapter = TokioRuntime::from_handle(existing.handle().clone());
    assert_eq!(
        Supervisor::new(adapter).supervise(&task(), &TaskContext::new()),
        Supervision::Completed
    );
    assert_eq!(existing.block_on(async { 7 }), 7);
}

#[test]
fn tokio_task_cancellation_is_cooperative_and_panic_is_reported() {
    let runtime = TokioRuntime::managed().unwrap();
    let started = Arc::new(AtomicBool::new(false));
    let task_started = started.clone();
    let definition = TaskDefinition::new(
        PROCESS,
        "cancellable-service",
        TaskKind::Service,
        true,
        None,
        FailurePolicy::Shutdown,
        move |context| {
            task_started.store(true, Ordering::Release);
            while !context.is_cancelled() {
                std::thread::yield_now();
            }
            Ok(())
        },
    );
    let supervisor = Supervisor::new(runtime);
    let running = supervisor.start(&definition, &TaskContext::new());
    while !started.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    running.cancel();
    assert_eq!(running.wait(), Supervision::Shutdown(TaskExit::Cancelled));

    let panics = TaskDefinition::new(
        PROCESS,
        "panic-service",
        TaskKind::Finite,
        true,
        None,
        FailurePolicy::Shutdown,
        |_| panic!("tokio task panic"),
    );
    assert_eq!(
        supervisor.supervise(&panics, &TaskContext::new()),
        Supervision::Shutdown(TaskExit::Panicked)
    );
}

#[test]
fn native_async_tasks_support_cancellation_deadlines_timeout_and_panic_reporting() {
    let runtime = TokioRuntime::managed().unwrap();
    let task = runtime.spawn_async(TaskContext::new(), |_| async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok(())
    });
    task.cancel();
    assert_eq!(
        runtime.block_on(task.wait(Some(Duration::from_secs(1)))),
        TaskExit::Cancelled
    );

    let context = TaskContext::new().child(Some(Duration::from_millis(2)));
    let task = runtime.spawn_async(context, |_| async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        Ok(())
    });
    assert_eq!(runtime.block_on(task.wait(None)), TaskExit::TimedOut);

    let task = runtime.spawn_async(TaskContext::new(), |_| async {
        std::future::pending::<Result<(), String>>().await
    });
    assert_eq!(
        runtime.block_on(task.wait(Some(Duration::from_millis(2)))),
        TaskExit::TimedOut
    );

    let task = runtime.spawn_async(TaskContext::new(), |_| async {
        panic!("native async task panic");
        #[allow(unreachable_code)]
        Ok(())
    });
    assert_eq!(runtime.block_on(task.wait(None)), TaskExit::Panicked);
}

#[test]
fn native_async_supervisor_retries_and_applies_required_failure_policy() {
    let runtime = TokioRuntime::managed().unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempts = calls.clone();
    let recovered = runtime.block_on(runtime.supervise_async(
        &TaskContext::new(),
        None,
        true,
        FailurePolicy::RestartOnce,
        move |_| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt == 0 {
                    Err("retry".to_owned())
                } else {
                    Ok(())
                }
            }
        },
    ));
    assert_eq!(recovered, Supervision::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let optional = runtime.block_on(runtime.supervise_async(
        &TaskContext::new(),
        None,
        false,
        FailurePolicy::Degrade,
        |_| async { Err("optional failure".to_owned()) },
    ));
    assert_eq!(
        optional,
        Supervision::Degraded(TaskExit::Failed("optional failure".to_owned()))
    );

    let required = runtime.block_on(runtime.supervise_async(
        &TaskContext::new(),
        None,
        true,
        FailurePolicy::Degrade,
        |_| async { Err("required failure".to_owned()) },
    ));
    assert_eq!(
        required,
        Supervision::Shutdown(TaskExit::Failed("required failure".to_owned()))
    );
}

#[test]
fn managed_runtimes_have_the_requested_worker_count() {
    let workers = |runtime: TokioRuntime| {
        runtime.block_on(async { tokio::runtime::Handle::current().metrics().num_workers() })
    };
    assert_eq!(workers(TokioRuntime::managed().unwrap()), 2);
    let four = std::num::NonZeroUsize::new(4).unwrap();
    assert_eq!(
        workers(TokioRuntime::managed_with_threads(four).unwrap()),
        4
    );
}
