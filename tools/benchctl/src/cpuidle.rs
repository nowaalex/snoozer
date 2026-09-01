use std::env;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::BenchError;
use crate::journal::{
    Actor, IdleEntry, Journal, Request, Stage, TerminalOutcome, actor, boot_id, journal_path,
    load as load_journal, store as store_journal,
};
use crate::receipt::{file_digest, load as load_receipt, receipt_digest};
use crate::runtime::atomic_write;
use crate::supervision::{self, CoordinateRequest, Outcome};

pub(crate) const REAL_SYSFS_ROOT: &str = "/sys/devices/system/cpu";
pub(crate) const REAL_STATE_ROOT: &str = "/run/benchctl/operations";
const CPUIDLE_LOCK: &str = "/run/lock/benchctl-cpuidle.lock";
const RECOVERY_LOCK_WAIT: Duration = Duration::from_secs(10);

pub(crate) struct RunRequest {
    pub(crate) operation_id: Option<String>,
    pub(crate) receipt_path: PathBuf,
    pub(crate) cpus: Vec<usize>,
    pub(crate) timeout: Duration,
    pub(crate) workload: Vec<String>,
    pub(crate) sysfs_root: PathBuf,
    pub(crate) state_root: PathBuf,
    pub(crate) coordinator: bool,
    pub(crate) client_pid: Option<u32>,
}

pub(crate) struct ControlRequest<'a> {
    pub(crate) operation_id: Option<&'a str>,
    pub(crate) sysfs_root: &'a Path,
    pub(crate) state_root: &'a Path,
    pub(crate) coordinator: bool,
}

pub(crate) fn run(mut request: RunRequest) -> Result<(), BenchError> {
    request.receipt_path = absolute(&request.receipt_path)?;
    request.sysfs_root = absolute_existing(&request.sysfs_root, "canonicalizing sysfs root")?;
    request.state_root = absolute_without_existing(&request.state_root)?;
    validate_production_state_root(&request.sysfs_root, &request.state_root)?;
    if is_real_root(&request.sysfs_root) && !request.coordinator {
        return sudo_run(&request);
    }
    require_coordinator_privilege(&request.sysfs_root, request.coordinator)?;
    if is_real_root(&request.sysfs_root) && request.client_pid.is_none() {
        return Err(BenchError::Preflight(
            "privileged run coordinator requires its unprivileged client PID".to_owned(),
        ));
    }
    coordinate_run(request)
}

pub(crate) fn status(request: ControlRequest<'_>) -> Result<(), BenchError> {
    let sysfs_root = absolute_existing(request.sysfs_root, "canonicalizing sysfs root")?;
    let state_root = absolute_without_existing(request.state_root)?;
    validate_production_state_root(&sysfs_root, &state_root)?;
    if is_real_root(&sysfs_root) && !request.coordinator {
        return sudo_control("status", request.operation_id, &sysfs_root, &state_root);
    }
    require_coordinator_privilege(&sysfs_root, request.coordinator)?;
    let journals = select_journals(&state_root, request.operation_id, false)?;
    if journals.is_empty() {
        println!("no benchctl operations");
        return Ok(());
    }
    for path in journals {
        let journal = load_journal(&path)?;
        println!("{}\t{:?}", journal.operation_id, journal.stage());
    }
    Ok(())
}

