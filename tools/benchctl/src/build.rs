use std::collections::{HashSet, VecDeque};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::BufReader;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cargo_metadata::{Message, PackageId, TargetKind};
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use tempfile::NamedTempFile;

use crate::error::BenchError;
use crate::receipt::{BuildProvenance, BuildReceipt, file_digest, store};
use crate::runtime::atomic_write;

const BUILD_AFFECTING_ENVIRONMENT: &[&str] = &[
    "RUSTC",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTC_BOOTSTRAP",
    "CARGO_INCREMENTAL",
    "CARGO_BUILD_TARGET",
    "CARGO_BUILD_RUSTC",
    "CARGO_BUILD_RUSTFLAGS",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
];

pub(crate) struct BuildRequest {
    pub(crate) manifest_path: PathBuf,
    pub(crate) bench: String,
    pub(crate) features: Vec<String>,
    pub(crate) receipt_path: PathBuf,
    pub(crate) timeout: Duration,
}

pub(crate) struct BuildGuardianRequest {
    pub(crate) coordinator_pid: u32,
    pub(crate) ready: PathBuf,
    pub(crate) drain: PathBuf,
    pub(crate) status: PathBuf,
    pub(crate) stdout: PathBuf,
    pub(crate) repository: PathBuf,
    pub(crate) cargo_arguments: Vec<OsString>,
}

#[derive(Debug, Deserialize, Serialize)]
struct BuildStatus {
    code: Option<i32>,
    signal: Option<i32>,
}

impl BuildStatus {
    fn success(&self) -> bool {
        self.code == Some(0)
    }

    fn display(&self) -> String {
        if let Some(code) = self.code {
            format!("exit code {code}")
        } else if let Some(signal) = self.signal {
            format!("signal {signal}")
        } else {
            "unknown exit status".to_owned()
        }
    }
}

