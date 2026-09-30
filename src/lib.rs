//! Runtime-neutral task and cancellation contracts.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use rustclamp_core::ProcessId;

/// Cooperative cancellation signal shared with a running task.
///
/// Tasks either check [`is_cancelled`](Self::is_cancelled) or await
/// [`cancelled`](Self::cancelled); the future needs no executor-specific support.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<TokenState>);

#[derive(Debug, Default)]
struct TokenState {
    cancelled: AtomicBool,
    waiters: Mutex<Waiters>,
}

/// Wakers of pending [`Cancelled`] futures, keyed so a dropped future deregisters.
#[derive(Debug, Default)]
struct Waiters {
    next: u64,
    wakers: BTreeMap<u64, Waker>,
}

impl CancellationToken {
    /// Requests cancellation and wakes every task awaiting [`cancelled`](Self::cancelled).
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        let wakers = std::mem::take(&mut self.waiters().wakers);
        wakers.into_values().for_each(Waker::wake);
    }

    /// Reports whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    /// Completes once cancellation is requested.
    pub fn cancelled(&self) -> Cancelled<'_> {
        Cancelled {
            tokens: vec![(self, None)],
        }
    }

    fn waiters(&self) -> std::sync::MutexGuard<'_, Waiters> {
        self.0
            .waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Future returned by [`CancellationToken::cancelled`] and [`TaskContext::cancelled`].
///
/// Completes when any watched token is cancelled. Dropping it deregisters its
/// wakers, so long-lived parent tokens do not accumulate finished waiters.
#[derive(Debug)]
pub struct Cancelled<'a> {
    tokens: Vec<(&'a CancellationToken, Option<u64>)>,
}

impl Future for Cancelled<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        for (token, key) in &mut self.tokens {
            if token.is_cancelled() {
                return Poll::Ready(());
            }
            let mut waiters = token.waiters();
            // cancel() stores the flag before taking the waiters, so re-check under the lock.
            if token.is_cancelled() {
                return Poll::Ready(());
            }
            match key {
                Some(key) => {
                    if let Some(waker) = waiters.wakers.get_mut(key)
                        && !waker.will_wake(context.waker())
                    {
                        waker.clone_from(context.waker());
                    }
                }
                None => {
                    let next = waiters.next;
                    waiters.next += 1;
                    waiters.wakers.insert(next, context.waker().clone());
                    *key = Some(next);
                }
            }
        }
        Poll::Pending
    }
}

impl Drop for Cancelled<'_> {
    fn drop(&mut self) {
        for (token, key) in &self.tokens {
            if let Some(key) = key {
                token.waiters().wakers.remove(key);
            }
        }
    }
}

/// Context passed to one task attempt.
#[derive(Clone, Debug, Default)]
pub struct TaskContext {
    cancellation: CancellationToken,
    parent_cancellation: Vec<CancellationToken>,
    deadline: Option<Instant>,
}

impl TaskContext {
    /// Creates a root task context with no deadline.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a child context whose deadline cannot extend its parent's.
    pub fn child(&self, timeout: Option<Duration>) -> Self {
        let requested = timeout.and_then(|duration| Instant::now().checked_add(duration));
        let deadline = match (self.deadline, requested) {
            (Some(parent), Some(child)) => Some(parent.min(child)),
            (parent, child) => parent.or(child),
        };
        let mut parent_cancellation = self.parent_cancellation.clone();
        parent_cancellation.push(self.cancellation.clone());
        Self {
            cancellation: CancellationToken::default(),
            parent_cancellation,
            deadline,
        }
    }

    /// Returns this task's cancellation token.
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Reports cancellation requested by this task or its parent.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
            || self
                .parent_cancellation
                .iter()
                .any(CancellationToken::is_cancelled)
    }

    /// Completes once this task or any parent is cancelled.
    pub fn cancelled(&self) -> Cancelled<'_> {
        Cancelled {
            tokens: std::iter::once(&self.cancellation)
                .chain(&self.parent_cancellation)
                .map(|token| (token, None))
                .collect(),
        }
    }

    /// Returns this task's monotonic deadline, if one exists.
    pub const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Reports whether this task's deadline has elapsed.
    pub fn is_expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }
}

