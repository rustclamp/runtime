//! Optional Tokio-backed task execution and platform signal adapter.

use std::future::Future;
use std::io;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread;
use std::time::{Duration, Instant};

use tokio::runtime::{Builder, Handle, Runtime};
use tokio::task::JoinHandle;

use crate::{FailurePolicy, Supervision, TaskContext, TaskDefinition, TaskExit, TaskRuntime};

/// Platform signal translated into a runtime-neutral shutdown request.
#[cfg(feature = "signal")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownSignal {
    /// The process received its interrupt signal (Ctrl-C / SIGINT).
    Interrupt,
    /// The process received SIGTERM (Unix only), the usual service-manager stop.
    Terminate,
}

/// Installs shutdown handlers now and returns a future that completes on the
/// first request: SIGINT (Ctrl-C), or SIGTERM on Unix.
///
/// Call from inside the runtime, before startup work: from this call on, those
/// signals are delivered to the future instead of terminating the process.
/// Off Unix, Ctrl-C is registered when the future is first polled.
#[cfg(feature = "signal")]
pub fn shutdown_signal() -> io::Result<impl Future<Output = ShutdownSignal> + Send + 'static> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        Ok(async move {
            tokio::select! {
                _ = interrupt.recv() => ShutdownSignal::Interrupt,
                _ = terminate.recv() => ShutdownSignal::Terminate,
            }
        })
    }
    #[cfg(not(unix))]
    {
        Ok(async {
            // ponytail: a ctrl_c registration error off Unix is treated as "never signalled".
            match tokio::signal::ctrl_c().await {
                Ok(()) => ShutdownSignal::Interrupt,
                Err(_) => std::future::pending().await,
            }
        })
    }
}

/// Tokio adapter that can own a runtime or adopt an existing runtime handle.
pub struct TokioRuntime {
    handle: Handle,
    _owned_runtime: Option<Runtime>,
}

impl TokioRuntime {
    /// Builds and owns a multithreaded Tokio runtime with two worker threads.
    ///
    /// Use [`managed_with_threads`](Self::managed_with_threads) to size it for the host.
    pub fn managed() -> io::Result<Self> {
        Self::managed_with_threads(NonZeroUsize::new(2).expect("2 is non-zero"))
    }

