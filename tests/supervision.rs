//! Executor-free tests for the runtime supervision contracts.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rustclamp_core::ProcessId;
use rustclamp_runtime::{
    FailurePolicy, ManualRuntime, Supervision, Supervisor, TaskContext, TaskDefinition, TaskExit,
    TaskKind,
};

const PROCESS: ProcessId = ProcessId::new("runtime.test.worker");

#[test]
fn manual_runtime_completes_finite_work_and_applies_optional_failure_policy() {
    let runtime = ManualRuntime;
    let completed = TaskDefinition::new(
        PROCESS,
        "finite",
        TaskKind::Finite,
        true,
        None,
        FailurePolicy::Shutdown,
        |_| Ok(()),
    );
    assert_eq!(
        Supervisor::new(runtime).supervise(&completed, &TaskContext::new()),
        Supervision::Completed
    );

    let optional = TaskDefinition::new(
        PROCESS,
        "optional-service",
        TaskKind::Service,
        false,
        None,
        FailurePolicy::Degrade,
        |_| Err("offline".to_owned()),
    );
    assert_eq!(
        Supervisor::new(runtime).supervise(&optional, &TaskContext::new()),
        Supervision::Degraded(TaskExit::Failed("offline".to_owned()))
    );
}

#[test]
fn restart_policy_retries_once_and_required_failure_requests_shutdown() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let retry = TaskDefinition::new(
        PROCESS,
        "retry-once",
        TaskKind::Service,
        true,
        None,
        FailurePolicy::RestartOnce,
        move |_| {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("first attempt".to_owned())
            } else {
                Ok(())
            }
        },
    );
    assert_eq!(
        Supervisor::new(ManualRuntime).supervise(&retry, &TaskContext::new()),
        Supervision::Completed
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    let critical = TaskDefinition::new(
        PROCESS,
        "critical-service",
        TaskKind::Service,
        true,
        None,
        FailurePolicy::Degrade,
        |_| Err("critical failure".to_owned()),
    );
    assert_eq!(
        Supervisor::new(ManualRuntime).supervise(&critical, &TaskContext::new()),
        Supervision::Shutdown(TaskExit::Failed("critical failure".to_owned()))
    );
}

#[test]
fn panics_are_reported_as_task_failures() {
    let task = TaskDefinition::new(
        PROCESS,
        "panics",
        TaskKind::Finite,
        true,
        None,
        FailurePolicy::Shutdown,
        |_| panic!("task panic"),
    );
    assert_eq!(
        Supervisor::new(ManualRuntime).supervise(&task, &TaskContext::new()),
        Supervision::Shutdown(TaskExit::Panicked)
    );
}

#[test]
fn manual_runtime_observes_cancellation_and_child_deadlines_never_extend_parent() {
    let runtime = ManualRuntime;
    let task = TaskDefinition::new(
        PROCESS,
        "cancelled-service",
        TaskKind::Service,
        false,
        None,
        FailurePolicy::Ignore,
        |_| Ok(()),
    );
    let supervisor = Supervisor::new(runtime);
    let running = supervisor.start(&task, &TaskContext::new());
    running.cancel();
    assert_eq!(running.wait(), Supervision::Ignored(TaskExit::Cancelled));

    let parent = TaskContext::new().child(Some(Duration::ZERO));
    let child = parent.child(Some(Duration::from_secs(60)));
    assert_eq!(child.deadline(), parent.deadline());
    assert!(child.is_expired());
    let parent = TaskContext::new();
    let child = parent.child(None);
    let grandchild = child.child(None);
    parent.cancellation().cancel();
    assert!(child.is_cancelled());
    assert!(grandchild.is_cancelled());
}
