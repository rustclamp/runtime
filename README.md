<img src="https://raw.githubusercontent.com/rustclamp/docs.rustclamp.com/main/assets/rustclamp-logo.png" alt="RustClamp logo" width="160">

# rustclamp-runtime

The planned **Runtime component of RustClamp**, the framework in the
[`rustclamp`](https://github.com/rustclamp/rustclamp) repository. Runtime is
intended to provide execution-environment contracts selected by each process.

This is a companion package, not a standalone framework. It is currently a Phase 0
scaffold: there are no public contracts yet. This package builds alone
with Rust 1.96.1 and has no dependencies. Publishing is disabled until licensing,
registry ownership and the first prototype API have been reviewed.

Runtime contracts should let each process select the execution model it needs.
Kernel coordination and runtime execution are separate responsibilities:

```mermaid
flowchart LR
    Projection[Resolved process] --> Needs[Execution requirements]
    Needs --> Select[Select runtime per process]
    Select --> Drive[Runtime drives work]
    Kernel[Kernel coordinates] --> Drive
    Drive --> Shutdown[Cancellation and bounded shutdown]
```

| Baseline | Current result |
| --- | --- |
| External Rust dependencies | 0 |
| Public runtime contracts | 0 |
| Executor requirement | None |
| Runtime measurements | Not applicable until behavior exists |

When a driver is implemented, compare it with equivalent direct Rust under the
same workload. Record dependencies, build and binary cost, tasks/threads,
allocations, startup-to-ready time, throughput, and shutdown duration where
relevant. Missing observations are unavailable, never zero.

```sh
cargo fmt --all -- --check
cargo clippy --offline --locked --all-targets --all-features -- -D warnings
cargo test --offline --locked --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --offline --locked --no-deps --all-features
```

For coordinated checkout, architecture checks, measurements, and release policy,
see the [facade contributor guide](https://github.com/rustclamp/rustclamp/blob/main/CONTRIBUTING.md).
The configured remote is `https://github.com/rustclamp/runtime.git`; repository existence
and public visibility were verified during Phase 0.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you state otherwise, any
contribution you submit for inclusion is dual licensed as above, without
additional terms or conditions.