pub(crate) fn recover(request: ControlRequest<'_>) -> Result<(), BenchError> {
    let sysfs_root = absolute_existing(request.sysfs_root, "canonicalizing sysfs root")?;
    let state_root = absolute_without_existing(request.state_root)?;
    validate_production_state_root(&sysfs_root, &state_root)?;
    if is_real_root(&sysfs_root) && !request.coordinator {
        return sudo_control("recover", request.operation_id, &sysfs_root, &state_root);
    }
    require_coordinator_privilege(&sysfs_root, request.coordinator)?;
    fs::create_dir_all(&state_root)
        .map_err(|error| BenchError::io("creating operation state directory", error))?;
    secure_state_directory(&state_root)?;
    let _lock = lock_for(&sysfs_root, &state_root, true)?;
    supervision::require_inactive(&state_root)?;
    let paths = select_journals(&state_root, request.operation_id, true)?;
    let [path] = paths.as_slice() else {
        return Err(BenchError::State(if paths.is_empty() {
            "there is no recoverable operation".to_owned()
        } else {
            "more than one operation is recoverable; provide an operation ID".to_owned()
        }));
    };
    let mut journal = load_journal(path)?;
    if journal.stage() == Stage::Restored {
        supervision::cleanup_runtime(&state_root, &journal.operation_id)?;
        println!("{}: AlreadyRestored", journal.operation_id);
        return Ok(());
    }
    if journal.boot_id != boot_id()? {
        return Err(BenchError::State(
            "operation belongs to another Linux boot; automatic recovery is refused".to_owned(),
        ));
    }
    reconcile_runtime_outcome(&state_root, path, &mut journal)?;
    restore(&sysfs_root, path, &mut journal)?;
    supervision::cleanup_runtime(&state_root, &journal.operation_id)?;
    println!("{}: restored", journal.operation_id);
    Ok(())
}