    /// Builds and owns a multithreaded Tokio runtime with `threads` worker threads,
    /// e.g. `std::thread::available_parallelism()?` for a CPU-bound service.
    pub fn managed_with_threads(threads: NonZeroUsize) -> io::Result<Self> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(threads.get())
            .enable_all()
            .build()?;
        let handle = runtime.handle().clone();
        Ok(Self {
            handle,
            _owned_runtime: Some(runtime),
        })
    }

    /// Adopts a runtime created and owned by the caller.
    pub fn from_handle(handle: Handle) -> Self {
        Self {
            handle,
            _owned_runtime: None,
        }
    }

    /// Blocks until Ctrl-C and returns a platform-neutral signal value.
    #[cfg(feature = "signal")]
    pub fn wait_for_ctrl_c(&self) -> io::Result<ShutdownSignal> {
        self.handle.block_on(async {
            tokio::signal::ctrl_c().await?;
            Ok(ShutdownSignal::Interrupt)
        })
    }

    /// Drives one future on this runtime. Call outside an async runtime context.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.handle.block_on(future)
    }

    /// Starts native asynchronous work using the shared task context contract.
    pub fn spawn_async<F, Fut>(&self, context: TaskContext, operation: F) -> AsyncTask
    where
        F: FnOnce(TaskContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let task_context = context.clone();
        let join = self.handle.spawn(async move {
            if task_context.is_cancelled() {
                return TaskExit::Cancelled;
            }
            if task_context.is_expired() {
                return TaskExit::TimedOut;
            }
            let future = operation(task_context.clone());
            tokio::select! {
                result = future => match result {
                    Ok(()) if task_context.is_cancelled() => TaskExit::Cancelled,
                    Ok(()) => TaskExit::Completed,
                    Err(error) => TaskExit::Failed(error),
                },
                () = task_context.cancelled() => TaskExit::Cancelled,
                () = async {
                    if let Some(deadline) = task_context.deadline() {
                        tokio::time::sleep_until(deadline.into()).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => TaskExit::TimedOut,
            }
        });
        AsyncTask {
            join: Some(join),
            context,
        }
    }

    /// Runs native async work with the same retry and failure policy as sync tasks.
    pub async fn supervise_async<F, Fut>(
        &self,
        parent: &TaskContext,
        timeout: Option<Duration>,
        required: bool,
        policy: FailurePolicy,
        operation: F,
    ) -> Supervision
    where
        F: Fn(TaskContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let operation = std::sync::Arc::new(operation);
        for attempt in 0..=u8::from(policy == FailurePolicy::RestartOnce) {
            let context = parent.child(timeout);
            let attempt_operation = operation.clone();
            let task = self.spawn_async(context, move |context| attempt_operation(context));
            let exit = task.wait(None).await;
            if exit == TaskExit::Completed {
                return Supervision::Completed;
            }
            if attempt == 0 && policy == FailurePolicy::RestartOnce && exit != TaskExit::Cancelled {
                continue;
            }
            return Supervision::from_exit(exit, required, policy);
        }
        unreachable!("the async task loop always returns or runs one attempt")
    }
}

/// Cancellable native async task owned by a Tokio runtime adapter.
pub struct AsyncTask {
    join: Option<JoinHandle<TaskExit>>,
    context: TaskContext,
}

impl AsyncTask {
    /// Requests cancellation and aborts the async future at its next poll.
    pub fn cancel(&self) {
        self.context.cancellation().cancel();
        if let Some(join) = &self.join {
            join.abort();
        }
    }

    /// Waits for completion, optionally bounding the wait and aborting on timeout.
    pub async fn wait(mut self, timeout: Option<Duration>) -> TaskExit {
        let Some(join) = self.join.take() else {
            return TaskExit::Panicked;
        };
        let abort = join.abort_handle();
        let result = match timeout {
            Some(timeout) => match tokio::time::timeout(timeout, join).await {
                Ok(result) => result,
                Err(_) => {
                    self.context.cancellation().cancel();
                    abort.abort();
                    return TaskExit::TimedOut;
                }
            },
            None => join.await,
        };
        match result {
            Ok(exit) => exit,
            Err(error) if error.is_cancelled() => TaskExit::Cancelled,
            Err(error) if error.is_panic() => TaskExit::Panicked,
            Err(_) => TaskExit::Panicked,
        }
    }
}

/// Join and cancellation state for one task attempt.
pub struct TokioTask {
    join: Option<JoinHandle<TaskExit>>,
    context: TaskContext,
}

impl TaskRuntime for TokioRuntime {
    type Handle = TokioTask;

    fn spawn(&self, definition: &TaskDefinition, context: TaskContext) -> Self::Handle {
        let definition = definition.clone();
        let task_context = context.clone();
        let join = self.handle.spawn_blocking(move || {
            if task_context.is_cancelled() {
                return TaskExit::Cancelled;
            }
            match catch_unwind(AssertUnwindSafe(|| {
                (definition.operation)(task_context.clone())
            })) {
                Ok(Ok(())) if task_context.is_cancelled() => TaskExit::Cancelled,
                Ok(Ok(())) if task_context.is_expired() => TaskExit::TimedOut,
                Ok(Ok(())) => TaskExit::Completed,
                Ok(Err(error)) => TaskExit::Failed(error),
                Err(_) => TaskExit::Panicked,
            }
        });
        TokioTask {
            join: Some(join),
            context,
        }
    }

    fn cancel(&self, handle: &Self::Handle) {
        handle.context.cancellation().cancel();
    }

    fn join(&self, mut handle: Self::Handle, timeout: Duration) -> TaskExit {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            let Some(join) = handle.join.as_ref() else {
                return TaskExit::Panicked;
            };
            if join.is_finished() {
                let join = handle.join.take().expect("join handle checked above");
                return match self.handle.block_on(join) {
                    Ok(result) => result,
                    Err(error) if error.is_cancelled() => TaskExit::Cancelled,
                    Err(_) => TaskExit::Panicked,
                };
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                handle.context.cancellation().cancel();
                return TaskExit::TimedOut;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}