pub(crate) fn cargo_bench(request: BuildRequest) -> Result<(), BenchError> {
    if request.bench.is_empty() || request.features.iter().any(String::is_empty) {
        return Err(BenchError::Usage(
            "bench and features must not be empty".to_owned(),
        ));
    }
    reject_environment_overrides()?;
    let manifest_path = request
        .manifest_path
        .canonicalize()
        .map_err(|error| BenchError::io("canonicalizing Cargo manifest", error))?;
    let manifest_directory = manifest_path
        .parent()
        .ok_or_else(|| BenchError::Preflight("Cargo manifest has no parent".to_owned()))?;
    let repository = git_stdout(
        manifest_directory,
        &[OsStr::new("rev-parse"), OsStr::new("--show-toplevel")],
        "finding Git repository",
    )?;
    let repository = PathBuf::from(repository.trim())
        .canonicalize()
        .map_err(|error| BenchError::io("canonicalizing Git repository", error))?;
    if !manifest_path.starts_with(&repository) {
        return Err(BenchError::Preflight(
            "Cargo manifest is outside its Git repository".to_owned(),
        ));
    }
    verify_cargo_configuration(&repository)?;
    require_tracked(&repository, &manifest_path, "Cargo manifest")?;
    let toolchain_channel = pinned_toolchain(&repository)?;
    let rustup_toolchain = validate_toolchain_override(&toolchain_channel)?;
    let rustc_version = command_stdout(
        Command::new("rustc").arg("--version"),
        "reading rustc version",
    )?;
    if !rustc_version.starts_with(&format!("rustc {toolchain_channel} ")) {
        return Err(BenchError::Preflight(format!(
            "rustc does not match pinned toolchain {toolchain_channel}: {}",
            rustc_version.trim()
        )));
    }
    let rustc = command_stdout(Command::new("rustc").arg("-vV"), "reading rustc identity")?;
    let target_triple = rustc
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or_else(|| BenchError::Preflight("rustc -vV did not report a host".to_owned()))?
        .to_owned();
    let cancelled = install_build_signal_handlers()?;
    let cargo_started = Instant::now();
    let metadata_output = NamedTempFile::new()
        .map_err(|error| BenchError::io("creating Cargo metadata log", error))?;
    let mut metadata_arguments = vec![
        OsString::from("metadata"),
        OsString::from("--manifest-path"),
        manifest_path.as_os_str().to_owned(),
        OsString::from("--locked"),
        OsString::from("--filter-platform"),
        OsString::from(&target_triple),
        OsString::from("--format-version"),
        OsString::from("1"),
    ];
    if !request.features.is_empty() {
        metadata_arguments.push(OsString::from("--features"));
        metadata_arguments.push(OsString::from(request.features.join(",")));
    }
    let metadata_status = guarded_cargo(
        &repository,
        metadata_output.path(),
        &metadata_arguments,
        remaining_build_time(cargo_started, request.timeout)?,
        &cancelled,
        &[],
    )?;
    if !metadata_status.success() {
        return Err(BenchError::Preflight(format!(
            "Cargo metadata exited with {}",
            metadata_status.display()
        )));
    }
    let metadata_reader = metadata_output
        .reopen()
        .map_err(|error| BenchError::io("reading Cargo metadata", error))?;
    let metadata: cargo_metadata::Metadata =
        serde_json::from_reader(metadata_reader).map_err(|source| BenchError::Json {
            operation: "reading Cargo metadata",
            source,
        })?;
    let workspace_root = metadata
        .workspace_root
        .as_std_path()
        .canonicalize()
        .map_err(|error| BenchError::io("canonicalizing Cargo workspace root", error))?;
    if !workspace_root.starts_with(&repository) {
        return Err(BenchError::Preflight(
            "Cargo workspace root is outside its Git repository".to_owned(),
        ));
    }
    require_tracked(
        &repository,
        &workspace_root.join("Cargo.toml"),
        "workspace manifest",
    )?;
    let selected = metadata
        .packages
        .iter()
        .filter(|package| {
            package.targets.iter().any(|target| {
                target.name == request.bench && target.kind.contains(&TargetKind::Bench)
            })
        })
        .collect::<Vec<_>>();
    let [selected_package] = selected.as_slice() else {
        return Err(BenchError::Preflight(format!(
            "expected exactly one Cargo package owning bench {}, found {}",
            request.bench,
            selected.len()
        )));
    };
    let selected_manifest = selected_package
        .manifest_path
        .as_std_path()
        .canonicalize()
        .map_err(|error| BenchError::io("canonicalizing selected package manifest", error))?;
    require_tracked(&repository, &selected_manifest, "selected package manifest")?;
    let selected_package_id = selected_package.id.to_string();
    validate_reachable_local_graph(&metadata, &selected_package.id, &repository)?;
    let source_commit = git_stdout(
        &repository,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("HEAD"),
        ],
        "reading source commit",
    )?
    .trim()
    .to_owned();
    let status = git_stdout(
        &repository,
        &[
            OsStr::new("status"),
            OsStr::new("--porcelain"),
            OsStr::new("--untracked-files=all"),
        ],
        "checking working tree",
    )?;
    if !status.is_empty() {
        return Err(BenchError::Preflight(
            "official builds require a clean working tree, including untracked files".to_owned(),
        ));
    }
    let lockfile = workspace_root.join("Cargo.lock");
    require_tracked(&repository, &lockfile, "Cargo lockfile")?;
    let lockfile_sha256 = file_digest(&lockfile, "digesting Cargo.lock")?;

    let cargo_messages = NamedTempFile::new()
        .map_err(|error| BenchError::io("creating Cargo message log", error))?;
    let mut bench_arguments = vec![
        OsString::from("bench"),
        OsString::from("--manifest-path"),
        manifest_path.as_os_str().to_owned(),
        OsString::from("--bench"),
        OsString::from(&request.bench),
        OsString::from("--no-run"),
        OsString::from("--locked"),
        OsString::from("--message-format=json"),
        OsString::from("--target"),
        OsString::from(&target_triple),
    ];
    if !request.features.is_empty() {
        bench_arguments.push(OsString::from("--features"));
        bench_arguments.push(OsString::from(request.features.join(",")));
    }
    let benchmark_environment = [
        ("SNOOZER_BENCHMARK_COMMIT", OsString::from(&source_commit)),
        (
            "SNOOZER_BENCHMARK_REPOSITORY",
            repository.as_os_str().to_owned(),
        ),
        ("SNOOZER_BENCHMARK_DIRTY", OsString::from("false")),
        (
            "SNOOZER_BENCHMARK_RUSTUP_TOOLCHAIN",
            OsString::from(&rustup_toolchain),
        ),
        ("SNOOZER_BENCHMARK_RUSTC", OsString::from(rustc.trim())),
    ];
    let status = guarded_cargo(
        &repository,
        cargo_messages.path(),
        &bench_arguments,
        remaining_build_time(cargo_started, request.timeout)?,
        &cancelled,
        &benchmark_environment,
    )?;
    if !status.success() {
        return Err(BenchError::Workload(format!(
            "Cargo exited with {}",
            status.display()
        )));
    }
    let reader = cargo_messages
        .reopen()
        .map_err(|error| BenchError::io("reading Cargo message log", error))?;
    let mut paths = Vec::new();
    for parsed in Message::parse_stream(BufReader::new(reader)) {
        let message = parsed.map_err(|error| {
            BenchError::Preflight(format!("invalid Cargo JSON message: {error}"))
        })?;
        if let Message::CompilerArtifact(artifact) = message
            && artifact.target.name == request.bench
            && artifact.target.kind.contains(&TargetKind::Bench)
            && let Some(executable) = artifact.executable
        {
            paths.push((
                executable.into_std_path_buf(),
                artifact.package_id.to_string(),
            ));
        }
    }
    let [(executable, artifact_package_id)] = paths.as_slice() else {
        return Err(BenchError::Preflight(format!(
            "expected exactly one executable for bench {}, found {}",
            request.bench,
            paths.len()
        )));
    };
    if artifact_package_id != &selected_package_id {
        return Err(BenchError::Preflight(
            "Cargo artifact package differs from resolved benchmark package".to_owned(),
        ));
    }
    let receipt = BuildReceipt::new(
        BuildProvenance {
            manifest_path,
            repository,
            source_commit,
            lockfile_path: lockfile,
            lockfile_sha256,
            package_id: selected_package_id,
            rustc: rustc.trim().to_owned(),
            rustup_toolchain,
            target_triple,
        },
        request.bench,
        request.features,
        executable.clone(),
    )?;
    verify_checkout_unchanged(
        &receipt.repository,
        &receipt.source_commit,
        &receipt.lockfile_path,
        &receipt.lockfile_sha256,
    )?;
    store(&request.receipt_path, &receipt)?;
    println!("{}", request.receipt_path.display());
    Ok(())
}