/// Work category used to distinguish one-shot tasks from lifecycle services.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskKind {
    /// Finite work expected to complete once.
    Finite,
    /// Long-lived work expected to run until cancellation.
    Service,
}

/// Application-selected response when a supervised task fails.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailurePolicy {
    /// Retry one time, then apply the configured fallback.
    RestartOnce,
    /// Keep the process alive and mark it degraded.
    Degrade,
    /// Request process shutdown.
    Shutdown,
    /// Ignore this optional task failure.
    Ignore,
}

/// Result returned by a task operation.
pub type TaskResult = Result<(), String>;

/// Reusable task declaration with process ownership and recovery policy.
#[derive(Clone)]
pub struct TaskDefinition {
    process: ProcessId,
    name: &'static str,
    kind: TaskKind,
    required: bool,
    timeout: Option<Duration>,
    policy: FailurePolicy,
    operation: Arc<dyn Fn(TaskContext) -> TaskResult + Send + Sync>,
}

impl std::fmt::Debug for TaskDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskDefinition")
            .field("process", &self.process)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("required", &self.required)
            .field("timeout", &self.timeout)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl TaskDefinition {
    /// Declares a task and its selected process policy.
    pub fn new(
        process: ProcessId,
        name: &'static str,
        kind: TaskKind,
        required: bool,
        timeout: Option<Duration>,
        policy: FailurePolicy,
        operation: impl Fn(TaskContext) -> TaskResult + Send + Sync + 'static,
    ) -> Self {
        Self {
            process,
            name,
            kind,
            required,
            timeout,
            policy,
            operation: Arc::new(operation),
        }
    }

    /// Returns the owning process.
    pub const fn process(&self) -> ProcessId {
        self.process
    }

    /// Returns the task's stable source name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Returns whether the task is finite or a long-lived service.
    pub const fn kind(&self) -> TaskKind {
        self.kind
    }

    /// Reports whether this task is required for the process.
    pub const fn required(&self) -> bool {
        self.required
    }

    /// Returns the recovery policy selected by the application.
    pub const fn policy(&self) -> FailurePolicy {
        self.policy
    }
}

/// Completion or failure observed by a runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskExit {
    /// The task returned successfully.
    Completed,
    /// The task returned an application error.
    Failed(String),
    /// The task observed cancellation and stopped cooperatively.
    Cancelled,
    /// The task exceeded its deadline.
    TimedOut,
    /// The task panicked while unwinding.
    Panicked,
}

/// Executes tasks and reports their completion through a synchronous contract.
///
/// Implementations may use a deterministic manual driver or an asynchronous
/// executor. Core and this contract do not name an executor or require async.
pub trait TaskRuntime {
    /// Opaque handle for an active task.
    type Handle;

    /// Starts one task attempt with its inherited cancellation and deadline.
    fn spawn(&self, definition: &TaskDefinition, context: TaskContext) -> Self::Handle;

    /// Requests cooperative cancellation of an active task.
    fn cancel(&self, handle: &Self::Handle);

    /// Waits up to the supplied bound and returns the observed result.
    fn join(&self, handle: Self::Handle, timeout: Duration) -> TaskExit;
}

/// A deterministic, executor-free driver for synchronous tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct ManualRuntime;

/// Handle retained until a manually driven task is joined.
pub struct ManualTask {
    definition: TaskDefinition,
    context: TaskContext,
}

impl TaskRuntime for ManualRuntime {
    type Handle = ManualTask;

    fn spawn(&self, definition: &TaskDefinition, context: TaskContext) -> Self::Handle {
        ManualTask {
            definition: definition.clone(),
            context,
        }
    }

    fn cancel(&self, handle: &Self::Handle) {
        handle.context.cancellation.cancel();
    }