fn coordinate_run(mut request: RunRequest) -> Result<(), BenchError> {
    if request.cpus.is_empty() {
        return Err(BenchError::Usage("at least one CPU is required".to_owned()));
    }
    request.cpus.sort_unstable();
    let old_len = request.cpus.len();
    request.cpus.dedup();
    if old_len != request.cpus.len() {
        return Err(BenchError::Usage("CPU IDs must be distinct".to_owned()));
    }
    fs::create_dir_all(&request.state_root)
        .map_err(|error| BenchError::io("creating operation state directory", error))?;
    secure_state_directory(&request.state_root)?;
    let _lock = lock_for(&request.sysfs_root, &request.state_root, false)?;
    supervision::require_inactive(&request.state_root)?;

    let workload_actor = invoking_actor(&request.sysfs_root)?;
    let receipt = load_receipt(&request.receipt_path)?;
    receipt.verify_checkout_as(workload_actor.uid, workload_actor.gid)?;
    let receipt_hash = receipt_digest(&request.receipt_path)?;
    let operation_id = request
        .operation_id
        .clone()
        .unwrap_or_else(new_operation_id);
    let path = journal_path(&request.state_root, &operation_id)?;
    let accepted_receipt = request.state_root.join(format!("{operation_id}.receipt"));
    let accepted_executable = request
        .state_root
        .join(format!("{operation_id}.executable"));
    let recorded_request = Request {
        receipt: request.receipt_path.clone(),
        accepted_receipt: accepted_receipt.clone(),
        receipt_digest: receipt_hash.clone(),
        accepted_executable: accepted_executable.clone(),
        executable_digest: receipt.executable_sha256.clone(),
        cpus: request.cpus.clone(),
        timeout_seconds: request.timeout.as_secs(),
        workload: request.workload.clone(),
        workload_actor: workload_actor.clone(),
    };
    if path.exists() {
        let existing = load_journal(&path)?;
        if existing.request_hash != recorded_request.digest()? {
            return Err(BenchError::RequestConflict { operation_id });
        }
        if existing.stage() == Stage::Restored {
            return replay_outcome(&existing);
        }
        return Err(BenchError::RecoveryRequired { operation_id });
    }
    require_no_unfinished_journal(&request.state_root)?;
    let client_watch = supervision::client_watch(request.client_pid)?;
    let receipt_bytes = fs::read(&request.receipt_path)
        .map_err(|error| BenchError::io("snapshotting accepted build receipt", error))?;
    atomic_write(&accepted_receipt, &receipt_bytes)?;
    if receipt_digest(&accepted_receipt)? != receipt_hash
        || load_receipt(&accepted_receipt)? != receipt
    {
        return Err(BenchError::Preflight(
            "build receipt changed while it was being accepted".to_owned(),
        ));
    }
    let executable_bytes = fs::read(&receipt.executable)
        .map_err(|error| BenchError::io("snapshotting accepted executable", error))?;
    atomic_write(&accepted_executable, &executable_bytes)?;
    fs::set_permissions(&accepted_executable, fs::Permissions::from_mode(0o555))
        .map_err(|error| BenchError::io("securing accepted executable snapshot", error))?;
    if file_digest(
        &accepted_executable,
        "digesting accepted executable snapshot",
    )? != receipt.executable_sha256
    {
        return Err(BenchError::Preflight(
            "benchmark executable changed while it was being accepted".to_owned(),
        ));
    }
    pause_after_executable_snapshot_for_test(&request.sysfs_root)?;

    let coordinator = actor();
    let mut journal = Journal::new(
        operation_id.clone(),
        recorded_request,
        coordinator.clone(),
        boot_id()?,
    )?;
    journal.entries = inventory(&request.sysfs_root, &request.cpus)?;
    store_journal(&path, &journal)?;
    println!("operation: {operation_id}");

    if let Err(error) = apply(&request.sysfs_root, &journal.entries) {
        let restore_result = restore(&request.sysfs_root, &path, &mut journal);
        return match restore_result {
            Ok(()) => Err(error),
            Err(restore_error) => Err(BenchError::State(format!(
                "apply failed ({error}); cleanup also failed ({restore_error})"
            ))),
        };
    }
    journal.transition(Stage::Applied, coordinator.clone());
    store_journal(&path, &journal)?;

    journal.transition(Stage::WorkloadStarted, coordinator.clone());
    store_journal(&path, &journal)?;
    let workload_status = match supervision::coordinate(CoordinateRequest {
        state_root: &request.state_root,
        operation_id: &operation_id,
        receipt: &accepted_receipt,
        executable: &accepted_executable,
        workload: &request.workload,
        uid: workload_actor.uid,
        gid: workload_actor.gid,
        timeout: request.timeout,
        client_watch,
        production_control: is_real_root(&request.sysfs_root),
    }) {
        Ok(outcome) => outcome,
        Err(error) => {
            reconcile_runtime_outcome(&request.state_root, &path, &mut journal)?;
            journal.transition(Stage::Recoverable, actor());
            store_journal(&path, &journal)?;
            return Err(error);
        }
    };
    journal.outcome = Some(terminal_outcome(&workload_status));
    journal.transition(Stage::WorkloadFinished, coordinator);
    journal.transition(Stage::Draining, actor());
    store_journal(&path, &journal)?;
    restore(&request.sysfs_root, &path, &mut journal)?;
    supervision::cleanup_runtime(&request.state_root, &operation_id)?;
    match workload_status {
        Outcome::Success => Ok(()),
        Outcome::Failed(status) => Err(BenchError::Workload(status)),
        Outcome::TimedOut => Err(BenchError::Workload("timeout elapsed".to_owned())),
        Outcome::Cancelled => Err(BenchError::Workload("cancelled by signal".to_owned())),
    }
}