fn verify_checkout_unchanged(
    repository: &Path,
    expected_commit: &str,
    lockfile: &Path,
    expected_lockfile_digest: &str,
) -> Result<(), BenchError> {
    let commit = git_stdout(
        repository,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("HEAD"),
        ],
        "rechecking source commit after Cargo build",
    )?;
    if commit.trim() != expected_commit {
        return Err(BenchError::Preflight(
            "source commit changed during Cargo build".to_owned(),
        ));
    }
    let status = git_stdout(
        repository,
        &[
            OsStr::new("status"),
            OsStr::new("--porcelain"),
            OsStr::new("--untracked-files=all"),
        ],
        "rechecking working tree after Cargo build",
    )?;
    if !status.is_empty() {
        return Err(BenchError::Preflight(
            "working tree changed during Cargo build".to_owned(),
        ));
    }
    if file_digest(lockfile, "rechecking Cargo.lock after build")? != expected_lockfile_digest {
        return Err(BenchError::Preflight(
            "Cargo.lock changed during Cargo build".to_owned(),
        ));
    }
    Ok(())
}

fn install_build_signal_handlers() -> Result<Arc<AtomicBool>, BenchError> {
    let cancelled = Arc::new(AtomicBool::new(false));
    for signal in [SIGHUP, SIGINT, SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&cancelled))
            .map_err(|error| BenchError::io("installing build cancellation handler", error))?;
    }
    Ok(cancelled)
}

