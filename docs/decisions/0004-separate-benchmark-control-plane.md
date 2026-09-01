# ADR 0004: Separate benchmark control from experiment execution

- **Status:** Accepted
- **Date:** 2026-09-01

## Context

The original benchmark helper and CPU-idle runner are POSIX shell scripts. Their test suites mix
build provenance, host-state recovery, process-group teardown, and shell portability in the same
operational path as the Snoozer benchmark. That makes the control plane hard to evolve and invites
incorrect tests that treat a benchmark scenario as lifecycle evidence.

The coordinated wake-latency experiment remains project-specific. Its timing protocol, public API
matrix, topology checks, sample validation, and JSONL schema must not become generic control-tool
responsibilities.

## Decision

Introduce `tools/benchctl` as a Rust workspace package responsible only for benchmark build
receipts, CPU-idle policy application and restoration, durable operation records, process-group
lifecycle, status, and recovery.

The first contract targets trusted/cooperative CLI workloads. A crash guardian can prove the
owned process group empty, but Benchctl does not claim sandbox containment against a workload that
deliberately escapes its group. Real CPU-idle writes use one root coordinator invocation; the
workload drops to the invoking UID/GID. Client liveness is watched through a Linux pidfd, and a
coordinator crash drains without automatic restoration so the durable journal remains authoritative.

Benchctl and `wake_latency` communicate through a versioned receipt and an explicit workload
invocation. Benchctl lifecycle tests use a fake sysfs tree and a fixture workload; they must never
invoke Snoozer API strategies or benchmark scenarios. Benchmark unit and integration tests remain
under the benchmark's existing source and test owners and must not mutate CPU-idle state or test
Benchctl recovery.

The legacy shell helpers are removed once Benchctl owns the stable build, run, status, and recover
commands and its tests cover the legacy fail-closed behavior. A result decoder retains ownership of
its historical JSONL schema meaning; Benchctl does not translate benchmark result versions.

## Consequences

- Host-state correctness has one typed implementation and one isolated lifecycle suite.
- Benchmark results retain one experiment owner and cannot be mistaken for control-plane tests.
- Operation IDs, request hashes, and durable records make disconnect and crash recovery
  discoverable rather than dependent on a caller's private temporary directory.
- The operational CLI changes from script paths to `benchctl`; contributor and CI recipes must
  update together.

## Related sources

- [Benchctl control plane](../benchctl.md)
- [Benchmarking](../benchmarking.md)
- [ADR 0002: custom coordinated wake-latency harness](0002-custom-benchmark-harness.md)
