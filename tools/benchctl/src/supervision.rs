use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

use crate::error::BenchError;
use crate::receipt::load_accepted;
use crate::runtime::atomic_write;

const STARTUP_LIMIT: Duration = Duration::from_secs(5);
const STOP_GRACE: Duration = Duration::from_secs(5);
const DRAIN_LIMIT: Duration = Duration::from_secs(5);
const ACTIVE_WAIT: Duration = Duration::from_secs(12);

pub(crate) struct CoordinateRequest<'a> {
    pub(crate) state_root: &'a Path,
    pub(crate) operation_id: &'a str,
    pub(crate) receipt: &'a Path,
    pub(crate) executable: &'a Path,
    pub(crate) workload: &'a [String],
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) timeout: Duration,
    pub(crate) client_watch: Option<ClientWatch>,
    pub(crate) production_control: bool,
}

pub(crate) struct ClientWatch(rustix::fd::OwnedFd);

#[derive(Clone, Debug)]
pub(crate) struct SupervisorRequest {
    pub(crate) go: PathBuf,
    pub(crate) status: PathBuf,
    pub(crate) receipt: PathBuf,
    pub(crate) executable: PathBuf,
    pub(crate) operation_id: String,
    pub(crate) coordinator_pid: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) workload: Vec<String>,
    pub(crate) production_control: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct GuardianRequest {
    pub(crate) coordinator_pid: u32,
    pub(crate) pgid: i32,
    pub(crate) active_lock: PathBuf,
    pub(crate) ready: PathBuf,
    pub(crate) drain: PathBuf,
    pub(crate) drained: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub(crate) enum Outcome {
    Success,
    Failed(String),
    TimedOut,
    Cancelled,
}

#[derive(Debug, Deserialize, Serialize)]
struct WorkloadStatus {
    success: bool,
    code: Option<i32>,
    signal: Option<i32>,
}

#[derive(Serialize)]
struct ProductionControlProof<'a> {
    version: &'static str,
    operation_id: &'a str,
    build_id: &'a str,
}