fn pause_after_executable_snapshot_for_test(sysfs_root: &Path) -> Result<(), BenchError> {
    if is_real_root(sysfs_root) {
        return Ok(());
    }
    let Some(ready) = std::env::var_os("BENCHCTL_TEST_EXECUTABLE_SNAPSHOT_READY") else {
        return Ok(());
    };
    let release =
        std::env::var_os("BENCHCTL_TEST_EXECUTABLE_SNAPSHOT_RELEASE").ok_or_else(|| {
            BenchError::Usage("executable snapshot test seam requires a release path".to_owned())
        })?;
    atomic_write(Path::new(&ready), b"ready\n")?;
    let started = std::time::Instant::now();
    while !Path::new(&release).exists() {
        if started.elapsed() >= Duration::from_secs(15) {
            return Err(BenchError::State(
                "executable snapshot test seam timed out".to_owned(),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn terminal_outcome(outcome: &Outcome) -> TerminalOutcome {
    match outcome {
        Outcome::Success => TerminalOutcome::Success,
        Outcome::Failed(status) => TerminalOutcome::Failed(status.clone()),
        Outcome::TimedOut => TerminalOutcome::TimedOut,
        Outcome::Cancelled => TerminalOutcome::Cancelled,
    }
}

fn reconcile_runtime_outcome(
    state_root: &Path,
    journal_path: &Path,
    journal: &mut Journal,
) -> Result<(), BenchError> {
    if journal.outcome.is_none()
        && let Some(outcome) = supervision::recover_outcome(state_root, &journal.operation_id)?
    {
        journal.outcome = Some(terminal_outcome(&outcome));
        store_journal(journal_path, journal)?;
    }
    Ok(())
}

fn replay_outcome(journal: &Journal) -> Result<(), BenchError> {
    match journal.outcome.as_ref() {
        Some(TerminalOutcome::Success) => {
            println!("{}: completed", journal.operation_id);
            Ok(())
        }
        Some(TerminalOutcome::Failed(status)) => Err(BenchError::Workload(status.clone())),
        Some(TerminalOutcome::TimedOut) => Err(BenchError::Workload("timeout elapsed".to_owned())),
        Some(TerminalOutcome::Cancelled) => {
            Err(BenchError::Workload("cancelled by signal".to_owned()))
        }
        None => Err(BenchError::State(format!(
            "operation {} was restored without a recorded workload outcome",
            journal.operation_id
        ))),
    }
}

fn require_no_unfinished_journal(state_root: &Path) -> Result<(), BenchError> {
    for path in select_journals(state_root, None, false)? {
        let journal = load_journal(&path)?;
        if journal.stage() != Stage::Restored {
            return Err(BenchError::RecoveryRequired {
                operation_id: journal.operation_id,
            });
        }
    }
    Ok(())
}

fn inventory(sysfs_root: &Path, cpus: &[usize]) -> Result<Vec<IdleEntry>, BenchError> {
    let mut entries = Vec::new();
    for &cpu in cpus {
        let cpu_root = sysfs_root.join(format!("cpu{cpu}"));
        reject_symlink(&cpu_root)?;
        let root = cpu_root.join("cpuidle");
        reject_symlink(&root)?;
        let mut poll_count = 0_usize;
        let mut c1_count = 0_usize;
        let mut cpu_entries = Vec::new();
        for item in fs::read_dir(&root)
            .map_err(|error| BenchError::io("reading cpuidle inventory", error))?
        {
            let item = item.map_err(|error| BenchError::io("reading cpuidle entry", error))?;
            let file_name = item.file_name();
            let Some(index) = file_name
                .to_str()
                .and_then(|name| name.strip_prefix("state"))
            else {
                continue;
            };
            let state = index.parse::<usize>().map_err(|_| {
                BenchError::Preflight(format!("invalid cpuidle state directory: {index}"))
            })?;
            let state_root = item.path();
            reject_symlink(&state_root)?;
            let name_path = state_root.join("name");
            let disable_path = state_root.join("disable");
            reject_symlink(&name_path)?;
            reject_symlink(&disable_path)?;
            let name = read_trimmed(&name_path, "reading cpuidle state name")?;
            let original = read_bit(&disable_path)?;
            let desired = if name == "POLL" || name == "C1" {
                if name == "POLL" {
                    poll_count += 1;
                } else {
                    c1_count += 1;
                }
                "0"
            } else {
                "1"
            };
            cpu_entries.push(IdleEntry {
                cpu,
                state,
                name,
                disable_path,
                original,
                desired: desired.to_owned(),
            });
        }
        if cpu_entries.is_empty() || poll_count != 1 || c1_count != 1 {
            return Err(BenchError::Preflight(format!(
                "CPU {cpu} must expose exact POLL and C1 cpuidle states"
            )));
        }
        cpu_entries.sort_by_key(|entry| entry.state);
        entries.extend(cpu_entries);
    }
    Ok(entries)
}

fn apply(sysfs_root: &Path, entries: &[IdleEntry]) -> Result<(), BenchError> {
    validate_inventory(sysfs_root, entries)?;
    for entry in entries {
        let current = read_bit(&entry.disable_path)?;
        if current != entry.original {
            return Err(BenchError::State(format!(
                "cpuidle value changed before apply: {}",
                entry.disable_path.display()
            )));
        }
        write_bit(&entry.disable_path, &entry.desired)?;
    }
    Ok(())
}

fn restore(sysfs_root: &Path, path: &Path, journal: &mut Journal) -> Result<(), BenchError> {
    validate_inventory(sysfs_root, &journal.entries)?;
    for entry in &journal.entries {
        let current = read_bit(&entry.disable_path)?;
        if current != entry.original && current != entry.desired {
            let conflict_path = entry.disable_path.clone();
            let expected = format!("{} or {}", entry.original, entry.desired);
            journal.transition(Stage::RestoreConflict, actor());
            store_journal(path, journal)?;
            return Err(BenchError::RestoreConflict {
                path: conflict_path,
                expected,
                actual: current,
            });
        }
    }
    journal.transition(Stage::Restoring, actor());
    store_journal(path, journal)?;
    for entry in &journal.entries {
        let current = read_bit(&entry.disable_path)?;
        if current != entry.original && current != entry.desired {
            let conflict_path = entry.disable_path.clone();
            let expected = format!("{} or {}", entry.original, entry.desired);
            journal.transition(Stage::RestoreConflict, actor());
            store_journal(path, journal)?;
            return Err(BenchError::RestoreConflict {
                path: conflict_path,
                expected,
                actual: current,
            });
        }
        if current == entry.desired && current != entry.original {
            write_bit(&entry.disable_path, &entry.original)?;
        }
    }
    validate_inventory(sysfs_root, &journal.entries)?;
    for entry in &journal.entries {
        let actual = read_bit(&entry.disable_path)?;
        if actual != entry.original {
            let conflict_path = entry.disable_path.clone();
            let expected = entry.original.clone();
            journal.transition(Stage::RestoreConflict, actor());
            store_journal(path, journal)?;
            return Err(BenchError::RestoreConflict {
                path: conflict_path,
                expected,
                actual,
            });
        }
    }
    journal.transition(Stage::Restored, actor());
    store_journal(path, journal)
}

fn validate_inventory(sysfs_root: &Path, recorded: &[IdleEntry]) -> Result<(), BenchError> {
    let mut cpus = recorded.iter().map(|entry| entry.cpu).collect::<Vec<_>>();
    cpus.sort_unstable();
    cpus.dedup();
    let current = inventory(sysfs_root, &cpus)?;
    if current.len() != recorded.len()
        || current.iter().zip(recorded).any(|(left, right)| {
            left.cpu != right.cpu
                || left.state != right.state
                || left.name != right.name
                || left.disable_path != right.disable_path
        })
    {
        return Err(BenchError::State(
            "current cpuidle inventory differs from the journal".to_owned(),
        ));
    }
    Ok(())
}

fn write_bit(path: &Path, value: &str) -> Result<(), BenchError> {
    fs::write(path, format!("{value}\n"))
        .map_err(|error| BenchError::io("writing cpuidle value", error))?;
    let actual = read_bit(path)?;
    if actual != value {
        return Err(BenchError::State(format!(
            "cpuidle readback failed for {}: expected {value}, found {actual}",
            path.display()
        )));
    }
    Ok(())
}

fn read_bit(path: &Path) -> Result<String, BenchError> {
    let value = read_trimmed(path, "reading cpuidle value")?;
    if value != "0" && value != "1" {
        return Err(BenchError::State(format!(
            "cpuidle value is not 0 or 1: {}",
            path.display()
        )));
    }
    Ok(value)
}

fn read_trimmed(path: &Path, operation: &'static str) -> Result<String, BenchError> {
    fs::read_to_string(path)
        .map(|value| value.trim().to_owned())
        .map_err(|error| BenchError::io(operation, error))
}

fn reject_symlink(path: &Path) -> Result<(), BenchError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| BenchError::io("inspecting controlled path", error))?;
    if metadata.file_type().is_symlink() {
        return Err(BenchError::Preflight(format!(
            "symbolic links are rejected: {}",
            path.display()
        )));
    }
    Ok(())
}

fn secure_state_directory(path: &Path) -> Result<(), BenchError> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| BenchError::io("securing operation state directory", error))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| BenchError::io("inspecting operation state directory", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(BenchError::Preflight(
            "operation state root must be a real directory".to_owned(),
        ));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o777 != 0o700 {
        return Err(BenchError::Preflight(
            "operation state root must be owned by the coordinator with mode 0700".to_owned(),
        ));
    }
    Ok(())
}

fn lock_for(sysfs_root: &Path, _state_root: &Path, wait: bool) -> Result<File, BenchError> {
    let (path, expected_uid, expected_mode) = if is_real_root(sysfs_root) {
        (PathBuf::from(CPUIDLE_LOCK), 0, 0o666)
    } else {
        (
            sysfs_root.join(".benchctl-cpuidle.lock"),
            rustix::process::geteuid().as_raw(),
            0o600,
        )
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| BenchError::io("creating lock directory", error))?;
    }
    let existed = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(BenchError::Preflight(format!(
                    "symbolic links are rejected: {}",
                    path.display()
                )));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(BenchError::io("inspecting cpuidle lock", error)),
    };
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(&path)
        .map_err(|error| BenchError::io("opening cpuidle lock", error))?;
    if !existed {
        file.set_permissions(fs::Permissions::from_mode(expected_mode))
            .map_err(|error| BenchError::io("securing cpuidle lock", error))?;
    }
    let metadata = file
        .metadata()
        .map_err(|error| BenchError::io("inspecting cpuidle lock", error))?;
    if !metadata.is_file()
        || metadata.uid() != expected_uid
        || metadata.mode() & 0o777 != expected_mode
    {
        return Err(BenchError::Preflight(
            "global cpuidle lock has unsafe ownership or mode".to_owned(),
        ));
    }
    acquire_lock(&file, wait, "another cpuidle operation owns the lock")?;
    Ok(file)
}

