mod support;

use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use support::{Rig, read, stderr, stdout};

#[test]
fn successful_run_restores_and_retries_idempotently() {
    let rig = Rig::new("1");
    let first = rig.run("stable-operation", "check", 10);
    assert!(first.status.success(), "{}", stderr(&first));
    rig.assert_originals();

    let retry = rig.run("stable-operation", "check", 10);
    assert!(retry.status.success(), "{}", stderr(&retry));
    assert!(stdout(&retry).contains("completed"));
    rig.assert_originals();

    let status = rig.status("stable-operation");
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(stdout(&status).contains("Restored"));

    let changed = rig
        .command()
        .args(["run", "--receipt"])
        .arg(&rig.receipt)
        .args([
            "--operation-id",
            "stable-operation",
            "--cpuidle",
            "poll-c1",
            "--cpu",
            "1",
            "--timeout",
            "10",
            "--sysfs-root",
        ])
        .arg(&rig.sysfs)
        .arg("--state-root")
        .arg(&rig.state)
        .args(["--", "check"])
        .output()
        .expect("run changed request");
    assert!(!changed.status.success());
    assert!(stderr(&changed).contains("conflicts with its recorded request"));
    rig.assert_originals();
}

#[test]
fn failed_workload_retry_replays_failure_after_restoration() {
    let rig = Rig::new("1");
    let first = rig.run("failed-operation", "fail", 10);
    assert!(!first.status.success());
    assert!(stderr(&first).contains("exit code 7"));
    rig.assert_originals();

    let retry = rig.run("failed-operation", "fail", 10);
    assert!(!retry.status.success());
    assert!(stderr(&retry).contains("exit code 7"));
    rig.assert_originals();
}

#[test]
fn recovery_replays_outcome_persisted_before_coordinator_crash() {
    let rig = Rig::new("1");
    let outcome_ready = rig.receipt.with_file_name("outcome-ready");
    let mut command = rig.run_command("lost-response", "fail", 30);
    command
        .env("BENCHCTL_TEST_OUTCOME_READY", &outcome_ready)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("start lost-response fixture");
    wait_until(Duration::from_secs(15), || outcome_ready.exists());
    kill(Pid::from_raw(child.id() as i32), Signal::SIGKILL)
        .expect("kill coordinator after durable outcome");
    let output = child.wait_with_output().expect("reap killed coordinator");
    assert_eq!(output.status.signal(), Some(Signal::SIGKILL as i32));

    let recovered = rig.recover("lost-response");
    assert!(recovered.status.success(), "{}", stderr(&recovered));
    rig.assert_originals();

    let retry = rig.run("lost-response", "fail", 30);
    assert!(!retry.status.success());
    assert!(stderr(&retry).contains("exit code 7"));
}

#[test]
fn workload_consumes_immutable_accepted_receipt_snapshot() {
    let rig = Rig::new("1");
    let original: serde_json::Value =
        serde_json::from_slice(&fs::read(&rig.receipt).expect("read original receipt"))
            .expect("parse original receipt");
    let output = rig.run("receipt-snapshot", "snapshot", 10);
    assert!(output.status.success(), "{}", stderr(&output));
    let captured: serde_json::Value =
        serde_json::from_slice(&fs::read(&rig.captured_receipt).expect("read captured receipt"))
            .expect("parse captured receipt");
    assert_eq!(captured["build_id"], original["build_id"]);
    assert_eq!(captured["executable_sha256"], original["executable_sha256"]);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &fs::read(&rig.receipt).expect("read replaced source receipt")
        )
        .expect("parse replaced source receipt"),
        serde_json::json!({"forged": true})
    );
    rig.assert_originals();
}

#[test]
fn workload_executes_verified_snapshot_after_source_artifact_swap() {
    let rig = Rig::new("1");
    let ready = rig.receipt.with_file_name("executable-snapshot-ready");
    let release = rig.receipt.with_file_name("executable-snapshot-release");
    let malicious_marker = rig.receipt.with_file_name("malicious-executable-ran");
    let mut command = rig.run_command("executable-snapshot", "check", 30);
    command
        .env("BENCHCTL_TEST_EXECUTABLE_SNAPSHOT_READY", &ready)
        .env("BENCHCTL_TEST_EXECUTABLE_SNAPSHOT_RELEASE", &release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("start executable-snapshot fixture");
    wait_until(Duration::from_secs(15), || ready.exists());
    fs::write(
        &rig.worker,
        format!(
            "#!/bin/sh\nprintf malicious >{}\nexit 88\n",
            malicious_marker.display()
        ),
    )
    .expect("replace caller-owned executable path");
    fs::set_permissions(&rig.worker, fs::Permissions::from_mode(0o755))
        .expect("make replacement executable");
    fs::write(&release, "release\n").expect("release executable-snapshot fixture");
    let output = child
        .wait_with_output()
        .expect("reap executable-snapshot fixture");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!malicious_marker.exists());
    rig.assert_originals();
}

