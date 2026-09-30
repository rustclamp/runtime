//! App runner: drives core lifecycle participants and long-lived services
//! under one shutdown token. See ADR 0022.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rustclamp_core::{
    ApplicationId, Drain, Initialize, LifecycleContext, Module, ProcessId, Ready, Start, Stop,
};
use tokio::task::JoinSet;

use crate::CancellationToken;

/// Lifecycle phase in which a [`Failure`] happened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Phase {
    /// [`Initialize::initialize`].
    Initialize,
    /// [`Start::start`].
    Start,
    /// [`Ready::ready`].
    Ready,
    /// A service failed, panicked or exited while the app was running.
    Service,
    /// [`Drain::drain`], or a service that outlived the drain timeout.
    Drain,
    /// [`Stop::stop`].
    Stop,
}

/// One failed phase of one participant or service.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Failure {
    /// The phase that failed.
    pub phase: Phase,
    /// The module or service name.
    pub name: &'static str,
    /// The error, rendered with `Display`.
    pub message: String,
}

/// Everything that went wrong in one run, in the order it happened.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct RunError {
    /// Failures in occurrence order; never empty.
    pub failures: Vec<Failure>,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, failure) in self.failures.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(
                f,
                "{:?} {}: {}",
                failure.phase, failure.name, failure.message
            )?;
        }
        Ok(())
    }
}

impl Error for RunError {}

/// Calls a hook, turning a panic into an error so the unwinding sequence still runs.
fn call(hook: &mut Hook, context: &LifecycleContext) -> Result<(), String> {
    catch_unwind(AssertUnwindSafe(|| hook(context))).unwrap_or_else(|payload| {
        let text = payload
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned());
        Err(format!("panicked: {}", text.unwrap_or_default()))
    })
}

/// How long an aborted service gets to drop its handles before parts stop.
const ABORT_GRACE: Duration = Duration::from_secs(1);

type Select = fn(&mut Hooks) -> &mut Option<Hook>;
type Hook = Box<dyn FnMut(&LifecycleContext) -> Result<(), String> + Send>;

/// A module plus the lifecycle traits it opted into.
///
/// The core traits are separate opt-ins, so each is registered explicitly:
/// `Part::new(module).initialize().start().stop()`.
pub struct Part<M> {
    module: Arc<Mutex<M>>,
    hooks: Hooks,
}

#[derive(Default)]
struct Hooks {
    initialize: Option<Hook>,
    start: Option<Hook>,
    ready: Option<Hook>,
    drain: Option<Hook>,
    stop: Option<Hook>,
}

struct Registered {
    name: &'static str,
    hooks: Hooks,
}

macro_rules! opt_in {
    ($($method:ident: $trait:ident),* $(,)?) => {$(
        /// Runs this trait's hook for the module during its phase.
        #[must_use]
        pub fn $method(mut self) -> Self
        where
            M: $trait,
            <M as $trait>::Error: fmt::Display,
        {
            let module = Arc::clone(&self.module);
            self.hooks.$method = Some(Box::new(move |context| {
                let mut module = module.lock().unwrap_or_else(PoisonError::into_inner);
                $trait::$method(&mut *module, context).map_err(|error| error.to_string())
            }));
            self
        }
    )*};
}

impl<M: Module + Send + 'static> Part<M> {
    /// Wraps a module; register the lifecycle traits it implements next.
    pub fn new(module: M) -> Self {
        Self {
            module: Arc::new(Mutex::new(module)),
            hooks: Hooks::default(),
        }
    }

    opt_in!(initialize: Initialize, start: Start, ready: Ready, drain: Drain, stop: Stop);

    fn register(self) -> Registered {
        Registered {
            name: M::ID.as_str(),
            hooks: self.hooks,
        }
    }
}

type ServiceFuture = std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

struct Service {
    name: &'static str,
    run: Box<dyn FnOnce(CancellationToken) -> ServiceFuture + Send>,
}

/// Sequences participants and services for one process, ending in a graceful stop.
///
/// Order: `initialize`, `start`, `ready` for each part in registration order
/// (each phase completes for all parts before the next begins); then services
/// run until shutdown is requested or one of them exits; then the shutdown token
/// is cancelled, services are joined within the drain timeout, and parts
/// `drain` then `stop` in reverse registration order.
pub struct AppRunner {
    context: LifecycleContext,
    token: CancellationToken,
    drain_timeout: Duration,
    parts: Vec<Registered>,
    services: Vec<Service>,
}

impl AppRunner {
    /// Creates a runner with a 30 second drain timeout.
    pub fn new(application: ApplicationId, process: ProcessId) -> Self {
        Self {
            context: LifecycleContext::new(application, process),
            token: CancellationToken::default(),
            drain_timeout: Duration::from_secs(30),
            parts: Vec::new(),
            services: Vec::new(),
        }
    }