fn acquire_lock(file: &File, wait: bool, label: &str) -> Result<(), BenchError> {
    let started = std::time::Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock)
                if wait && started.elapsed() < RECOVERY_LOCK_WAIT =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(BenchError::State(format!("{label}: {error}"))),
        }
    }
}

fn select_journals(
    state_root: &Path,
    operation_id: Option<&str>,
    recoverable_only: bool,
) -> Result<Vec<PathBuf>, BenchError> {
    if let Some(operation_id) = operation_id {
        let path = journal_path(state_root, operation_id)?;
        if path.exists() {
            return Ok(vec![path]);
        }
        return Ok(Vec::new());
    }
    if !state_root.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(state_root)
        .map_err(|error| BenchError::io("listing operation state", error))?
    {
        let path = entry
            .map_err(|error| BenchError::io("listing operation entry", error))?
            .path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        if recoverable_only && load_journal(&path)?.stage() == Stage::Restored {
            continue;
        }
        paths.push(path);
    }
    paths.sort();
    Ok(paths)
}

fn invoking_actor(sysfs_root: &Path) -> Result<Actor, BenchError> {
    if !is_real_root(sysfs_root) || !rustix::process::geteuid().is_root() {
        return Ok(actor());
    }
    let uid = parse_sudo_id("SUDO_UID")?;
    let gid = parse_sudo_id("SUDO_GID")?;
    Ok(Actor { uid, gid })
}