#[test]
fn conflicting_external_value_is_retained_until_recovery() {
    let rig = Rig::new("0");
    let failed = rig.run("conflicting-operation", "conflict", 10);
    assert!(!failed.status.success());
    assert!(stderr(&failed).contains("restore conflict"));
    assert_eq!(read(&rig.poll), "1");
    assert_eq!(read(&rig.c1), "0");
    assert_eq!(read(&rig.deep), "1");

    fs::write(&rig.poll, "0\n").expect("repair external conflict");
    let recovered = rig.recover("conflicting-operation");
    assert!(recovered.status.success(), "{}", stderr(&recovered));
    rig.assert_originals();

    let repeated = rig.recover("conflicting-operation");
    assert!(repeated.status.success(), "{}", stderr(&repeated));
    assert!(stdout(&repeated).contains("AlreadyRestored"));
}

#[test]
fn timeout_drains_descendant_before_restoration() {
    let rig = Rig::new("1");
    let started = Instant::now();
    // Leave enough startup margin for this test to run reliably alongside the
    // other process-heavy lifecycle cases on a saturated CI worker.
    let output = rig.run("timed-operation", "timeout", 3);
    assert!(!output.status.success());
    // Timeout plus the documented TERM grace and forced-drain windows remains
    // bounded even when a saturated CI worker consumes the full cleanup path.
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(stderr(&output).contains("timeout elapsed"));
    rig.assert_originals();

    let pid = fs::read_to_string(&rig.child_pid)
        .expect("fixture published descendant PID")
        .trim()
        .to_owned();
    for _ in 0..100 {
        if !std::path::Path::new("/proc").join(&pid).exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("fixture descendant {pid} survived process-group drain");
}

#[test]
fn guardian_drains_after_coordinator_sigkill_and_recovery_restores() {
    let rig = Rig::new("1");
    let mut command = rig.run_command("killed-coordinator", "timeout", 30);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command.spawn().expect("start coordinator fixture");

    wait_until(Duration::from_secs(15), || {
        rig.child_pid.exists()
            && read(&rig.poll) == "0"
            && read(&rig.c1) == "0"
            && read(&rig.deep) == "1"
    });
    kill(Pid::from_raw(child.id() as i32), Signal::SIGKILL).expect("kill test coordinator");
    let output = child.wait_with_output().expect("reap killed coordinator");
    assert_eq!(output.status.signal(), Some(Signal::SIGKILL as i32));

    let descendant = fs::read_to_string(&rig.child_pid)
        .expect("fixture published descendant PID")
        .trim()
        .to_owned();
    wait_until(Duration::from_secs(12), || {
        !std::path::Path::new("/proc").join(&descendant).exists()
    });

    // A killed coordinator cannot restore host state.  The durable journal and
    // guardian lock keep that restoration explicit and recoverable.
    assert_eq!(read(&rig.poll), "0");
    assert_eq!(read(&rig.c1), "0");
    assert_eq!(read(&rig.deep), "1");
    let blocked = rig.run("different-operation", "check", 10);
    assert!(!blocked.status.success());
    assert!(stderr(&blocked).contains("killed-coordinator"));
    assert_eq!(read(&rig.poll), "0");
    assert_eq!(read(&rig.c1), "0");
    assert_eq!(read(&rig.deep), "1");
    let recovered = rig.recover("killed-coordinator");
    assert!(recovered.status.success(), "{}", stderr(&recovered));
    rig.assert_originals();
}

#[test]
fn client_pidfd_exit_cancels_drains_and_restores_without_stealing_stdin() {
    let rig = Rig::new("1");
    let mut client = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("start client identity fixture");
    let mut command = rig.watched_run_command("client-disconnect", "timeout", 30, client.id());
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command.spawn().expect("start watched coordinator fixture");
    wait_until(Duration::from_secs(15), || rig.child_pid.exists());
    client.kill().expect("kill client identity fixture");
    client.wait().expect("reap client identity fixture");

    let output = child
        .wait_with_output()
        .expect("reap cancelled coordinator");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("cancelled by signal"));
    rig.assert_originals();

    let descendant = fs::read_to_string(&rig.child_pid)
        .expect("fixture published descendant PID")
        .trim()
        .to_owned();
    assert!(
        !std::path::Path::new("/proc").join(&descendant).exists(),
        "fixture descendant {descendant} survived client disconnect"
    );
}