fn remaining_build_time(started: Instant, timeout: Duration) -> Result<Duration, BenchError> {
    timeout
        .checked_sub(started.elapsed())
        .ok_or_else(|| BenchError::Preflight("Cargo build timeout elapsed".to_owned()))
}

fn guarded_cargo(
    repository: &Path,
    stdout: &Path,
    cargo_arguments: &[OsString],
    timeout: Duration,
    cancelled: &AtomicBool,
    environment: &[(&str, OsString)],
) -> Result<BuildStatus, BenchError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(BenchError::Workload(
            "Cargo build cancelled by signal".to_owned(),
        ));
    }
    let guardian_root = tempfile::tempdir()
        .map_err(|error| BenchError::io("creating build guardian state", error))?;
    let ready = guardian_root.path().join("ready");
    let drain = guardian_root.path().join("drain");
    let status = guardian_root.path().join("status.json");
    let executable = std::env::current_exe()
        .map_err(|error| BenchError::io("resolving Benchctl executable", error))?;
    let mut command = Command::new(executable);
    command
        .arg("__build-guardian")
        .arg("--coordinator-pid")
        .arg(std::process::id().to_string())
        .arg("--ready")
        .arg(&ready)
        .arg("--drain")
        .arg(&drain)
        .arg("--status")
        .arg(&status)
        .arg("--stdout")
        .arg(stdout)
        .arg("--repository")
        .arg(repository)
        .arg("--")
        .args(cargo_arguments)
        .process_group(0);
    for (name, value) in environment {
        command.env(name, value);
    }
    let mut guardian = command
        .spawn()
        .map_err(|error| BenchError::io("starting Cargo crash guardian", error))?;
    wait_for_cargo(&mut guardian, &ready, &drain, &status, timeout, cancelled)
}