fn parse_sudo_id(name: &'static str) -> Result<u32, BenchError> {
    env::var(name)
        .map_err(|_| BenchError::Preflight(format!("{name} is required from sudo")))?
        .parse::<u32>()
        .map_err(|_| BenchError::Preflight(format!("{name} is malformed")))
}

fn require_coordinator_privilege(sysfs_root: &Path, coordinator: bool) -> Result<(), BenchError> {
    if is_real_root(sysfs_root) && (!coordinator || !rustix::process::geteuid().is_root()) {
        return Err(BenchError::Preflight(
            "real sysfs operations require the hidden privileged coordinator".to_owned(),
        ));
    }
    Ok(())
}

fn validate_production_state_root(sysfs_root: &Path, state_root: &Path) -> Result<(), BenchError> {
    if is_real_root(sysfs_root) && state_root != Path::new(REAL_STATE_ROOT) {
        return Err(BenchError::Preflight(
            "real sysfs operations require the fixed root-owned Benchctl state directory"
                .to_owned(),
        ));
    }
    Ok(())
}

fn sudo_run(request: &RunRequest) -> Result<(), BenchError> {
    let executable = env::current_exe()
        .map_err(|error| BenchError::io("resolving benchctl executable", error))?;
    let mut command = Command::new("sudo");
    command
        .arg("--")
        .arg(executable)
        .args(["run", "--coordinator", "--client-pid"])
        .arg(std::process::id().to_string())
        .arg("--receipt");
    command.arg(&request.receipt_path);
    command.args(["--cpuidle", "poll-c1", "--timeout"]);
    command.arg(request.timeout.as_secs().to_string());
    command.args(["--sysfs-root"]).arg(&request.sysfs_root);
    command.args(["--state-root"]).arg(&request.state_root);
    if let Some(operation_id) = &request.operation_id {
        command.args(["--operation-id", operation_id]);
    }
    for cpu in &request.cpus {
        command.args(["--cpu", &cpu.to_string()]);
    }
    command.arg("--").args(&request.workload);
    forward_status(&mut command, "privileged coordinator")
}

