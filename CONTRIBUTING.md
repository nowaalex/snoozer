# Contributing

Thank you for helping improve snoozer. Changes should keep the raw hardware boundary small, make
unsupported behavior visible, and avoid performance claims that are not backed by a controlled
measurement.

## Toolchain

The repository pins Rust in [`rust-toolchain.toml`](rust-toolchain.toml). Install
[cargo-nextest](https://nexte.st/) at the minimum version required by
[`.config/nextest.toml`](.config/nextest.toml), install the `just` version pinned by
[CI](.github/workflows/ci.yml), and install
[cargo-mutants](https://mutants.rs/):

```console
cargo install cargo-nextest --locked --version 0.9.140
cargo install cargo-mutants --locked --version 27.1.0
```

## Before sending a change

The [`Justfile`](Justfile) is the single owner of the unprivileged pull-request gate and the
development command index. Run `just` or `just --list` to see documented recipes. Benchmark and
hardware recipes remain explicit separate operations.

Run the same unprivileged checks as CI:

```console
just ci
```

The Benchctl lifecycle suite uses only disposable fixtures, fake CPU sysfs trees, and fixture
workloads. It does not invoke `sudo` or change the machine's real CPU-idle controls.

Run mutation testing for changes to protocol and strategy logic:

```console
cargo mutants
```

The checked-in [cargo-mutants configuration](.cargo/mutants.toml) selects nextest, the Snoozer
package, and all features. Benchctl is an independent package with its own complete CI suite and is
not rebuilt for every Snoozer mutant. The bounded mutation profile excludes the native
`hardware_wait` test binary: deterministic
mutations run without host latency noise, while `just hardware-test-strict` supplies the separate
instruction evidence. Pure CPUID decoding and leaf interpretation, sample collection, TSC
reconstruction, timer-calibration arithmetic, and the production MWAITX
arm/recheck/classification protocol stay in that mutation set. Only live CPUID/RDTSCP and
wall-clock sample acquisition, inline assembly, and the thin native hardware dispatch are
excluded because their result depends on the CI host; those paths instead require focused
capability, encoding/safety review, and bounded target-hardware tests. Cfg-inactive
unsupported-target stubs are also excluded as equivalent on the mutation host and are
cross-compiled by CI.

## Hardware smoke tests

The hardware tests run the process-wide preflight and bounded native waits on a supported AMD
`MONITORX/MWAITX` or Intel `UMONITOR/UMWAIT` host. The Intel production path requests C0.1 only;
C0.2 is not a production mode, and neither name refers to Linux CPU idle states C2 or C3. An
ordinary optional probe prints a visible `SKIP` diagnostic and returns successfully on an
unsupported host:

```console
just hardware-test
```

To cite the command as target-hardware evidence, enable the strict gate so an unsupported target,
CPU, timer configuration, or failed operational preflight is a test failure:

```console
just hardware-test-strict
```

The strict recipe owns the exact environment variable and integration-test target. These tests are
bounded operational evidence: they show that the controlled publication/wait trials completed,
but do not prove the exact event that ended an individual hardware wait. They are not latency
benchmarks. Record the selected backend, CPU model, kernel, preflight report, and test output when
reporting a hardware-only failure.

## Benchmarks

Read [Benchmarking](docs/benchmarking.md) before running measurements. In particular, an official
run temporarily changes CPU-idle state controls and must preserve its durable operation journal.

Run a short, non-official smoke measurement without changing CPU-idle state:

```console
cargo bench --bench wake_latency --features benchmark-only -- --smoke
```

Smoke mode reads and reports the current CPU-idle configuration and prints a `NON-OFFICIAL`
warning. It records hardware preflight status and selects the native vendor's hardware cases only
after a pass; on failure it runs only the explicitly portable cases. It does not change CPU-idle
policy and cannot produce a publishable result.

For an official run, select four logical CPUs that satisfy the topology contract in
[Benchmarking](docs/benchmarking.md), then use Benchctl. Run it as your normal user; it invokes
`sudo` once to start its privileged coordinator. Benchctl owns the locked clean build receipt,
exact sysfs state journal, process-group guardian, and recovery record.

```sh
SNOOZER_WAITER_CPU=CPU
SNOOZER_VICTIM_CPU=CPU
SNOOZER_PRODUCER_CPU=CPU
SNOOZER_CONTROLLER_CPU=CPU

just benchmark-official
```

`just benchmark-build` creates a receipt without changing CPU-idle state. `just
benchmark-official` creates or replaces that receipt and uses it for the run. The `benchctl --help`
output and [Benchctl documentation](docs/benchctl.md) own the exact control-plane interface.

Official results are valid only when Benchctl and the benchmark complete topology and native
hardware preflight checks, permit only POLL and exact CPU C1 on assigned CPUs, disable C1E and
every other state including CPU C2/CPU C3+, verify the new state, and restore the original state.
Intel UMWAIT C0.1 is a separate instruction hint, not that CPU C1 state.

If a prior process was killed before restoration, do not start another run. Inspect its durable
operation record and recover it as the same user:

```console
just benchmark-status
just benchmark-recover
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