fn wait_for_cargo(
    guardian: &mut Child,
    ready: &Path,
    drain: &Path,
    status_path: &Path,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<BuildStatus, BenchError> {
    let started = Instant::now();
    loop {
        if status_path.exists() {
            let bytes = fs::read(status_path)
                .map_err(|error| BenchError::io("reading Cargo status", error))?;
            let status = serde_json::from_slice(&bytes).map_err(|source| BenchError::Json {
                operation: "reading Cargo status",
                source,
            })?;
            require_guardian_success(guardian)?;
            return Ok(status);
        }
        if cancelled.load(Ordering::Relaxed) {
            request_build_drain(drain, guardian)?;
            return Err(BenchError::Workload(
                "Cargo build cancelled by signal".to_owned(),
            ));
        }
        if started.elapsed() >= timeout {
            request_build_drain(drain, guardian)?;
            return Err(BenchError::Preflight(
                "Cargo build timeout elapsed".to_owned(),
            ));
        }
        if let Some(status) = guardian
            .try_wait()
            .map_err(|error| BenchError::io("checking Cargo crash guardian", error))?
        {
            return Err(BenchError::State(format!(
                "Cargo crash guardian exited before recording status: {status}"
            )));
        }
        if !ready.exists() && started.elapsed() >= Duration::from_secs(5) {
            request_build_drain(drain, guardian)?;
            return Err(BenchError::State(
                "Cargo crash guardian startup timed out".to_owned(),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn request_build_drain(drain: &Path, guardian: &mut Child) -> Result<(), BenchError> {
    atomic_write(drain, b"drain\n")?;
    require_guardian_success(guardian)
}

fn require_guardian_success(guardian: &mut Child) -> Result<(), BenchError> {
    let status = guardian
        .wait()
        .map_err(|error| BenchError::io("reaping Cargo crash guardian", error))?;
    if status.success() {
        Ok(())
    } else {
        Err(BenchError::State(format!(
            "Cargo crash guardian exited with {status}"
        )))
    }
}

fn validate_reachable_local_graph(
    metadata: &cargo_metadata::Metadata,
    root: &PackageId,
    repository: &Path,
) -> Result<(), BenchError> {
    let resolve = metadata.resolve.as_ref().ok_or_else(|| {
        BenchError::Preflight("Cargo metadata did not include a resolved graph".to_owned())
    })?;
    let mut pending = VecDeque::from([root.clone()]);
    let mut reachable = HashSet::new();
    while let Some(package_id) = pending.pop_front() {
        if !reachable.insert(package_id.clone()) {
            continue;
        }
        let node = resolve
            .nodes
            .iter()
            .find(|node| node.id == package_id)
            .ok_or_else(|| {
                BenchError::Preflight(format!(
                    "Cargo resolve graph has no node for package {package_id}"
                ))
            })?;
        pending.extend(node.deps.iter().map(|dependency| dependency.pkg.clone()));
    }

    for package_id in reachable {
        let package = metadata
            .packages
            .iter()
            .find(|package| package.id == package_id)
            .ok_or_else(|| {
                BenchError::Preflight(format!(
                    "Cargo metadata has no package for resolved node {package_id}"
                ))
            })?;
        if package.source.is_some() {
            continue;
        }
        let manifest = package
            .manifest_path
            .as_std_path()
            .canonicalize()
            .map_err(|error| BenchError::io("canonicalizing local package manifest", error))?;
        if !manifest.starts_with(repository) {
            return Err(BenchError::Preflight(format!(
                "reachable local package {} is outside the Git repository: {}",
                package.name,
                manifest.display()
            )));
        }
        require_tracked(repository, &manifest, "reachable local package manifest")?;
        for target in &package.targets {
            let source = target
                .src_path
                .as_std_path()
                .canonicalize()
                .map_err(|error| BenchError::io("canonicalizing local Cargo target", error))?;
            if !source.starts_with(repository) {
                return Err(BenchError::Preflight(format!(
                    "Cargo target {} for local package {} is outside the Git repository: {}",
                    target.name,
                    package.name,
                    source.display()
                )));
            }
            require_tracked(repository, &source, "reachable local Cargo target")?;
        }
    }
    Ok(())
}

pub(crate) fn build_guardian(request: BuildGuardianRequest) -> Result<(), BenchError> {
    let coordinator_pid = i32::try_from(request.coordinator_pid)
        .map_err(|_| BenchError::Usage("coordinator PID is outside the PID range".to_owned()))?;
    if !has_parent(coordinator_pid) {
        return Ok(());
    }
    let stdout = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&request.stdout)
        .map_err(|error| BenchError::io("opening Cargo message log", error))?;
    let mut command = Command::new("cargo");
    command
        .current_dir(&request.repository)
        .args(&request.cargo_arguments)
        .stdout(Stdio::from(stdout))
        .process_group(0);
    let mut cargo = command
        .spawn()
        .map_err(|error| BenchError::io("starting Cargo", error))?;
    let pgid = Pid::from_raw(i32::try_from(cargo.id()).map_err(|_| {
        BenchError::State("Cargo PID is outside the process-group range".to_owned())
    })?);
    let result = monitor_cargo(&request, coordinator_pid, pgid, &mut cargo);
    if let Err(error) = result {
        return match terminate_cargo_group(&mut cargo) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(BenchError::State(format!(
                "Cargo guardian failed ({error}); cleanup also failed ({cleanup})"
            ))),
        };
    }
    Ok(())
}

fn monitor_cargo(
    request: &BuildGuardianRequest,
    coordinator_pid: i32,
    pgid: Pid,
    cargo: &mut Child,
) -> Result<(), BenchError> {
    atomic_write(&request.ready, b"ready\n")?;
    loop {
        if request.drain.exists() || !has_parent(coordinator_pid) {
            return terminate_cargo_group(cargo);
        }
        if let Some(status) = cargo
            .try_wait()
            .map_err(|error| BenchError::io("waiting for Cargo", error))?
        {
            drain_cargo_group(pgid)?;
            let record = BuildStatus {
                code: status.code(),
                signal: status.signal(),
            };
            let bytes = serde_json::to_vec(&record).map_err(|source| BenchError::Json {
                operation: "serializing Cargo status",
                source,
            })?;
            atomic_write(&request.status, &bytes)?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn require_tracked(repository: &Path, path: &Path, label: &str) -> Result<(), BenchError> {
    let relative = path
        .strip_prefix(repository)
        .map_err(|_| BenchError::Preflight(format!("{label} is outside its Git repository")))?;
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(relative)
        .output()
        .map_err(|error| BenchError::io("checking tracked build input", error))?;
    if !output.status.success() {
        return Err(BenchError::Preflight(format!(
            "{label} is not tracked by Git: {}",
            path.display()
        )));
    }
    Ok(())
}

fn terminate_cargo_group(child: &mut std::process::Child) -> Result<(), BenchError> {
    let pgid = Pid::from_raw(i32::try_from(child.id()).map_err(|_| {
        BenchError::State("Cargo PID is outside the process-group range".to_owned())
    })?);
    signal_cargo_group(pgid, Signal::SIGTERM)?;
    let grace_started = Instant::now();
    while grace_started.elapsed() < Duration::from_secs(5) {
        child
            .try_wait()
            .map_err(|error| BenchError::io("checking Cargo during drain", error))?;
        if cargo_group_is_empty(pgid)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    signal_cargo_group(pgid, Signal::SIGKILL)?;
    let kill_started = Instant::now();
    while kill_started.elapsed() < Duration::from_secs(5) {
        child
            .try_wait()
            .map_err(|error| BenchError::io("reaping killed Cargo", error))?;
        if cargo_group_is_empty(pgid)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(BenchError::State(
        "Cargo process group could not be proved empty".to_owned(),
    ))
}

fn drain_cargo_group(pgid: Pid) -> Result<(), BenchError> {
    signal_cargo_group(pgid, Signal::SIGTERM)?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if cargo_group_is_empty(pgid)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    signal_cargo_group(pgid, Signal::SIGKILL)?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if cargo_group_is_empty(pgid)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(BenchError::State(
        "Cargo process group could not be proved empty".to_owned(),
    ))
}

fn signal_cargo_group(pgid: Pid, signal: Signal) -> Result<(), BenchError> {
    match killpg(pgid, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(BenchError::State(format!(
            "cannot signal Cargo process group: {error}"
        ))),
    }
}

fn cargo_group_is_empty(pgid: Pid) -> Result<bool, BenchError> {
    match killpg(pgid, None) {
        Err(Errno::ESRCH) => Ok(true),
        Ok(()) => Ok(false),
        Err(error) => Err(BenchError::State(format!(
            "cannot inspect Cargo process group: {error}"
        ))),
    }
}

fn has_parent(expected: i32) -> bool {
    rustix::process::getppid().map(|pid| pid.as_raw_pid()) == Some(expected)
}

fn reject_environment_overrides() -> Result<(), BenchError> {
    for (name, _) in env::vars_os() {
        let name = name.to_string_lossy();
        let fixed = BUILD_AFFECTING_ENVIRONMENT
            .iter()
            .any(|candidate| *candidate == name);
        let dynamic_target = name.starts_with("CARGO_TARGET_")
            && (name.ends_with("_RUSTFLAGS") || name.ends_with("_LINKER"));
        let dynamic_profile =
            name.starts_with("CARGO_PROFILE_BENCH_") || name.starts_with("CARGO_PROFILE_RELEASE_");
        if fixed || dynamic_target || dynamic_profile {
            return Err(BenchError::Preflight(format!(
                "build-affecting environment override is rejected: {name}"
            )));
        }
    }
    Ok(())
}

fn verify_cargo_configuration(repository: &Path) -> Result<(), BenchError> {
    for relative in [".cargo/config", ".cargo/config.toml"] {
        let candidate = repository.join(relative);
        if candidate.exists() {
            let output = Command::new("git")
                .arg("-C")
                .arg(repository)
                .args(["ls-files", "--error-unmatch", relative])
                .output()
                .map_err(|error| BenchError::io("checking repository Cargo config", error))?;
            if !output.status.success() {
                return Err(BenchError::Preflight(format!(
                    "repository Cargo config is not tracked: {}",
                    candidate.display()
                )));
            }
        }
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| BenchError::Preflight("HOME is required".to_owned()))?;
    let default_cargo_home = home.join(".cargo");
    if let Some(configured) = env::var_os("CARGO_HOME")
        && Path::new(&configured) != default_cargo_home
    {
        return Err(BenchError::Preflight(
            "non-default CARGO_HOME is rejected".to_owned(),
        ));
    }
    reject_config_files(&default_cargo_home)?;
    let mut ancestor = repository.parent();
    while let Some(directory) = ancestor {
        reject_config_files(&directory.join(".cargo"))?;
        ancestor = directory.parent();
    }
    Ok(())
}

fn reject_config_files(directory: &Path) -> Result<(), BenchError> {
    for name in ["config", "config.toml"] {
        let candidate = directory.join(name);
        if candidate.exists() {
            return Err(BenchError::Preflight(format!(
                "Cargo config outside the tracked repository is rejected: {}",
                candidate.display()
            )));
        }
    }
    Ok(())
}

fn pinned_toolchain(repository: &Path) -> Result<String, BenchError> {
    let contents = fs::read_to_string(repository.join("rust-toolchain.toml"))
        .map_err(|error| BenchError::io("reading pinned toolchain", error))?;
    let channels = contents
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "channel").then(|| value.trim().trim_matches('"').to_owned())
        })
        .collect::<Vec<_>>();
    let [channel] = channels.as_slice() else {
        return Err(BenchError::Preflight(
            "rust-toolchain.toml must contain exactly one channel".to_owned(),
        ));
    };
    Ok(channel.clone())
}

fn validate_toolchain_override(channel: &str) -> Result<String, BenchError> {
    let Some(value) = env::var_os("RUSTUP_TOOLCHAIN") else {
        return Ok("repository-toolchain-file".to_owned());
    };
    let value = value.to_string_lossy();
    if value != channel && !value.starts_with(&format!("{channel}-")) {
        return Err(BenchError::Preflight(format!(
            "RUSTUP_TOOLCHAIN does not match pinned {channel}"
        )));
    }
    Ok(value.into_owned())
}

fn git_stdout(
    repository: &Path,
    arguments: &[&OsStr],
    operation: &'static str,
) -> Result<String, BenchError> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repository).args(arguments);
    command_stdout(&mut command, operation)
}

fn command_stdout(command: &mut Command, operation: &'static str) -> Result<String, BenchError> {
    let output = command
        .output()
        .map_err(|error| BenchError::io(operation, error))?;
    if !output.status.success() {
        return Err(BenchError::Preflight(format!(
            "{operation} exited with {}",
            output.status
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| BenchError::Preflight(format!("{operation} emitted non-UTF-8 output")))
}
