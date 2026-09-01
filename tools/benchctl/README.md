# Benchctl

## Change Contract

Benchctl owns reproducible Cargo benchmark builds, versioned build receipts, temporary Linux
CPU-idle policy changes, workload supervision, durable operation journals, status, and recovery.
It must journal original state before the first write, read applied values back, drain the trusted
workload process group before restoration, and retain ambiguous recovery state.

Benchctl must not depend on Snoozer, implement benchmark scenarios, interpret latency samples, or
own the benchmark result schema. Runtime defaults and CLI parsing are owned by
[`src/cli.rs`](src/cli.rs); lifecycle invariants are owned by [`src/cpuidle.rs`](src/cpuidle.rs)
and targeted by `cargo test --package benchctl`.

## Commands

```console
benchctl build cargo-bench --manifest-path Cargo.toml --bench wake_latency \
  --feature benchmark-only --receipt target/snoozer-bench/receipt.json

benchctl run --receipt target/snoozer-bench/receipt.json \
  --cpuidle poll-c1 --cpu 2 --cpu 3 --cpu 4 --cpu 6 -- \
  --official --waiter-cpu 2 --victim-cpu 3 --producer-cpu 4 --controller-cpu 6

benchctl status [OPERATION_ID]
benchctl recover [OPERATION_ID]
```

On the real kernel CPU sysfs tree, the public process invokes the same executable once through
`sudo`. Only that coordinator owns the lock, journal, and sysfs writes; the workload runs with the
invoking user's UID and GID. Alternate roots are hidden integration-test seams and never establish
official benchmark evidence.

The coordinator copies the validated build receipt and executable into root-owned operation state
before the first host mutation. The supervisor hashes and executes the already-open snapshot
descriptor, so replacing the caller-owned artifact path cannot change the workload. The
unprivileged workload receives the immutable receipt through `BENCHCTL_BUILD_RECEIPT_JSON`. For the
fixed real-sysfs backend it also passes a root-owned
one-shot proof pipe in `BENCHCTL_PRODUCTION_CONTROL_FD`; ordinary environment variables cannot
manufacture official-run evidence, and user-namespace root is not accepted as host root.

The coordinator watches the public client through a Linux pidfd; client exit cancels the operation
without consuming workload stdin. A separate guardian
owns the active-operation lock and drains the cooperative workload process group if the coordinator
dies. Coordinator death leaves CPU-idle state and the journal recoverable; it does not restore
under a possibly live workload. `recover` performs conditional restoration and retains any third-
party value conflict.

Benchctl is reusable as a pattern for trusted console workloads and typed Linux state adapters, but
it is not a process sandbox. A hostile workload can escape process-group supervision with
`setsid`/`setpgid`; callers that need hostile-code containment need a cgroup-based boundary.

## Development

```console
cargo fmt --all -- --check
cargo check --package benchctl --all-targets
cargo clippy --package benchctl --all-targets -- -D warnings
cargo test --package benchctl
```

Benchctl tests use disposable files and tiny fixture programs. They never execute `wake_latency`,
Snoozer waiting strategies, timing calibration, or result-schema assertions.
