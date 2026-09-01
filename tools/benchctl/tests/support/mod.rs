use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

pub struct Rig {
    _temporary: TempDir,
    pub sysfs: PathBuf,
    pub state: PathBuf,
    pub receipt: PathBuf,
    pub worker: PathBuf,
    pub poll: PathBuf,
    pub c1: PathBuf,
    pub deep: PathBuf,
    pub child_pid: PathBuf,
    pub captured_receipt: PathBuf,
    originals: [String; 3],
}

impl Rig {
    pub fn new(poll_original: &str) -> Self {
        let fixture_base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/benchctl-test-tmp");
        fs::create_dir_all(&fixture_base).expect("create fixture base");
        let fixture_base = fixture_base
            .canonicalize()
            .expect("canonicalize fixture base");
        let temporary = tempfile::tempdir_in(fixture_base).expect("create fixture root");
        let repository = temporary.path().join("repository");
        let sysfs = temporary.path().join("sysfs");
        let state = temporary.path().join("state");
        let receipt = temporary.path().join("receipt.json");
        let child_pid = temporary.path().join("child.pid");
        let captured_receipt = temporary.path().join("captured-receipt.json");
        fs::create_dir_all(&repository).expect("create fixture repository");
        fs::write(
            repository.join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.0.0'\n",
        )
        .expect("write fixture manifest");
        fs::write(
            repository.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
        )
        .expect("write fixture lockfile");
        let worker = repository.join("worker.sh");
        fs::write(
            &worker,
            r#"#!/bin/sh
set -eu
case "$1" in
  check)
    test -z "${BENCHCTL_PRODUCTION_CONTROL_FD:-}"
    test "$(tr -d '[:space:]' <"$BENCHCTL_TEST_POLL")" = 0
    test "$(tr -d '[:space:]' <"$BENCHCTL_TEST_C1")" = 0
    test "$(tr -d '[:space:]' <"$BENCHCTL_TEST_DEEP")" = 1
    ;;
  conflict)
    printf '1\n' >"$BENCHCTL_TEST_POLL"
    ;;
  timeout)
    sleep 30 &
    child=$!
    printf '%s\n' "$child" >"$BENCHCTL_TEST_CHILD_PID"
    wait "$child"
    ;;
  fail)
    exit 7
    ;;
  snapshot)
    printf '{"forged":true}\n' >"$BENCHCTL_TEST_SOURCE_RECEIPT"
    printf '%s\n' "$BENCHCTL_BUILD_RECEIPT_JSON" >"$BENCHCTL_TEST_CAPTURED_RECEIPT"
    ;;
