#!/bin/sh
set -eu

has_ancestor_cargo_config() {
    inspected_directory=$1
    while :; do
        if [ -e "$inspected_directory/.cargo/config" ] \
            || [ -e "$inspected_directory/.cargo/config.toml" ]; then
            return 0
        fi
        [ "$inspected_directory" != / ] || return 1
        inspected_directory=${inspected_directory%/*}
        [ -n "$inspected_directory" ] || inspected_directory=/
    done
}

fixture_parent=${TMPDIR:-/tmp}
fixture_parent=$(CDPATH= cd -- "$fixture_parent" && pwd -P) \
    || { echo "benchmark build test cannot resolve TMPDIR" >&2; exit 1; }
if has_ancestor_cargo_config "$fixture_parent"; then
    fixture_parent=/tmp
fi
if has_ancestor_cargo_config "$fixture_parent"; then
    echo "benchmark build test needs a temporary directory without an ancestor Cargo config" >&2
    exit 1
fi
test_root=$(mktemp -d "$fixture_parent/snoozer-build-benchmark-test.XXXXXX")
trap 'rm -rf "$test_root"' EXIT HUP INT TERM
repository=$test_root/repository
fake_bin=$test_root/bin
test_home=$test_root/home
mkdir -p "$repository/scripts" "$fake_bin" "$test_home"
cp "$(CDPATH= cd -- "$(dirname "$0")" && pwd)/build_benchmark.sh" \
    "$repository/scripts/build_benchmark.sh"
chmod +x "$repository/scripts/build_benchmark.sh"

printf '[workspace]\nmembers = []\n' >"$repository/Cargo.toml"
printf '[toolchain]\nchannel = "1.98.0"\n' >"$repository/rust-toolchain.toml"

cat >"$fake_bin/rustc" <<'EOF'
#!/bin/sh
printf 'rustc %s (fixture 2026-08-03)\n' "${SNOOZER_TEST_RUSTC_VERSION:-1.98.0}"
EOF
chmod +x "$fake_bin/rustc"

cat >"$fake_bin/cargo" <<'EOF'
#!/bin/sh
printf '%s\n' '{"reason":"compiler-artifact","target":{"name":"wake_latency","kind":["bench"]},"executable":"/tmp/fake-wake-latency"}'
EOF
chmod +x "$fake_bin/cargo"

git -C "$repository" init -q
git -C "$repository" add Cargo.toml rust-toolchain.toml scripts/build_benchmark.sh
git -C "$repository" \
    -c user.name='Snoozer Test' -c user.email='snoozer@example.invalid' \
    commit -qm fixture

run_clean() {
    env -i HOME="$test_home" PATH="$fake_bin:$PATH" "$@"
}

run_helper() {
    run_clean "$repository/scripts/build_benchmark.sh"
}

[ "$(run_helper)" = /tmp/fake-wake-latency ]

printf 'untracked\n' >"$repository/untracked-input"
set +e
untracked_tree_output=$(run_helper 2>&1)
untracked_tree_status=$?
set -e
[ "$untracked_tree_status" -ne 0 ]
printf '%s\n' "$untracked_tree_output" | grep -q 'clean working tree'
rm "$repository/untracked-input"

cp "$repository/Cargo.toml" "$test_root/Cargo.toml.clean"
printf '# tracked modification\n' >>"$repository/Cargo.toml"
set +e
tracked_tree_output=$(run_helper 2>&1)
tracked_tree_status=$?
set -e
[ "$tracked_tree_status" -ne 0 ]
printf '%s\n' "$tracked_tree_output" | grep -q 'clean working tree'
cp "$test_root/Cargo.toml.clean" "$repository/Cargo.toml"

# A symlinked invocation still resolves the physical repository and checks its
# physical ancestor chain for Cargo configuration.
mkdir -p "$test_root/.cargo" "$test_root/invocation"
printf '[build]\nrustflags = ["-Ctarget-cpu=native"]\n' \
    >"$test_root/.cargo/config.toml"
ln -s "$repository/scripts/build_benchmark.sh" "$test_root/invocation/build-benchmark"
set +e
physical_ancestor_output=$(run_clean "$test_root/invocation/build-benchmark" 2>&1)
physical_ancestor_status=$?
set -e
[ "$physical_ancestor_status" -ne 0 ]
printf '%s\n' "$physical_ancestor_output" | grep -q 'outside the tracked repository'
rm -f "$test_root/.cargo/config.toml"
rmdir "$test_root/.cargo"
[ "$(run_clean "$test_root/invocation/build-benchmark")" = /tmp/fake-wake-latency ]

mkdir -p "$repository/.cargo"
printf '[build]\nrustflags = ["-Ctarget-cpu=native"]\n' \
    >"$repository/.cargo/config.toml"
set +e
untracked_output=$(run_helper 2>&1)
untracked_status=$?
set -e
[ "$untracked_status" -ne 0 ]
printf '%s\n' "$untracked_output" | grep -Eq 'untracked or ignored|working tree'
rm -f "$repository/.cargo/config.toml"
rmdir "$repository/.cargo"

set +e
override_output=$(run_clean RUSTFLAGS=-Ctarget-cpu=native \
    "$repository/scripts/build_benchmark.sh" 2>&1)
override_status=$?
set -e
[ "$override_status" -ne 0 ]
printf '%s\n' "$override_output" | grep -q 'RUSTFLAGS'

set +e
profile_output=$(run_clean CARGO_PROFILE_RELEASE_LTO=true \
    "$repository/scripts/build_benchmark.sh" 2>&1)
profile_status=$?
set -e
[ "$profile_status" -ne 0 ]
printf '%s\n' "$profile_output" | grep -q 'CARGO_PROFILE_RELEASE_LTO'

mkdir -p "$test_home/.cargo"
printf '[build]\nrustflags = ["-Ctarget-cpu=native"]\n' \
    >"$test_home/.cargo/config.toml"
set +e
external_config_output=$(run_helper 2>&1)
external_config_status=$?
set -e
[ "$external_config_status" -ne 0 ]
printf '%s\n' "$external_config_output" | grep -q 'outside the tracked repository'
rm -f "$test_home/.cargo/config.toml"

set +e
cargo_home_output=$(run_clean CARGO_HOME="$test_root/alternate-cargo-home" \
    RUSTUP_TOOLCHAIN=1.98.0-x86_64-unknown-linux-gnu \
    "$repository/scripts/build_benchmark.sh" 2>&1)
cargo_home_status=$?
set -e
[ "$cargo_home_status" -ne 0 ]
printf '%s\n' "$cargo_home_output" | grep -q 'CARGO_HOME'

set +e
rustup_output=$(run_clean RUSTUP_TOOLCHAIN=nightly \
    "$repository/scripts/build_benchmark.sh" 2>&1)
rustup_status=$?
set -e
[ "$rustup_status" -ne 0 ]
printf '%s\n' "$rustup_output" | grep -q 'RUSTUP_TOOLCHAIN'

set +e
wrong_rustc_output=$(run_clean SNOOZER_TEST_RUSTC_VERSION=1.99.0 \
    "$repository/scripts/build_benchmark.sh" 2>&1)
wrong_rustc_status=$?
set -e
[ "$wrong_rustc_status" -ne 0 ]
printf '%s\n' "$wrong_rustc_output" | grep -q 'does not match pinned Rust 1.98.0'

SNOOZER_TEST_REAL_GIT=$(command -v git)
cat >"$fake_bin/git" <<'EOF'
#!/bin/sh
if [ "$1" = -C ] && [ "$3" = status ]; then
    exit 74
fi
exec "$SNOOZER_TEST_REAL_GIT" "$@"
EOF
chmod +x "$fake_bin/git"
set +e
status_failure_output=$(run_clean SNOOZER_TEST_REAL_GIT="$SNOOZER_TEST_REAL_GIT" \
    "$repository/scripts/build_benchmark.sh" 2>&1)
status_failure_status=$?
set -e
[ "$status_failure_status" -ne 0 ]
printf '%s\n' "$status_failure_output" \
    | grep -q 'cannot verify that the benchmark working tree is clean'
rm "$fake_bin/git"

echo "benchmark build provenance preflight tests: PASS"
