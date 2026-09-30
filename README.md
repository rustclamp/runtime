<img src="https://docs.rustclamp.com/assets/rustclamp-logo.png" alt="RustClamp logo" width="160">

# rustclamp-runtime

Runtime component of [RustClamp](https://github.com/rustclamp/rustclamp):
synchronous task supervision and application lifecycle, with an optional Tokio
adapter. Base contracts contain no Tokio types and need no `async`. Companion
crate, not a standalone framework.

## Install

Not published to crates.io yet (`publish = false`). Depend on it from git, Rust 1.96.1+:

```toml
[dependencies]
rustclamp-runtime = { git = "https://github.com/rustclamp/runtime" }
```

Enable features as needed: `features = ["tokio"]` or `["signal"]`.

## Example

```rust
use rustclamp_runtime::app::AppRunner;
use std::time::Duration;

// application/process ids come from rustclamp-core
let runner = AppRunner::new(application, process)
    .drain_timeout(Duration::from_secs(10))
    .service("worker", |shutdown| async move {
        shutdown.cancelled().await; // run until asked to stop
        Ok(())
    });
runner.run().await?; // returns on SIGINT/SIGTERM (feature `signal`)
```

## Main API

- **Supervision (no features):** `Supervisor`, `TaskDefinition`, `TaskKind`,
  `FailurePolicy`, `Supervision`, `TaskExit`, `TaskRuntime`, the deterministic
  `ManualRuntime`, and cooperative `CancellationToken` / `TaskContext`
  (awaitable `cancelled`).
- **`app::AppRunner` (`tokio`):** runs Core's `Initialize`/`Start`/`Ready`/`Drain`/`Stop`
  parts (`Part`) and long-lived services under one shutdown token, with a drain
  timeout and unwinding on startup failure. `run` (`signal`) joins SIGINT/SIGTERM;
  `run_until` takes any shutdown future.
- **`tokio_runtime::TokioRuntime` (`tokio`):** `managed()` (two workers),
  `managed_with_threads(NonZeroUsize)`, or adopt an existing handle; sync tasks
  run on the blocking pool, async tasks share the same recovery policies.
- **Signals (`signal`):** `shutdown_signal`, `wait_for_shutdown` (blocking, for sync
  apps), `TokioRuntime::wait_for_ctrl_c`, and `reload_signal` / `ReloadSignal`
  (SIGHUP, Unix).

Cancellation is cooperative: it cannot undo committed side effects, and a task
that ignores it may outlive its join timeout.

| Feature | Adds |
| --- | --- |
| default | nothing (0 external dependencies) |
| `tokio` | Tokio adapter, `AppRunner` |
| `signal` | `tokio` + Tokio signal handling |

See [CHANGELOG.md](CHANGELOG.md).

## Documentation

<https://docs.rustclamp.com>

## Development

```sh
cargo fmt --all -- --check
cargo clippy --offline --locked --all-targets --all-features -- -D warnings
cargo test --offline --locked --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --offline --locked --no-deps --all-features
```

Coordinated checkout, architecture checks and release policy: see the
[facade contributor guide](https://github.com/rustclamp/rustclamp/blob/main/CONTRIBUTING.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you state otherwise, any
contribution you submit for inclusion is dual licensed as above, without
additional terms or conditions.