fn sudo_control(
    action: &str,
    operation_id: Option<&str>,
    sysfs_root: &Path,
    state_root: &Path,
) -> Result<(), BenchError> {
    let executable = env::current_exe()
        .map_err(|error| BenchError::io("resolving benchctl executable", error))?;
    let mut command = Command::new("sudo");
    command
        .arg("--")
        .arg(executable)
        .arg(action)
        .arg("--coordinator")
        .arg("--sysfs-root")
        .arg(sysfs_root)
        .arg("--state-root")
        .arg(state_root);
    if let Some(operation_id) = operation_id {
        command.arg(operation_id);
    }
    forward_status(&mut command, "privileged coordinator")
}

fn forward_status(command: &mut Command, label: &str) -> Result<(), BenchError> {
    let status = command
        .status()
        .map_err(|error| BenchError::io("starting privileged coordinator", error))?;
    if status.success() {
        Ok(())
    } else {
        Err(BenchError::Workload(format!(
            "{label} exited with {status}"
        )))
    }
}

fn absolute(path: &Path) -> Result<PathBuf, BenchError> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        env::current_dir()
            .map(|directory| directory.join(path))
            .map_err(|error| BenchError::io("resolving current directory", error))
    }
}

fn absolute_existing(path: &Path, operation: &'static str) -> Result<PathBuf, BenchError> {
    absolute(path)?
        .canonicalize()
        .map_err(|error| BenchError::io(operation, error))
}

fn absolute_without_existing(path: &Path) -> Result<PathBuf, BenchError> {
    let absolute = absolute(path)?;
    if absolute == Path::new("/") || absolute.components().any(|part| part.as_os_str() == "..") {
        return Err(BenchError::Preflight(
            "state root must be an absolute non-root path without traversal".to_owned(),
        ));
    }
    Ok(absolute)
}

fn is_real_root(path: &Path) -> bool {
    path == Path::new(REAL_SYSFS_ROOT)
}

fn new_operation_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_millis());
    format!("{millis}-{}", std::process::id())
}
