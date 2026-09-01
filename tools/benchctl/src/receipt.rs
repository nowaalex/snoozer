use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::BenchError;
use crate::journal::hex_digest;
use crate::runtime::atomic_write;

pub(crate) const RECEIPT_VERSION: &str = "benchctl-build-receipt-v1";

#[derive(Clone, Debug)]
pub(crate) struct BuildProvenance {
    pub(crate) manifest_path: PathBuf,
    pub(crate) repository: PathBuf,
    pub(crate) source_commit: String,
    pub(crate) lockfile_path: PathBuf,
    pub(crate) lockfile_sha256: String,
    pub(crate) package_id: String,
    pub(crate) rustc: String,
    pub(crate) rustup_toolchain: String,
    pub(crate) target_triple: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct BuildReceipt {
    pub(crate) version: String,
    pub(crate) build_id: String,
    pub(crate) manifest_path: PathBuf,
    pub(crate) repository: PathBuf,
    pub(crate) source_commit: String,
    pub(crate) source_clean: bool,
    pub(crate) lockfile_path: PathBuf,
    pub(crate) lockfile_sha256: String,
    pub(crate) package_id: String,
    pub(crate) bench: String,
    pub(crate) features: Vec<String>,
    pub(crate) profile: String,
    pub(crate) rustc: String,
    pub(crate) rustup_toolchain: String,
    pub(crate) target_triple: String,
    pub(crate) benchctl_version: String,
    pub(crate) benchctl_executable_sha256: String,
    pub(crate) executable: PathBuf,
    pub(crate) executable_sha256: String,
    pub(crate) built_unix_millis: u128,
}

impl BuildReceipt {
    pub(crate) fn new(
        provenance: BuildProvenance,
        bench: String,
        mut features: Vec<String>,
        executable: PathBuf,
    ) -> Result<Self, BenchError> {
        features.sort();
        features.dedup();
        let executable_sha256 = file_digest(&executable, "digesting receipt executable")?;
        let benchctl_executable = std::env::current_exe()
            .map_err(|error| BenchError::io("resolving Benchctl executable", error))?;
        let benchctl_executable_sha256 =
            file_digest(&benchctl_executable, "digesting Benchctl executable")?;
        let mut receipt = Self {
            version: RECEIPT_VERSION.to_owned(),
            build_id: String::new(),
            manifest_path: provenance.manifest_path,
            repository: provenance.repository,
            source_commit: provenance.source_commit,
            source_clean: true,
            lockfile_path: provenance.lockfile_path,
            lockfile_sha256: provenance.lockfile_sha256,
            package_id: provenance.package_id,
            bench,
            features,
            profile: "bench".to_owned(),
            rustc: provenance.rustc,
            rustup_toolchain: provenance.rustup_toolchain,
            target_triple: provenance.target_triple,
            benchctl_version: env!("CARGO_PKG_VERSION").to_owned(),
            benchctl_executable_sha256,
            executable,
            executable_sha256,
            built_unix_millis: now_millis(),
        };
        receipt.build_id = receipt.identity_digest()?;
        Ok(receipt)
    }

    pub(crate) fn verify(&self) -> Result<(), BenchError> {
        self.verify_identity()?;
        let actual = file_digest(&self.executable, "digesting receipt executable")?;
        if actual != self.executable_sha256 {
            return Err(BenchError::Preflight(
                "receipt executable digest no longer matches".to_owned(),
            ));
        }
        Ok(())
    }

    fn verify_identity(&self) -> Result<(), BenchError> {
        if self.version != RECEIPT_VERSION {
            return Err(BenchError::Preflight(format!(
                "unsupported build receipt version: {}",
                self.version
            )));
        }
        if !self.source_clean {
            return Err(BenchError::Preflight(
                "official build receipt is not clean".to_owned(),
            ));
        }
        if self.profile != "bench"
            || self.benchctl_version != env!("CARGO_PKG_VERSION")
            || self.bench.is_empty()
            || self.package_id.is_empty()
            || self.source_commit.is_empty()
            || self.lockfile_sha256.is_empty()
            || self.rustc.is_empty()
            || self.rustup_toolchain.is_empty()
            || self.target_triple.is_empty()
            || !self.manifest_path.is_absolute()
            || !self.repository.is_absolute()
            || !self.lockfile_path.is_absolute()
            || !self.lockfile_path.starts_with(&self.repository)
            || !self.executable.is_absolute()
            || self.features.iter().any(String::is_empty)
            || self.features.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(BenchError::Preflight(
                "build receipt has incomplete or invalid identity fields".to_owned(),
            ));
        }
        let benchctl = std::env::current_exe()
            .map_err(|error| BenchError::io("resolving Benchctl executable", error))?;
        let actual_benchctl = file_digest(&benchctl, "digesting Benchctl executable")?;
        if actual_benchctl != self.benchctl_executable_sha256 {
            return Err(BenchError::Preflight(
                "build receipt belongs to a different Benchctl executable".to_owned(),
            ));
        }
        if self.identity_digest()? != self.build_id {
            return Err(BenchError::Preflight(
                "build receipt identity digest does not match its fields".to_owned(),
            ));
        }
        Ok(())
    }