esac
"#,
        )
        .expect("write fixture worker");
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755))
            .expect("make fixture worker executable");
        git(&repository, &["init", "-q"]);
        git(&repository, &["add", "."]);
        git(
            &repository,
            &[
                "-c",
                "user.name=Benchctl Fixture",
                "-c",
                "user.email=benchctl@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        );

        let poll = idle_state(&sysfs, 0, 0, "POLL", poll_original);
        let c1 = idle_state(&sysfs, 0, 1, "C1", "1");
        let deep = idle_state(&sysfs, 0, 2, "C2", "0");
        let commit = git_stdout(&repository, &["rev-parse", "HEAD"]);
        let manifest_path = repository.join("Cargo.toml");
        let lockfile_path = repository.join("Cargo.lock");
        let source_commit = commit.trim().to_owned();
        let lockfile_sha256 = digest_file(&lockfile_path);
        let executable_sha256 = digest_file(&worker);
        let benchctl_executable_sha256 = digest_file(Path::new(env!("CARGO_BIN_EXE_benchctl")));
        let features = Vec::<String>::new();
        let identity = serde_json::to_vec(&(
            (
                "benchctl-build-receipt-v1",
                &manifest_path,
                &repository,
                &source_commit,
                true,
                &lockfile_path,
                &lockfile_sha256,
                "fixture 0.0.0",
                "fixture",
                &features,
            ),
            (
                "bench",
                "fixture",
                "fixture",
                "fixture",
                env!("CARGO_PKG_VERSION"),
                &benchctl_executable_sha256,
                &worker,
                &executable_sha256,
                1_u128,
            ),
        ))
        .expect("serialize fixture build identity");
        let receipt_value = json!({
            "version": "benchctl-build-receipt-v1",
            "build_id": format!("{:x}", Sha256::digest(identity)),
            "manifest_path": manifest_path,
            "repository": repository,
            "source_commit": source_commit,
            "source_clean": true,
            "lockfile_path": lockfile_path,
            "lockfile_sha256": lockfile_sha256,
            "package_id": "fixture 0.0.0",
            "bench": "fixture",
            "features": features,
            "profile": "bench",
            "rustc": "fixture",
            "rustup_toolchain": "fixture",
            "target_triple": "fixture",
            "benchctl_version": env!("CARGO_PKG_VERSION"),
            "benchctl_executable_sha256": benchctl_executable_sha256,
            "executable": worker,
            "executable_sha256": executable_sha256,
            "built_unix_millis": 1,
        });
        fs::write(
            &receipt,
            serde_json::to_vec_pretty(&receipt_value).expect("serialize fixture receipt"),
        )
        .expect("write fixture receipt");
        Self {
            _temporary: temporary,
            sysfs,
            state,
            receipt,
            worker,
            poll,
            c1,
            deep,
            child_pid,
            captured_receipt,
            originals: [poll_original.to_owned(), "1".to_owned(), "0".to_owned()],
        }
    }

    pub fn run(&self, operation_id: &str, mode: &str, timeout: u64) -> Output {
        self.run_command(operation_id, mode, timeout)
            .output()
            .expect("run benchctl fixture")
    }

    pub fn run_command(&self, operation_id: &str, mode: &str, timeout: u64) -> Command {
        self.configure_run_command(operation_id, mode, timeout, None)
    }

    pub fn watched_run_command(
        &self,
        operation_id: &str,
        mode: &str,
        timeout: u64,
        client_pid: u32,
    ) -> Command {
        self.configure_run_command(operation_id, mode, timeout, Some(client_pid))
    }

    fn configure_run_command(
        &self,
        operation_id: &str,
        mode: &str,
        timeout: u64,
        client_pid: Option<u32>,
    ) -> Command {
        let mut command = self.command();
        command
            .args(["run", "--receipt"])
            .arg(&self.receipt)
            .args([
                "--operation-id",
                operation_id,
                "--cpuidle",
                "poll-c1",
                "--cpu",
                "0",
                "--timeout",
                &timeout.to_string(),
                "--sysfs-root",
            ])
            .arg(&self.sysfs)
            .arg("--state-root")
            .arg(&self.state);
        if let Some(client_pid) = client_pid {
            command.arg("--client-pid").arg(client_pid.to_string());
        }
        command.arg("--").arg(mode);
        command
    }

    pub fn recover(&self, operation_id: &str) -> Output {
        self.command()
            .arg("recover")
            .arg(operation_id)
            .arg("--sysfs-root")
            .arg(&self.sysfs)
            .arg("--state-root")
            .arg(&self.state)
            .output()
            .expect("recover benchctl fixture")
    }

    pub fn status(&self, operation_id: &str) -> Output {
        self.command()
            .arg("status")
            .arg(operation_id)
            .arg("--sysfs-root")
            .arg(&self.sysfs)
            .arg("--state-root")
            .arg(&self.state)
            .output()
            .expect("inspect benchctl fixture")
    }

    pub fn assert_originals(&self) {
        assert_eq!(read(&self.poll), self.originals[0]);
        assert_eq!(read(&self.c1), self.originals[1]);
        assert_eq!(read(&self.deep), self.originals[2]);
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_benchctl"));
        command
            .env("BENCHCTL_TEST_POLL", &self.poll)
            .env("BENCHCTL_TEST_C1", &self.c1)
            .env("BENCHCTL_TEST_DEEP", &self.deep)
            .env("BENCHCTL_TEST_CHILD_PID", &self.child_pid);
        command
            .env("BENCHCTL_TEST_SOURCE_RECEIPT", &self.receipt)
            .env("BENCHCTL_TEST_CAPTURED_RECEIPT", &self.captured_receipt);
        command
    }
}

pub fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .expect("read fixture value")
        .trim()
        .to_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn idle_state(sysfs: &Path, cpu: usize, state: usize, name: &str, disable: &str) -> PathBuf {
    let directory = sysfs.join(format!("cpu{cpu}/cpuidle/state{state}"));
    fs::create_dir_all(&directory).expect("create idle state");
    fs::write(directory.join("name"), format!("{name}\n")).expect("write idle name");
    let path = directory.join("disable");
    fs::write(&path, format!("{disable}\n")).expect("write idle value");
    path
}

fn digest_file(path: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(fs::read(path).expect("read digest input"))
    )
}

fn git(repository: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .status()
        .expect("run fixture Git");
    assert!(status.success(), "fixture Git failed with {status}");
}

fn git_stdout(repository: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .output()
        .expect("run fixture Git");
    assert!(output.status.success());
    String::from_utf8(output.stdout).expect("fixture Git output is UTF-8")
}
