<img src="https://raw.githubusercontent.com/rustclamp/docs.rustclamp.com/main/assets/rustclamp-logo.png" alt="RustClamp logo" width="160">

# rustclamp-runtime

Runtime-neutral synchronous task supervision contracts for RustClamp. Tasks
declare their process, whether they are finite or long-lived, whether they are
required, and how failures should be recovered. `TaskRuntime` can be driven by
the deterministic `ManualRuntime` or by the optional Tokio adapter.

The default feature set has no external crate dependencies. Tokio is an
optional feature; its adapter can own a multithreaded runtime or adopt a caller's
existing Tokio handle. The adapter runs synchronous tasks on Tokio's blocking pool, supports native
async tasks with the same recovery policies, and translates Ctrl-C into a
neutral signal value. Core and the base runtime contract contain no Tokio types
and require no async API.

Cancellation is cooperative. It cannot undo committed side effects, and a task
that ignores cancellation may outlive its join timeout. Panic results are
reported when unwinding is enabled; panic-abort builds cannot recover them.
Applications choose failure policy because only they know whether work is
critical. RestartOnce is deliberately limited to one retry in this proof.

| Mode | External packages | Executor required | Purpose |
| --- | ---: | --- | --- |
| Default / ManualRuntime | 0 | No | Deterministic coordination tests |
| `tokio` feature | Tokio | Yes | Supervised task adapter |
| `signal` feature | `tokio` + Tokio `signal` (platform signal crates) | Yes | `TokioRuntime::wait_for_ctrl_c` |

The lifecycle example demonstrates process-specific runtime selection, optional
feature activation, task supervision, task-stop deadlines, and fake-resource
startup/shutdown. Its async dependency remains confined to the optional Tokio adapter. The base
contract is synchronous; native async operations use the adapter's supervision
path and retain the same cancellation and failure policy.

```sh
cargo fmt --all -- --check
cargo clippy --offline --locked --all-targets --all-features -- -D warnings
cargo test --offline --locked --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --offline --locked --no-deps --all-features
```

For the measured adapter comparison and limitations, see the [Phase 5 evidence](../rustclamp/docs/evidence/phase5-lifecycle.md).
