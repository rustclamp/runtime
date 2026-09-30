# Changelog: rustclamp-runtime

## Unreleased

### Added

- `app::AppRunner` (feature `tokio`): drives the core `Initialize/Start/Ready/Drain/Stop`
  traits (via `app::Part`) plus long-lived services under one shutdown token, with a
  drain timeout and unwinding on startup failure; `run()` (feature `signal`) joins
  SIGINT/SIGTERM and SIGHUP (`on_reload`). Proposed in ADR 0022.
- `TokioRuntime::managed_with_threads(NonZeroUsize)`; `managed()` keeps two workers.
- `CancellationToken::cancelled` and `TaskContext::cancelled` futures: awaitable,
  executor-neutral cancellation (own token or any parent). Dropped waiters
  deregister, so long-lived root tokens do not accumulate wakers.
- `tokio_runtime::shutdown_signal` (feature `signal`): installs SIGINT and, on
  Unix, SIGTERM handlers eagerly and returns a future that resolves on the first;
  new `ShutdownSignal::Terminate`.

- `tokio_runtime::reload_signal` / `ReloadSignal` (feature `signal`, Unix): SIGHUP
  as a repeatable `recv()`, installed eagerly (#4).
- `tokio_runtime::wait_for_shutdown` (feature `signal`): blocking SIGINT/SIGTERM wait
  for sync apps (#4).

### Changed

- `spawn_async` awaits cancellation instead of polling every 5 ms.
- Tokio `signal` moved behind a new `signal` feature; `tokio` alone no longer
  pulls `signal-hook-registry`/`errno`. `wait_for_ctrl_c` and `ShutdownSignal`
  require `signal` (ADR 0017).

### Added

- Synchronous process-scoped task, cancellation, deadline, and recovery
  contracts with an executor-free deterministic manual driver.
- An optional Tokio adapter that can manage a runtime or adopt an existing
  runtime handle, supervise cooperative task cancellation, and translate
  Ctrl-C to a neutral signal value.
- Boundary checks and Phase 5 evidence for keeping Tokio out of Core and out
  of default Runtime builds.
