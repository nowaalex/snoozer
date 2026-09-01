use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::build::{self, BuildGuardianRequest, BuildRequest};
use crate::cpuidle::{self, ControlRequest, RunRequest};
use crate::error::BenchError;
use crate::supervision::{self, GuardianRequest, SupervisorRequest};

#[derive(Parser)]
#[command(name = "benchctl", about = "Fail-closed benchmark control utility")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Build(Build),
    Run(Run),
    Status(Control),
    Recover(Control),
    #[command(name = "__workload", hide = true)]
    Workload(Workload),
    #[command(name = "__guardian", hide = true)]
    Guardian(Guardian),
    #[command(name = "__build-guardian", hide = true)]
    BuildGuardian(BuildGuardian),
}

#[derive(Args)]
struct Build {
    #[command(subcommand)]
    command: BuildCommand,
}

#[derive(Subcommand)]
enum BuildCommand {
    CargoBench(CargoBench),
}

#[derive(Args)]
struct CargoBench {
    #[arg(long)]
    manifest_path: PathBuf,
    #[arg(long)]
    bench: String,
    #[arg(long = "feature")]
    features: Vec<String>,
    #[arg(long)]
    receipt: PathBuf,
    #[arg(long, default_value_t = 300)]
    timeout: u64,
}

#[derive(Args)]
struct Run {
    #[arg(long)]
    receipt: PathBuf,
    #[arg(long)]
    operation_id: Option<String>,
    #[arg(long)]
    cpuidle: CpuIdlePolicy,
    #[arg(long = "cpu", required = true)]
    cpus: Vec<usize>,
    #[arg(long, default_value_t = 900)]
    timeout: u64,
    #[arg(long, hide = true, default_value = cpuidle::REAL_SYSFS_ROOT)]
    sysfs_root: PathBuf,
    #[arg(long, hide = true, default_value = cpuidle::REAL_STATE_ROOT)]
    state_root: PathBuf,
    #[arg(long, hide = true)]
    coordinator: bool,
    #[arg(long, hide = true)]
    client_pid: Option<u32>,
    #[arg(last = true, required = true)]
    workload: Vec<String>,
}

#[derive(Args)]
struct Control {
    operation_id: Option<String>,
    #[arg(long, hide = true, default_value = cpuidle::REAL_SYSFS_ROOT)]
    sysfs_root: PathBuf,
    #[arg(long, hide = true, default_value = cpuidle::REAL_STATE_ROOT)]
    state_root: PathBuf,
    #[arg(long, hide = true)]
    coordinator: bool,
}

#[derive(Args)]
struct Workload {
    #[arg(long)]
    go: PathBuf,
    #[arg(long)]
    status: PathBuf,
    #[arg(long)]
    receipt: PathBuf,
    #[arg(long)]
    executable: PathBuf,
    #[arg(long)]
    operation_id: String,
    #[arg(long)]
    coordinator_pid: u32,
    #[arg(long)]
    uid: u32,
    #[arg(long)]
    gid: u32,
    #[arg(long)]
    production_control: bool,
    #[arg(last = true)]
    workload: Vec<String>,
}

#[derive(Args)]
struct Guardian {
    #[arg(long)]
    coordinator_pid: u32,
    #[arg(long)]
    pgid: i32,
    #[arg(long)]
    active_lock: PathBuf,
    #[arg(long)]
    ready: PathBuf,
    #[arg(long)]
    drain: PathBuf,
    #[arg(long)]
    drained: PathBuf,
}

#[derive(Args)]
struct BuildGuardian {
    #[arg(long)]
    coordinator_pid: u32,
    #[arg(long)]
    ready: PathBuf,
    #[arg(long)]
    drain: PathBuf,
    #[arg(long)]
    status: PathBuf,
    #[arg(long)]
    stdout: PathBuf,
    #[arg(long)]
    repository: PathBuf,
    #[arg(last = true, required = true)]
    cargo_arguments: Vec<OsString>,
}

#[derive(Clone, Copy, ValueEnum)]
enum CpuIdlePolicy {
    PollC1,
}

pub(crate) fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<(), BenchError> {
    let cli =
        Cli::try_parse_from(arguments).map_err(|error| BenchError::Usage(error.to_string()))?;
    match cli.command {
        Command::Build(Build {
            command: BuildCommand::CargoBench(value),
        }) => build::cargo_bench(BuildRequest {
            manifest_path: value.manifest_path,
            bench: value.bench,
            features: value.features,
            receipt_path: value.receipt,
            timeout: Duration::from_secs(value.timeout),
        }),
        Command::Run(value) => {
            let CpuIdlePolicy::PollC1 = value.cpuidle;
            cpuidle::run(RunRequest {
                operation_id: value.operation_id,
                receipt_path: value.receipt,
                cpus: value.cpus,
                timeout: Duration::from_secs(value.timeout),
                workload: value.workload,
                sysfs_root: value.sysfs_root,
                state_root: value.state_root,
                coordinator: value.coordinator,
                client_pid: value.client_pid,
            })
        }
        Command::Status(value) => cpuidle::status(ControlRequest {
            operation_id: value.operation_id.as_deref(),
            sysfs_root: &value.sysfs_root,
            state_root: &value.state_root,
            coordinator: value.coordinator,
        }),
        Command::Recover(value) => cpuidle::recover(ControlRequest {
            operation_id: value.operation_id.as_deref(),
            sysfs_root: &value.sysfs_root,
            state_root: &value.state_root,
            coordinator: value.coordinator,
        }),
        Command::Workload(value) => supervision::workload_supervisor(SupervisorRequest {
            go: value.go,
            status: value.status,
            receipt: value.receipt,
            executable: value.executable,
            operation_id: value.operation_id,
            coordinator_pid: value.coordinator_pid,
            uid: value.uid,
            gid: value.gid,
            workload: value.workload,
            production_control: value.production_control,
        }),
        Command::Guardian(value) => supervision::guardian(GuardianRequest {
            coordinator_pid: value.coordinator_pid,
            pgid: value.pgid,
            active_lock: value.active_lock,
            ready: value.ready,
            drain: value.drain,
            drained: value.drained,
        }),
        Command::BuildGuardian(value) => build::build_guardian(BuildGuardianRequest {
            coordinator_pid: value.coordinator_pid,
            ready: value.ready,
            drain: value.drain,
            status: value.status,
            stdout: value.stdout,
            repository: value.repository,
            cargo_arguments: value.cargo_arguments,
        }),
    }
}