    fn join(&self, handle: Self::Handle, timeout: Duration) -> TaskExit {
        if handle.context.is_cancelled() {
            return TaskExit::Cancelled;
        }
        if handle.context.is_expired() || timeout.is_zero() {
            return TaskExit::TimedOut;
        }
        let started = Instant::now();
        let exit = match catch_unwind(AssertUnwindSafe(|| {
            (handle.definition.operation)(handle.context.clone())
        })) {
            Ok(Ok(())) if handle.context.is_cancelled() => TaskExit::Cancelled,
            Ok(Ok(())) if handle.context.is_expired() => TaskExit::TimedOut,
            Ok(Ok(())) => TaskExit::Completed,
            Ok(Err(error)) => TaskExit::Failed(error),
            Err(_) => TaskExit::Panicked,
        };
        if started.elapsed() > timeout || handle.context.is_expired() {
            TaskExit::TimedOut
        } else {
            exit
        }
    }
}

/// Final supervisor decision after task completion and configured recovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Supervision {
    /// Task completed successfully.
    Completed,
    /// Task failed and the process should become degraded.
    Degraded(TaskExit),
    /// Required task failed and process shutdown should begin.
    Shutdown(TaskExit),
    /// Optional task failure was ignored by policy.
    Ignored(TaskExit),
}

impl Supervision {
    /// Applies a failure policy to a completed task attempt.
    pub fn from_exit(exit: TaskExit, required: bool, policy: FailurePolicy) -> Self {
        match policy {
            FailurePolicy::Degrade if required => Self::Shutdown(exit),
            FailurePolicy::Degrade => Self::Degraded(exit),
            FailurePolicy::Shutdown => Self::Shutdown(exit),
            FailurePolicy::Ignore if required => Self::Shutdown(exit),
            FailurePolicy::Ignore => Self::Ignored(exit),
            FailurePolicy::RestartOnce if required => Self::Shutdown(exit),
            FailurePolicy::RestartOnce => Self::Degraded(exit),
        }
    }
}

/// Runs task attempts using one runtime and the declared recovery policy.
pub struct Supervisor<R> {
    runtime: R,
}

impl<R: TaskRuntime> Supervisor<R> {
    /// Creates a supervisor using the process-selected runtime.
    pub const fn new(runtime: R) -> Self {
        Self { runtime }
    }

    /// Starts a task while returning a handle that can be cancelled by its owner.
    pub fn start<'a>(
        &'a self,
        definition: &TaskDefinition,
        parent: &TaskContext,
    ) -> RunningTask<'a, R> {
        let context = parent.child(definition.timeout);
        let handle = self.runtime.spawn(definition, context.clone());
        RunningTask {
            supervisor: self,
            definition: definition.clone(),
            parent: parent.clone(),
            context,
            handle: Some(handle),
            attempts: 1,
        }
    }

    /// Runs a task, including its optional single restart, and applies policy.
    pub fn supervise(&self, definition: &TaskDefinition, parent: &TaskContext) -> Supervision {
        self.start(definition, parent).wait()
    }
}

/// Started task whose owner may request cancellation before joining it.
pub struct RunningTask<'a, R: TaskRuntime> {
    supervisor: &'a Supervisor<R>,
    definition: TaskDefinition,
    parent: TaskContext,
    context: TaskContext,
    handle: Option<R::Handle>,
    attempts: u8,
}

impl<R: TaskRuntime> RunningTask<'_, R> {
    /// Requests cooperative cancellation.
    pub fn cancel(&self) {
        if let Some(handle) = &self.handle {
            self.supervisor.runtime.cancel(handle);
        }
    }

    /// Joins the task, applies its recovery policy, and returns the final status.
    pub fn wait(mut self) -> Supervision {
        loop {
            let timeout = self.context.deadline().map_or(Duration::MAX, |deadline| {
                deadline.saturating_duration_since(Instant::now())
            });
            let exit = self
                .supervisor
                .runtime
                .join(self.handle.take().expect("active task handle"), timeout);
            if exit == TaskExit::Completed {
                return Supervision::Completed;
            }
            if self.definition.policy == FailurePolicy::RestartOnce
                && self.attempts == 1
                && exit != TaskExit::Cancelled
            {
                self.attempts += 1;
                self.context = self.parent.child(self.definition.timeout);
                self.handle = Some(
                    self.supervisor
                        .runtime
                        .spawn(&self.definition, self.context.clone()),
                );
                continue;
            }
            return Supervision::from_exit(exit, self.definition.required, self.definition.policy);
        }
    }
}

#[cfg(feature = "tokio")]
pub mod app;
#[cfg(feature = "tokio")]
pub mod tokio_runtime;
