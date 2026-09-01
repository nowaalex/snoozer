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
syntax_status=0
for script in scripts/run_with_cpuidle.sh scripts/test_run_with_cpuidle.sh scripts/build_benchmark.sh scripts/test_build_benchmark.sh; do
  sh -n "$script" || syntax_status=1
done
[ "$syntax_status" -eq 0 ]
timeout --kill-after=5s 30s sh scripts/test_build_benchmark.sh
timeout --kill-after=5s 240s sh scripts/test_run_with_cpuidle.sh
```

The shell suites use only disposable fixtures and fake CPU sysfs trees. They do not invoke the
privileged runner against the machine's real CPU-idle controls.

The step-level `timeout` commands bound the directly supervised suite process; they are not a
containment boundary for a descendant that outlives that process. CI additionally bounds the
whole job and relies on GitHub-hosted runner teardown. When running locally, use a disposable
process environment and check for surviving test descendants after a forced timeout.

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

The AMD hardware tests perform bounded real `MONITORX/MWAITX` waits when the host supports them.
An ordinary optional probe prints a visible `SKIP` diagnostic and returns successfully on an
unsupported host:

```console
cargo test --test amd_mwaitx -- --nocapture
```

To cite the command as target-hardware evidence, enable the strict gate so an unsupported target,
CPU, or timer configuration is a test failure. The strict suite includes an equal-atomic raw wait
that necessarily reaches one real MWAITX instruction and is bounded by its internal safety timer:

```console
SNOOZER_REQUIRE_AMD_MWAITX=1 cargo test --test amd_mwaitx
```

`SNOOZER_REQUIRE_AMD_MWAITX` must be unset or exactly `1`; other values fail visibly so a typo
cannot silently disable the gate. These tests establish basic notification and timer progress.
They are not latency benchmarks. Record the CPU model, kernel, and test output when reporting a
hardware-only failure.

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
required sysfs writes and for the root-owned global lock and dirty-owner recovery metadata under
`/run/lock`.

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

If a prior process was killed before restoration, do not start another run. Recover as the same
user and with the same `SNOOZER_SYSFS_ROOT` value if you overrode it. The global dirty-owner record
selects the authoritative private state directory, even when recovery is invoked with a different
`SNOOZER_STATE_DIR`. A custom integration must also provide the trusted write helper needed to
restore its selected sysfs tree if the original run set `SNOOZER_WRITE_HELPER`:

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