    /// Bounds how long services may take to exit after shutdown is requested.
    #[must_use]
    pub fn drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain_timeout = timeout;
        self
    }

    /// Returns the shutdown token; cancelling it stops the app like a signal would.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Adds a participant. Registration order is start order; stop order is reversed.
    #[must_use]
    pub fn part<M: Module + Send + 'static>(mut self, part: Part<M>) -> Self {
        self.parts.push(part.register());
        self
    }

    /// Adds a long-lived service (HTTP server, worker, scheduler loop). It receives
    /// the shutdown token and must return soon after it is cancelled.
    #[must_use]
    pub fn service<F, Fut>(mut self, name: &'static str, run: F) -> Self
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        self.services.push(Service {
            name,
            run: Box::new(move |token| Box::pin(run(token))),
        });
        self
    }

    /// Runs until SIGINT/SIGTERM (or the shutdown token). Installs those handlers
    /// before any startup work.
    ///
    /// Reload is not built in: call [`reload_signal`](crate::tokio_runtime::reload_signal)
    /// before `run()`, and move the stream into a [`service`](Self::service) that
    /// selects on `recv()` and `token.cancelled()`.
    #[cfg(feature = "signal")]
    pub async fn run(self) -> Result<(), RunError> {
        let shutdown = crate::tokio_runtime::shutdown_signal().map_err(|error| RunError {
            failures: vec![Failure {
                phase: Phase::Start,
                name: "signal",
                message: error.to_string(),
            }],
        })?;
        self.run_until(async move {
            shutdown.await;
        })
        .await
    }

    /// Runs until `shutdown` completes or the shutdown token is cancelled.
    pub async fn run_until(self, shutdown: impl Future<Output = ()>) -> Result<(), RunError> {
        let Self {
            context,
            token,
            drain_timeout,
            mut parts,
            services,
            ..
        } = self;
        let mut failures = Vec::new();

        // Startup: a failure stops everything whose initialize succeeded.
        let mut initialized = 0;
        let phases: [(Phase, Select); 3] = [
            (Phase::Initialize, |h| &mut h.initialize),
            (Phase::Start, |h| &mut h.start),
            (Phase::Ready, |h| &mut h.ready),
        ];
        'startup: for (phase, select) in phases {
            for (index, part) in parts.iter_mut().enumerate() {
                if let Some(run) = select(&mut part.hooks)
                    && let Err(message) = call(run, &context)
                {
                    failures.push(Failure {
                        phase,
                        name: part.name,
                        message,
                    });
                    break 'startup;
                }
                if phase == Phase::Initialize {
                    initialized = index + 1;
                }
            }
        }
        if !failures.is_empty() {
            stop_parts(&mut parts[..initialized], &context, &mut failures);
            return Err(RunError { failures });
        }

        let mut running = JoinSet::new();
        let mut names = HashMap::new();
        for service in services {
            let handle = running.spawn((service.run)(token.clone()));
            names.insert(handle.id(), service.name);
        }
        // Any service leaving early (even Ok) ends the app: services are long-lived.
        let early = tokio::select! {
            () = shutdown => None,
            () = token.cancelled() => None,
            done = running.join_next_with_id(), if !running.is_empty() => done,
        };
        if let Some(done) = early {
            // A service that exited because the token was cancelled is a clean stop.
            let phase = if token.is_cancelled() {
                Phase::Drain
            } else {
                Phase::Service
            };
            record_exit(done, &mut names, phase, &mut failures);
        }

        // Shutdown: close admission (services), then drain and stop parts in reverse.
        token.cancel();
        let joined = tokio::time::timeout(drain_timeout, async {
            while let Some(done) = running.join_next_with_id().await {
                record_exit(done, &mut names, Phase::Drain, &mut failures);
            }
        })
        .await;
        if joined.is_err() {
            for name in names.values() {
                failures.push(Failure {
                    phase: Phase::Drain,
                    name,
                    message: format!("still running after {drain_timeout:?}; aborted"),
                });
            }
            // Parts must not stop while an aborted service still holds their handles,
            // so wait (briefly) for the abort to land.
            running.abort_all();
            let _ = tokio::time::timeout(ABORT_GRACE, async {
                while let Some(done) = running.join_next_with_id().await {
                    record_exit(done, &mut names, Phase::Drain, &mut failures);
                }
            })
            .await;
            // A task that never yields cannot be aborted.
            for name in names.values() {
                failures.push(Failure {
                    phase: Phase::Drain,
                    name,
                    message: format!("did not stop within {ABORT_GRACE:?} of abort"),
                });
            }
        }
        for part in parts.iter_mut().rev() {
            if let Some(hook) = part.hooks.drain.as_mut()
                && let Err(message) = call(hook, &context)
            {
                failures.push(Failure {
                    phase: Phase::Drain,
                    name: part.name,
                    message,
                });
            }
        }
        stop_parts(&mut parts, &context, &mut failures);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(RunError { failures })
        }
    }
}

/// Stops parts in reverse order, recording failures and continuing past them.
fn stop_parts(parts: &mut [Registered], context: &LifecycleContext, failures: &mut Vec<Failure>) {
    for part in parts.iter_mut().rev() {
        if let Some(hook) = part.hooks.stop.as_mut()
            && let Err(message) = call(hook, context)
        {
            failures.push(Failure {
                phase: Phase::Stop,
                name: part.name,
                message,
            });
        }
    }
}

/// Records a finished service and removes it from `names`. `Ok` counts as a
/// failure only for an early exit (`Phase::Service`); during drain a clean `Ok`
/// is the expected result, and an abort is already reported.
fn record_exit(
    done: Result<(tokio::task::Id, Result<(), String>), tokio::task::JoinError>,
    names: &mut HashMap<tokio::task::Id, &'static str>,
    phase: Phase,
    failures: &mut Vec<Failure>,
) {
    let (id, failure) = match done {
        Ok((id, Err(message))) => (id, Some(message)),
        Ok((id, Ok(()))) if phase == Phase::Service => (id, Some("unexpected exit".to_owned())),
        Ok((id, Ok(()))) => (id, None),
        Err(error) if error.is_cancelled() => (error.id(), None),
        Err(error) => (error.id(), Some(error.to_string())),
    };
    let name = names.remove(&id).unwrap_or("service");
    if let Some(message) = failure {
        failures.push(Failure {
            phase,
            name,
            message,
        });
    }
}