    fn identity_digest(&self) -> Result<String, BenchError> {
        let identity = serde_json::to_vec(&(
            (
                &self.version,
                &self.manifest_path,
                &self.repository,
                &self.source_commit,
                self.source_clean,
                &self.lockfile_path,
                &self.lockfile_sha256,
                &self.package_id,
                &self.bench,
                &self.features,
            ),
            (
                &self.profile,
                &self.rustc,
                &self.rustup_toolchain,
                &self.target_triple,
                &self.benchctl_version,
                &self.benchctl_executable_sha256,
                &self.executable,
                &self.executable_sha256,
                self.built_unix_millis,
            ),
        ))
        .map_err(|source| BenchError::Json {
            operation: "creating build identity",
            source,
        })?;
        Ok(hex_digest(&identity))
    }

    pub(crate) fn verify_checkout_as(&self, uid: u32, gid: u32) -> Result<(), BenchError> {
        let commit = git_stdout(
            &self.repository,
            &["rev-parse", "--verify", "HEAD"],
            uid,
            gid,
        )?;
        if commit.trim() != self.source_commit {
            return Err(BenchError::Preflight(
                "checked-out commit differs from the build receipt".to_owned(),
            ));
        }
        let status = git_stdout(
            &self.repository,
            &["status", "--porcelain", "--untracked-files=all"],
            uid,
            gid,
        )?;
        if !status.is_empty() {
            return Err(BenchError::Preflight(
                "official run requires the clean checkout recorded by the build receipt".to_owned(),
            ));
        }
        let actual_lock = file_digest(&self.lockfile_path, "digesting current Cargo.lock")?;
        if actual_lock != self.lockfile_sha256 {
            return Err(BenchError::Preflight(
                "Cargo.lock differs from the build receipt".to_owned(),
            ));
        }
        Ok(())
    }
}

pub(crate) fn load(path: &Path) -> Result<BuildReceipt, BenchError> {
    let bytes = fs::read(path).map_err(|error| BenchError::io("reading build receipt", error))?;
    let receipt: BuildReceipt =
        serde_json::from_slice(&bytes).map_err(|source| BenchError::Json {
            operation: "reading build receipt",
            source,
        })?;
    receipt.verify()?;
    Ok(receipt)
}

pub(crate) fn load_accepted(path: &Path) -> Result<BuildReceipt, BenchError> {
    let bytes =
        fs::read(path).map_err(|error| BenchError::io("reading accepted build receipt", error))?;
    let receipt: BuildReceipt =
        serde_json::from_slice(&bytes).map_err(|source| BenchError::Json {
            operation: "reading accepted build receipt",
            source,
        })?;
    receipt.verify_identity()?;
    Ok(receipt)
}

pub(crate) fn store(path: &Path, receipt: &BuildReceipt) -> Result<(), BenchError> {
    let bytes = serde_json::to_vec_pretty(receipt).map_err(|source| BenchError::Json {
        operation: "serializing build receipt",
        source,
    })?;
    atomic_write(path, &bytes)
}

pub(crate) fn receipt_digest(path: &Path) -> Result<String, BenchError> {
    file_digest(path, "digesting build receipt")
}

pub(crate) fn file_digest(path: &Path, operation: &'static str) -> Result<String, BenchError> {
    fs::read(path)
        .map(|value| hex_digest(&value))
        .map_err(|error| BenchError::io(operation, error))
}

fn git_stdout(
    repository: &Path,
    arguments: &[&str],
    uid: u32,
    gid: u32,
) -> Result<String, BenchError> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repository).args(arguments);
    if rustix::process::geteuid().is_root() {
        command.gid(gid).uid(uid);
    }
    let output = command
        .output()
        .map_err(|error| BenchError::io("running Git receipt verification", error))?;
    if !output.status.success() {
        return Err(BenchError::Preflight(format!(
            "Git receipt verification exited with {}",
            output.status
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| BenchError::Preflight("Git emitted non-UTF-8 output".to_owned()))
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_millis())
}
