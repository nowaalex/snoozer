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

# Type-check every target with both the default and complete feature sets.
check:
    cargo check --workspace --all-targets
    cargo check --workspace --all-targets --all-features

# Cross-check the unsupported-target boundary for future Arm support.
check-arm:
    cargo check --workspace --all-features --target aarch64-unknown-linux-gnu

# Run Clippy for both feature sets with warnings treated as errors.
clippy:
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Run Benchctl's isolated fake-sysfs and fixture-workload suite.
benchctl-test:
    cargo test --package benchctl

# Run deterministic Snoozer tests for both feature sets; native hardware evidence is separate.
snoozer-test:
    cargo nextest run --package snoozer -E 'not binary(hardware_wait)'
    cargo nextest run --package snoozer --all-features -E 'not binary(hardware_wait)'

# Run both independently owned suites.
test: benchctl-test snoozer-test

# Run both nextest feature sets with the bounded CI profile.
test-ci:
    cargo nextest run --package snoozer --profile ci
    cargo nextest run --package snoozer --all-features --profile ci

# Run Rust documentation tests for both feature sets.
doctest:
    cargo test --workspace --doc
    cargo test --workspace --all-features --doc

# Build public API documentation with warnings denied.
docs:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps

# Run the complete unprivileged pull-request gate.
ci: fmt-check check check-arm clippy benchctl-test test-ci doctest docs

# Run cargo-mutants with the checked-in nextest policy.
mutants:
    cargo mutants

# Run the native x86 hardware-wait preflight and bounded hardware checks with visible skips.
hardware-test:
    cargo test --test hardware_wait -- --nocapture

# Require this host to pass its native AMD MWAITX or Intel UMWAIT hardware checks.
hardware-test-strict:
    SNOOZER_REQUIRE_HARDWARE_WAIT=1 cargo test --test hardware_wait -- --nocapture

# Run the short non-official benchmark without changing CPU-idle policy.
benchmark-smoke:
    cargo bench --bench wake_latency --features benchmark-only -- --smoke

# Build the clean, provenance-stamped official benchmark artifact.
benchmark-build:
    cargo run --locked --package benchctl -- build cargo-bench \
        --manifest-path Cargo.toml --bench wake_latency --feature benchmark-only \
        --receipt "${SNOOZER_BENCH_RECEIPT:-target/snoozer-bench/receipt.json}"

# Inspect an interrupted official operation by ID, or list known operations.
benchmark-status:
    cargo run --locked --package benchctl -- status ${SNOOZER_BENCH_OPERATION_ID:-}

# Recover an interrupted official run from its durable ownership record.
benchmark-recover:
    cargo run --locked --package benchctl -- recover ${SNOOZER_BENCH_OPERATION_ID:-}

# Run officially with only POLL/exact CPU C1; Intel UMWAIT still requests its separate C0.1 hint.
benchmark-official:
    #!/bin/sh
    set -eu
    : "${SNOOZER_WAITER_CPU:?set SNOOZER_WAITER_CPU}"
    : "${SNOOZER_VICTIM_CPU:?set SNOOZER_VICTIM_CPU}"
    : "${SNOOZER_PRODUCER_CPU:?set SNOOZER_PRODUCER_CPU}"
    : "${SNOOZER_CONTROLLER_CPU:?set SNOOZER_CONTROLLER_CPU}"
    receipt=${SNOOZER_BENCH_RECEIPT:-target/snoozer-bench/receipt.json}
    cargo run --locked --package benchctl -- build cargo-bench \
        --manifest-path Cargo.toml --bench wake_latency --feature benchmark-only \
        --receipt "$receipt"
    cargo run --locked --package benchctl -- run \
        --receipt "$receipt" --cpuidle poll-c1 \
        --cpu "$SNOOZER_WAITER_CPU" --cpu "$SNOOZER_VICTIM_CPU" \
        --cpu "$SNOOZER_PRODUCER_CPU" --cpu "$SNOOZER_CONTROLLER_CPU" -- \
        --official --waiter-cpu "$SNOOZER_WAITER_CPU" \
        --victim-cpu "$SNOOZER_VICTIM_CPU" \
        --producer-cpu "$SNOOZER_PRODUCER_CPU" \
        --controller-cpu "$SNOOZER_CONTROLLER_CPU"