pub(crate) fn coordinate(request: CoordinateRequest<'_>) -> Result<Outcome, BenchError> {
    let runtime = request
        .state_root
        .join(format!("{}.runtime", request.operation_id));
    fs::create_dir(&runtime)
        .map_err(|error| BenchError::io("creating operation runtime directory", error))?;
    let go = runtime.join("go");
    let status = runtime.join("status.json");
    let ready = runtime.join("guardian.ready");
    let drain = runtime.join("drain");
    let drained = runtime.join("drained");
    let outcome_path = runtime.join("outcome.json");
    let active_lock = request.state_root.join("active.lock");
    let coordinator_pid = std::process::id();

    let executable = std::env::current_exe()
        .map_err(|error| BenchError::io("resolving benchctl executable", error))?;
    let mut supervisor_command = Command::new(&executable);
    supervisor_command
        .arg("__workload")
        .arg("--go")
        .arg(&go)
        .arg("--status")
        .arg(&status)
        .arg("--receipt")
        .arg(request.receipt)
        .arg("--executable")
        .arg(request.executable)
        .arg("--operation-id")
        .arg(request.operation_id)
        .arg("--coordinator-pid")
        .arg(coordinator_pid.to_string())
        .arg("--uid")
        .arg(request.uid.to_string())
        .arg("--gid")
        .arg(request.gid.to_string());
    if request.production_control {
        supervisor_command.arg("--production-control");
    }
    supervisor_command
        .arg("--")
        .args(request.workload)
        .process_group(0);
    let mut supervisor = supervisor_command
        .spawn()
        .map_err(|error| BenchError::io("starting workload supervisor", error))?;
    let pgid = i32::try_from(supervisor.id())
        .map(Pid::from_raw)
        .map_err(|_| BenchError::State("workload PID is outside the PID range".to_owned()))?;

    let guardian_result = Command::new(&executable)
        .arg("__guardian")
        .arg("--coordinator-pid")
        .arg(coordinator_pid.to_string())
        .arg("--pgid")
        .arg(pgid.as_raw().to_string())
        .arg("--active-lock")
        .arg(&active_lock)
        .arg("--ready")
        .arg(&ready)
        .arg("--drain")
        .arg(&drain)
        .arg("--drained")
        .arg(&drained)
        .process_group(0)
        .spawn();
    let mut guardian = match guardian_result {
        Ok(guardian) => guardian,
        Err(error) => {
            terminate_and_reap_group(pgid, &mut supervisor)?;
            return Err(BenchError::io("starting crash guardian", error));
        }
    };

    wait_for_ready(&ready, &mut guardian, &mut supervisor, pgid)?;
    atomic_write(&go, b"go\n")?;

    let cancelled = cancellation_flag(request.client_watch)?;
    let started = Instant::now();
    let outcome = loop {
        if let Some(result) = read_status(&status)? {
            break if result.success {
                Outcome::Success
            } else {
                Outcome::Failed(render_status(&result))
            };
        }
        if cancelled.load(Ordering::Relaxed) {
            break Outcome::Cancelled;
        }
        if started.elapsed() >= request.timeout {
            break Outcome::TimedOut;
        }
        if let Some(status) = guardian
            .try_wait()
            .map_err(|error| BenchError::io("checking crash guardian", error))?
        {
            terminate_and_reap_group(pgid, &mut supervisor)?;
            return Err(BenchError::State(format!(
                "crash guardian exited before drain request: {status}"
            )));
        }
        if let Some(status) = supervisor
            .try_wait()
            .map_err(|error| BenchError::io("checking workload supervisor", error))?
        {
            terminate_and_drain(pgid)?;
            return Err(BenchError::State(format!(
                "workload supervisor exited without a status record: {status}"
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    store_outcome(&outcome_path, &outcome)?;
    if !request.production_control
        && let Some(path) = std::env::var_os("BENCHCTL_TEST_OUTCOME_READY")
    {
        atomic_write(Path::new(&path), b"ready\n")?;
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
    atomic_write(&drain, b"drain\n")?;
    wait_for_drained(&drained, &mut guardian, &mut supervisor)?;
    let guardian_status = guardian
        .wait()
        .map_err(|error| BenchError::io("reaping crash guardian", error))?;
    if !guardian_status.success() {
        return Err(BenchError::State(format!(
            "crash guardian failed during drain: {guardian_status}"
        )));
    }
    let supervisor_status = supervisor
        .wait()
        .map_err(|error| BenchError::io("reaping workload supervisor", error))?;
    if supervisor_status.success() {
        return Err(BenchError::State(
            "workload supervisor escaped guardian drain".to_owned(),
        ));
    }
    Ok(outcome)
}

pub(crate) fn recover_outcome(
    state_root: &Path,
    operation_id: &str,
) -> Result<Option<Outcome>, BenchError> {
    let runtime = state_root.join(format!("{operation_id}.runtime"));
    let outcome = runtime.join("outcome.json");
    if outcome.exists() {
        let bytes = fs::read(outcome)
            .map_err(|error| BenchError::io("reading durable workload outcome", error))?;
        return serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| BenchError::Json {
                operation: "reading durable workload outcome",
                source,
            });
    }
    read_status(&runtime.join("status.json")).map(|status| {
        status.map(|status| {
            if status.success {
                Outcome::Success
            } else {
                Outcome::Failed(render_status(&status))
            }
        })
    })
}

pub(crate) fn cleanup_runtime(state_root: &Path, operation_id: &str) -> Result<(), BenchError> {
    let runtime = state_root.join(format!("{operation_id}.runtime"));
    if runtime.exists() {
        fs::remove_dir_all(runtime)
            .map_err(|error| BenchError::io("removing operation runtime directory", error))?;
    }
    Ok(())
}

pub(crate) fn workload_supervisor(request: SupervisorRequest) -> Result<(), BenchError> {
    let coordinator_pid = i32::try_from(request.coordinator_pid)
        .map_err(|_| BenchError::Usage("coordinator PID is outside the PID range".to_owned()))?;
    let started = Instant::now();
    while !request.go.exists() {
        if started.elapsed() >= STARTUP_LIMIT || !has_parent(coordinator_pid) {
            return Err(BenchError::State(
                "coordinator disappeared before workload authorization".to_owned(),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let receipt = load_accepted(&request.receipt)?;
    let receipt_json = serde_json::to_string(&receipt).map_err(|source| BenchError::Json {
        operation: "serializing accepted build receipt",
        source,
    })?;
    let production_proof = if request.production_control {
        Some(create_production_proof(
            &request.operation_id,
            &receipt.build_id,
        )?)
    } else {
        None
    };
    let executable = open_verified_executable(&request.executable, &receipt.executable_sha256)?;
    let mut command = Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()));
    // std's Unix CommandExt contract clears supplementary groups when uid is
    // set and no explicit group vector is supplied, before applying gid/uid.
    command
        .args(&request.workload)
        .env_remove("BENCHCTL_BUILD_RECEIPT")
        .env_remove("BENCHCTL_PRODUCTION_CONTROL")
        .env_remove("BENCHCTL_PRODUCTION_CONTROL_FD")
        .env("BENCHCTL_BUILD_RECEIPT_JSON", receipt_json)
        .env("BENCHCTL_OPERATION_ID", &request.operation_id)
        .gid(request.gid)
        .uid(request.uid);
    if let Some(reader) = &production_proof {
        command.env(
            "BENCHCTL_PRODUCTION_CONTROL_FD",
            reader.as_raw_fd().to_string(),
        );
    }
    let status = command
        .status()
        .map_err(|error| BenchError::io("starting unprivileged workload", error))?;
    let record = WorkloadStatus {
        success: status.success(),
        code: status.code(),
        signal: status.signal(),
    };
    let bytes = serde_json::to_vec(&record).map_err(|source| BenchError::Json {
        operation: "serializing workload status",
        source,
    })?;
    atomic_write(&request.status, &bytes)?;
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn open_verified_executable(path: &Path, expected_digest: &str) -> Result<File, BenchError> {
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| BenchError::io("opening accepted executable snapshot", error.into()))?;
    let mut file = File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|error| BenchError::io("inspecting accepted executable snapshot", error))?;
    if !metadata.is_file() {
        return Err(BenchError::Preflight(
            "accepted executable snapshot is not a regular file".to_owned(),
        ));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| BenchError::io("digesting accepted executable snapshot", error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    if format!("{:x}", hasher.finalize()) != expected_digest {
        return Err(BenchError::Preflight(
            "accepted executable snapshot digest no longer matches".to_owned(),
        ));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| BenchError::io("rewinding accepted executable snapshot", error))?;
    rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty()).map_err(|error| {
        BenchError::io(
            "making accepted executable snapshot inheritable",
            error.into(),
        )
    })?;
    Ok(file)
}

fn create_production_proof(
    operation_id: &str,
    build_id: &str,
) -> Result<rustix::fd::OwnedFd, BenchError> {
    if !rustix::process::geteuid().is_root() {
        return Err(BenchError::State(
            "production control proof requires a root coordinator".to_owned(),
        ));
    }
    require_host_root_mapping()?;
    let proof = serde_json::to_vec(&ProductionControlProof {
        version: "benchctl-production-control-v1",
        operation_id,
        build_id,
    })
    .map_err(|source| BenchError::Json {
        operation: "serializing production control proof",
        source,
    })?;
    let (reader, writer) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
        .map_err(|error| BenchError::io("creating production control proof pipe", error.into()))?;
    rustix::io::fcntl_setfd(&reader, rustix::io::FdFlags::empty()).map_err(|error| {
        BenchError::io("making production control proof inheritable", error.into())
    })?;
    let mut remaining = proof.as_slice();
    while !remaining.is_empty() {
        let written = rustix::io::write(&writer, remaining)
            .map_err(|error| BenchError::io("writing production control proof", error.into()))?;
        if written == 0 {
            return Err(BenchError::State(
                "production control proof pipe accepted no bytes".to_owned(),
            ));
        }
        remaining = &remaining[written..];
    }
    drop(writer);
    Ok(reader)
}

fn require_host_root_mapping() -> Result<(), BenchError> {
    let mappings = fs::read_to_string("/proc/self/uid_map")
        .map_err(|error| BenchError::io("reading coordinator user-namespace mapping", error))?;
    let maps_namespace_root_to_host_root = mappings.lines().any(|line| {
        let mut fields = line.split_whitespace();
        matches!(
            (fields.next(), fields.next(), fields.next(), fields.next()),
            (Some("0"), Some("0"), Some(length), None)
                if length.parse::<u64>().is_ok_and(|length| length > 0)
        )
    });
    if !maps_namespace_root_to_host_root {
        return Err(BenchError::State(
            "production control proof requires root mapped to host UID 0, not user-namespace root"
                .to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn guardian(request: GuardianRequest) -> Result<(), BenchError> {
    let active = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&request.active_lock)
        .map_err(|error| BenchError::io("opening active-operation lock", error))?;
    active
        .lock()
        .map_err(|error| BenchError::io("locking active operation", error))?;
    atomic_write(&request.ready, b"ready\n")?;
    let coordinator_pid = i32::try_from(request.coordinator_pid)
        .map_err(|_| BenchError::Usage("coordinator PID is outside the PID range".to_owned()))?;
    let pgid = Pid::from_raw(request.pgid);
    loop {
        let requested = request.drain.exists();
        let coordinator_gone = !has_parent(coordinator_pid);
        if requested || coordinator_gone {
            terminate_and_drain(pgid)?;
            if requested {
                atomic_write(&request.drained, b"drained\n")?;
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub(crate) fn require_inactive(state_root: &Path) -> Result<(), BenchError> {
    let path = state_root.join("active.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| BenchError::io("opening active-operation lock", error))?;
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < ACTIVE_WAIT => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(BenchError::State(format!(
                    "active workload cleanup has not released its lock: {error}"
                )));
            }
        }
    }
}

fn wait_for_ready(
    ready: &Path,
    guardian: &mut Child,
    supervisor: &mut Child,
    pgid: Pid,
) -> Result<(), BenchError> {
    let started = Instant::now();
    while !ready.exists() {
        if let Some(status) = guardian
            .try_wait()
            .map_err(|error| BenchError::io("checking guardian startup", error))?
        {
            terminate_and_reap_group(pgid, supervisor)?;
            return Err(BenchError::State(format!(
                "crash guardian failed during startup: {status}"
            )));
        }
        if let Some(status) = supervisor
            .try_wait()
            .map_err(|error| BenchError::io("checking supervisor startup", error))?
        {
            return Err(BenchError::State(format!(
                "workload supervisor failed during startup: {status}"
            )));
        }
        if started.elapsed() >= STARTUP_LIMIT {
            terminate_and_reap_group(pgid, supervisor)?;
            return Err(BenchError::State(
                "crash guardian startup timed out".to_owned(),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn wait_for_drained(
    drained: &Path,
    guardian: &mut Child,
    supervisor: &mut Child,
) -> Result<(), BenchError> {
    let started = Instant::now();
    let limit = STOP_GRACE + DRAIN_LIMIT + Duration::from_secs(1);
    while !drained.exists() {
        // Reap the process-group leader while the guardian is proving the group
        // empty.  A dead but unreaped leader still answers killpg(0), which
        // would otherwise make an orderly drain look stuck.
        supervisor
            .try_wait()
            .map_err(|error| BenchError::io("reaping workload supervisor", error))?;
        if let Some(status) = guardian
            .try_wait()
            .map_err(|error| BenchError::io("checking guardian drain", error))?
        {
            return Err(BenchError::State(format!(
                "crash guardian exited without drain proof: {status}"
            )));
        }
        if started.elapsed() >= limit {
            return Err(BenchError::State(
                "timed out waiting for crash guardian drain proof".to_owned(),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn has_parent(expected: i32) -> bool {
    rustix::process::getppid().map(|pid| pid.as_raw_pid()) == Some(expected)
}

pub(crate) fn client_watch(client_pid: Option<u32>) -> Result<Option<ClientWatch>, BenchError> {
    let Some(client_pid) = client_pid else {
        return Ok(None);
    };
    let raw = i32::try_from(client_pid)
        .map_err(|_| BenchError::Usage("client PID is outside the PID range".to_owned()))?;
    let pid = rustix::process::Pid::from_raw(raw)
        .ok_or_else(|| BenchError::Usage("client PID must be positive".to_owned()))?;
    let fd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).map_err(
        |error| BenchError::State(format!("cannot watch client PID {client_pid}: {error}")),
    )?;
    let mut descriptors = [rustix::event::PollFd::new(
        &fd,
        rustix::event::PollFlags::IN,
    )];
    let ready = rustix::event::poll(&mut descriptors, Some(&rustix::event::Timespec::default()))
        .map_err(|error| BenchError::State(format!("cannot inspect client PID: {error}")))?;
    if ready != 0 {
        return Err(BenchError::Workload(
            "unprivileged client exited before host mutation".to_owned(),
        ));
    }
    Ok(Some(ClientWatch(fd)))
}

fn cancellation_flag(client_watch: Option<ClientWatch>) -> Result<Arc<AtomicBool>, BenchError> {
    let cancelled = Arc::new(AtomicBool::new(false));
    for signal in [SIGHUP, SIGINT, SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&cancelled))
            .map_err(|error| BenchError::io("installing cancellation handler", error))?;
    }
    if let Some(ClientWatch(fd)) = client_watch {
        let client_gone = Arc::clone(&cancelled);
        std::thread::spawn(move || {
            let mut descriptors = [rustix::event::PollFd::new(
                &fd,
                rustix::event::PollFlags::IN,
            )];
            let _result = rustix::event::poll(&mut descriptors, None);
            client_gone.store(true, Ordering::Relaxed);
        });
    }
    Ok(cancelled)
}

fn read_status(path: &Path) -> Result<Option<WorkloadStatus>, BenchError> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path).map_err(|error| BenchError::io("reading workload status", error))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| BenchError::Json {
            operation: "reading workload status",
            source,
        })
}

fn store_outcome(path: &Path, outcome: &Outcome) -> Result<(), BenchError> {
    let bytes = serde_json::to_vec(outcome).map_err(|source| BenchError::Json {
        operation: "serializing durable workload outcome",
        source,
    })?;
    atomic_write(path, &bytes)
}

fn render_status(status: &WorkloadStatus) -> String {
    if let Some(code) = status.code {
        format!("exit code {code}")
    } else if let Some(signal) = status.signal {
        format!("signal {signal}")
    } else {
        "unknown exit status".to_owned()
    }
}

fn terminate_and_drain(pgid: Pid) -> Result<(), BenchError> {
    signal_group(pgid, Signal::SIGTERM)?;
    let started = Instant::now();
    while started.elapsed() < STOP_GRACE {
        if group_is_empty(pgid)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    signal_group(pgid, Signal::SIGKILL)?;
    let started = Instant::now();
    while started.elapsed() < DRAIN_LIMIT {
        if group_is_empty(pgid)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(BenchError::State(
        "workload process group could not be proved empty".to_owned(),
    ))
}

fn terminate_and_reap_group(pgid: Pid, leader: &mut Child) -> Result<(), BenchError> {
    signal_group(pgid, Signal::SIGTERM)?;
    let started = Instant::now();
    while started.elapsed() < STOP_GRACE {
        leader
            .try_wait()
            .map_err(|error| BenchError::io("reaping workload supervisor", error))?;
        if group_is_empty(pgid)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    signal_group(pgid, Signal::SIGKILL)?;
    let started = Instant::now();
    while started.elapsed() < DRAIN_LIMIT {
        leader
            .try_wait()
            .map_err(|error| BenchError::io("reaping workload supervisor", error))?;
        if group_is_empty(pgid)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(BenchError::State(
        "workload process group could not be proved empty".to_owned(),
    ))
}

fn signal_group(pgid: Pid, signal: Signal) -> Result<(), BenchError> {
    match killpg(pgid, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(BenchError::State(format!(
            "cannot signal workload process group: {error}"
        ))),
    }
}

fn group_is_empty(pgid: Pid) -> Result<bool, BenchError> {
    match killpg(pgid, None) {
        Err(Errno::ESRCH) => Ok(true),
        Ok(()) => Ok(false),
        Err(error) => Err(BenchError::State(format!(
            "cannot inspect workload process group: {error}"
        ))),
    }
}
