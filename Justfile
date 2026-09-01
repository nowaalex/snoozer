set shell := ["sh", "-eu", "-c"]

# List the documented development recipes.
default:
    @just --list

# Format Rust sources in place.
fmt:
    cargo fmt --all

# Check Rust formatting without changing files.
fmt-check:
    cargo fmt --all -- --check

# Type-check every target and feature on the host.
check:
    cargo check --workspace --all-targets --all-features

# Cross-check the unsupported-target boundary for future Arm support.
check-arm:
    cargo check --workspace --all-features --target aarch64-unknown-linux-gnu

# Run Clippy with warnings treated as errors.
clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Run the ordinary nextest suite.
test:
    cargo nextest run --workspace --all-features

# Run nextest with the bounded CI profile.
test-ci:
    cargo nextest run --workspace --all-features --profile ci

# Run Rust documentation tests.
doctest:
    cargo test --workspace --all-features --doc

# Build public API documentation with warnings denied.
docs:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps

# Parse every benchmark shell script independently.
shell-check:
    #!/bin/sh
    set -eu
    syntax_status=0
    for script in \
        scripts/run_with_cpuidle.sh \
        scripts/test_run_with_cpuidle.sh \
        scripts/build_benchmark.sh \
        scripts/test_build_benchmark.sh; do
        sh -n "$script" || syntax_status=1
    done
    [ "$syntax_status" -eq 0 ]

# Exercise build provenance and CPU-idle recovery against disposable fixtures.
shell-test: shell-check
    timeout --kill-after=5s 30s sh scripts/test_build_benchmark.sh
    timeout --kill-after=5s 240s sh scripts/test_run_with_cpuidle.sh

# Run the complete unprivileged pull-request gate.
ci: fmt-check check check-arm clippy shell-test test-ci doctest docs

# Run cargo-mutants with the checked-in nextest policy.
mutants:
    cargo mutants

# Run optional AMD MWAITX hardware checks with visible skip diagnostics.
hardware-test:
    cargo test --test amd_mwaitx -- --nocapture

# Require this host to execute every bounded AMD MWAITX hardware check.
hardware-test-strict:
    SNOOZER_REQUIRE_AMD_MWAITX=1 cargo test --test amd_mwaitx -- --nocapture

# Run the short non-official benchmark without changing CPU-idle policy.
benchmark-smoke:
    cargo bench --bench wake_latency --features benchmark-only -- --smoke

# Build the clean, provenance-stamped official benchmark artifact.
benchmark-build:
    scripts/build_benchmark.sh

# Recover an interrupted official run from its durable ownership record.
benchmark-recover:
    scripts/run_with_cpuidle.sh --recover

# Run officially with only POLL/exact C1; disables C1E and all other states, including C2/C3+.
benchmark-official:
    #!/bin/sh
    set -eu
    : "${SNOOZER_WAITER_CPU:?set SNOOZER_WAITER_CPU}"
    : "${SNOOZER_VICTIM_CPU:?set SNOOZER_VICTIM_CPU}"
    : "${SNOOZER_PRODUCER_CPU:?set SNOOZER_PRODUCER_CPU}"
    : "${SNOOZER_CONTROLLER_CPU:?set SNOOZER_CONTROLLER_CPU}"
    benchmark_binary=$(scripts/build_benchmark.sh)
    scripts/run_with_cpuidle.sh \
        --binary "$benchmark_binary" \
        --waiter-cpu "$SNOOZER_WAITER_CPU" \
        --victim-cpu "$SNOOZER_VICTIM_CPU" \
        --producer-cpu "$SNOOZER_PRODUCER_CPU" \
        --controller-cpu "$SNOOZER_CONTROLLER_CPU" \
        -- --official
