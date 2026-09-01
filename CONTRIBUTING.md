# Contributing

Thank you for helping improve snoozer. Changes should keep the raw hardware boundary small, make
unsupported behavior visible, and avoid performance claims that are not backed by a controlled
measurement.

## Toolchain

The repository pins Rust in [`rust-toolchain.toml`](rust-toolchain.toml). Install
[cargo-nextest](https://nexte.st/) at the minimum version required by
[`.config/nextest.toml`](.config/nextest.toml), and install
[cargo-mutants](https://mutants.rs/):

```console
cargo install cargo-nextest --locked --version 0.9.140
cargo install cargo-mutants --locked --version 27.1.0
```

## Before sending a change

Run the same unprivileged checks as CI:

```console
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace --all-features
cargo test --workspace --all-features --doc
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
```

Run mutation testing for changes to protocol and strategy logic:

```console
cargo mutants
```

The checked-in [cargo-mutants configuration](.cargo/mutants.toml) selects nextest, the workspace,
and all features. It mutation-tests the portable strategy and token protocols with a bounded
nextest profile. Pure CPUID decoding and leaf interpretation, sample collection, TSC
reconstruction, timer-calibration arithmetic, and the production MWAITX
arm/recheck/classification protocol stay in that mutation set. Only live CPUID/RDTSCP and
wall-clock sample acquisition, inline assembly, and the thin native hardware dispatch are
excluded because their result depends on the CI host; those paths instead require focused
capability, encoding/safety review, and bounded target-hardware tests. Cfg-inactive
unsupported-target stubs are also excluded as equivalent on the mutation host and are
cross-compiled by CI.

## Hardware smoke tests

The AMD hardware tests perform bounded real `MONITORX/MWAITX` waits when the host supports
them and report an unsupported host without executing the instructions:

```console
cargo test --test amd_mwaitx
```

These tests establish basic notification and timer progress. They are not latency benchmarks.
Record the CPU model, kernel, and test output when reporting a hardware-only failure.

## Benchmarks

Read [Benchmarking](docs/benchmarking.md) before running measurements. In particular, an official
run temporarily changes CPU-idle state controls and must preserve its recovery manifest.

Run a short, non-official smoke measurement without changing CPU-idle state:

```console
cargo bench --bench wake_latency --features benchmark-only -- --smoke
```

Smoke mode reads and reports the current CPU-idle configuration and prints a `NON-OFFICIAL`
warning. It does not disable C2/C3 or deeper states and cannot produce a publishable result.

For an official run, build the exact feature-enabled artifact, select four logical CPUs that
satisfy the topology contract in [Benchmarking](docs/benchmarking.md), and use the
state-preserving runner. Run the script as your normal user; it invokes `sudo` only for the
required sysfs writes.

```sh
SNOOZER_BENCH_BINARY=$(scripts/build_benchmark.sh)
SNOOZER_WAITER_CPU=CPU
SNOOZER_VICTIM_CPU=CPU
SNOOZER_PRODUCER_CPU=CPU
SNOOZER_CONTROLLER_CPU=CPU

scripts/run_with_cpuidle.sh \
  --binary "$SNOOZER_BENCH_BINARY" \
  --waiter-cpu "$SNOOZER_WAITER_CPU" \
  --victim-cpu "$SNOOZER_VICTIM_CPU" \
  --producer-cpu "$SNOOZER_PRODUCER_CPU" \
  --controller-cpu "$SNOOZER_CONTROLLER_CPU" \
  -- --official
```

`scripts/build_benchmark.sh` enables the repository-only `benchmark-only` feature so the
C1 diagnostic comparison is present and prints the exact executable path. The runner's
`--help` output owns its current command-line interface.

Official results are valid only when the runner and benchmark complete topology checks, disable
C2/C3 and deeper states on assigned CPUs, verify the new state, and restore the original state.

If a prior process was killed before restoration, do not start another run. Recover the manifest
as the same user and with the same `SNOOZER_STATE_DIR` and `SNOOZER_SYSFS_ROOT` values, if
you overrode them:

```console
scripts/run_with_cpuidle.sh --recover
```

## Documentation ownership

- Public behavior and selection guidance belong in
  [`docs/waiting-api.md`](docs/waiting-api.md).
- Backend layering belongs in [`docs/architecture.md`](docs/architecture.md).
- Atomic, lifetime, and assembly invariants belong in [`docs/safety.md`](docs/safety.md).
- Measurement and recovery procedures belong in
  [`docs/benchmarking.md`](docs/benchmarking.md).
- Expensive cross-cutting choices belong in an immutable record under
  [`docs/decisions/`](docs/decisions/README.md).

Do not copy tunable numeric settings into prose. Link to their source owner and ensure the runtime
output records the selected values.

## Change expectations

- Add deterministic fake-backend tests for protocol changes.
- Add a bounded target-hardware test for new instruction paths.
- Keep every `unsafe` block minimal and add an adjacent `// SAFETY:` explanation.
- Return a typed unsupported error; do not add an automatic fallback.
- Update the focused owner document when a public contract, invariant, backend boundary, or
  benchmark procedure changes.
- Keep repository-facing text, comments, output labels, and identifiers in English.
