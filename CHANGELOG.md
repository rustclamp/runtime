# Changelog: rustclamp-runtime

## Unreleased

### Added

- Synchronous process-scoped task, cancellation, deadline, and recovery
  contracts with an executor-free deterministic manual driver.
- An optional Tokio adapter that can manage a runtime or adopt an existing
  runtime handle, supervise cooperative task cancellation, and translate
  Ctrl-C to a neutral signal value.
- Boundary checks and Phase 5 evidence for keeping Tokio out of Core and out
  of default Runtime builds.