#[test]
fn coordinator_group_signal_does_not_kill_guardian() {
    let rig = Rig::new("1");
    let mut command = rig.run_command("group-signal", "timeout", 30);
    command
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("start isolated coordinator group");
    wait_until(Duration::from_secs(15), || rig.child_pid.exists());
    killpg(Pid::from_raw(child.id() as i32), Signal::SIGTERM)
        .expect("signal coordinator process group");

    let output = child
        .wait_with_output()
        .expect("reap signalled coordinator");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("cancelled by signal"));
    rig.assert_originals();
    let descendant = fs::read_to_string(&rig.child_pid)
        .expect("fixture published descendant PID")
        .trim()
        .to_owned();
    assert!(!std::path::Path::new("/proc").join(descendant).exists());
}

#[test]
fn lock_contention_and_artifact_tampering_fail_before_mutation() {
    let rig = Rig::new("1");
    fs::create_dir_all(&rig.state).expect("create state directory");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(rig.sysfs.join(".benchctl-cpuidle.lock"))
        .expect("open fixture lock");
    fs::set_permissions(
        rig.sysfs.join(".benchctl-cpuidle.lock"),
        fs::Permissions::from_mode(0o600),
    )
    .expect("secure fixture lock");
    lock.lock().expect("lock fixture state");
    let contended = rig.run("contended-operation", "check", 10);
    assert!(!contended.status.success());
    assert!(stderr(&contended).contains("owns the lock"));
    rig.assert_originals();
    lock.unlock().expect("unlock fixture state");

    fs::write(&rig.worker, "#!/bin/sh\nexit 0\n").expect("tamper fixture executable");
    let tampered = rig.run("tampered-operation", "check", 10);
    assert!(!tampered.status.success());
    assert!(stderr(&tampered).contains("digest no longer matches"));
    rig.assert_originals();
}

#[test]
fn duplicate_exact_idle_state_name_fails_before_mutation() {
    let rig = Rig::new("1");
    fs::write(
        rig.deep
            .parent()
            .expect("deep state directory")
            .join("name"),
        "C1\n",
    )
    .expect("duplicate exact C1 state");
    let output = rig.run("duplicate-c1", "check", 10);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("exact POLL and C1"));
    rig.assert_originals();
}

#[test]
fn unprivileged_hidden_supervisor_cannot_mint_production_proof() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let rig = Rig::new("1");
    let go = rig.receipt.with_file_name("forged-production-go");
    let status = rig.receipt.with_file_name("forged-production-status");
    fs::write(&go, "go\n").expect("authorize hidden-supervisor fixture");
    let output = rig
        .command()
        .arg("__workload")
        .arg("--go")
        .arg(&go)
        .arg("--status")
        .arg(&status)
        .arg("--receipt")
        .arg(&rig.receipt)
        .arg("--executable")
        .arg(&rig.worker)
        .arg("--operation-id")
        .arg("forged-production")
        .arg("--coordinator-pid")
        .arg(std::process::id().to_string())
        .arg("--uid")
        .arg(rustix::process::geteuid().as_raw().to_string())
        .arg("--gid")
        .arg(rustix::process::getegid().as_raw().to_string())
        .arg("--production-control")
        .arg("--")
        .arg("check")
        .output()
        .expect("run forged production supervisor");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("requires a root coordinator"));
    assert!(!status.exists());
}

#[test]
fn user_namespace_root_cannot_mint_production_proof() {
    let probe = std::process::Command::new("unshare")
        .args(["--user", "--map-root-user", "true"])
        .output();
    if !probe.is_ok_and(|output| output.status.success()) {
        return;
    }
    let rig = Rig::new("1");
    let go = rig.receipt.with_file_name("userns-production-go");
    let status = rig.receipt.with_file_name("userns-production-status");
    fs::write(&go, "go\n").expect("authorize userns-supervisor fixture");
    let output = std::process::Command::new("unshare")
        .args(["--user", "--map-root-user"])
        .arg(env!("CARGO_BIN_EXE_benchctl"))
        .arg("__workload")
        .arg("--go")
        .arg(&go)
        .arg("--status")
        .arg(&status)
        .arg("--receipt")
        .arg(&rig.receipt)
        .arg("--executable")
        .arg(&rig.worker)
        .arg("--operation-id")
        .arg("userns-production")
        .arg("--coordinator-pid")
        .arg("1")
        .arg("--uid")
        .arg("0")
        .arg("--gid")
        .arg("0")
        .arg("--production-control")
        .arg("--")
        .arg("check")
        .output()
        .expect("run userns production supervisor");
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("host UID 0"),
        "{}",
        stderr(&output)
    );
    assert!(!status.exists());
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let started = Instant::now();
    while !predicate() {
        assert!(started.elapsed() < timeout, "fixture condition timed out");
        thread::sleep(Duration::from_millis(10));
    }
}
