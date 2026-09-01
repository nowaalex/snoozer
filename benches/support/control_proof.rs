use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::PathBuf;

const PROOF_VERSION: &str = "benchctl-production-control-v1";

#[allow(dead_code)] // Included directly by the proof-only integration test.
pub(crate) fn validate_from_environment(expected_build_id: &str) -> Result<(), String> {
    let descriptor = std::env::var("BENCHCTL_PRODUCTION_CONTROL_FD")
        .map_err(|_| "official mode requires Benchctl's production-control proof".to_owned())?
        .parse::<u32>()
        .map_err(|_| "Benchctl production-control descriptor is invalid".to_owned())?;
    let operation_id = std::env::var("BENCHCTL_OPERATION_ID")
        .map_err(|_| "official mode requires a Benchctl operation identity".to_owned())?;
    validate_descriptor(descriptor, &operation_id, expected_build_id)
}

pub(crate) fn validate_descriptor(
    descriptor: u32,
    expected_operation_id: &str,
    expected_build_id: &str,
) -> Result<(), String> {
    if descriptor < 3 || descriptor > i32::MAX as u32 {
        return Err("Benchctl production-control descriptor is outside the valid range".to_owned());
    }
    let path = PathBuf::from(format!("/proc/self/fd/{descriptor}"));
    let metadata = fs::metadata(&path)
        .map_err(|error| format!("inspecting Benchctl production-control proof: {error}"))?;
    if metadata.uid() != 0 || !metadata.file_type().is_fifo() {
        return Err("official mode requires a root-owned Benchctl control-proof pipe".to_owned());
    }
    let uid_map = fs::read_to_string("/proc/self/uid_map")
        .map_err(|error| format!("reading benchmark user-namespace mapping: {error}"))?;
    if !maps_namespace_root_to_host_root(&uid_map) {
        return Err(
            "official mode requires a control proof rooted at host UID 0, not user-namespace root"
                .to_owned(),
        );
    }
    let encoded = fs::read_to_string(&path)
        .map_err(|error| format!("reading Benchctl production-control proof: {error}"))?;
    let proof: serde_json::Value = serde_json::from_str(&encoded)
        .map_err(|error| format!("parsing Benchctl production-control proof: {error}"))?;
    let proof = proof
        .as_object()
        .ok_or("Benchctl production-control proof must be an object")?;
    let field = |name: &str| {
        proof
            .get(name)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("Benchctl production-control proof lacks {name}"))
    };
    if field("version")? != PROOF_VERSION {
        return Err("unsupported Benchctl production-control proof version".to_owned());
    }
    if field("operation_id")? != expected_operation_id {
        return Err("Benchctl production-control proof names another operation".to_owned());
    }
    if field("build_id")? != expected_build_id {
        return Err("Benchctl production-control proof names another build receipt".to_owned());
    }
    Ok(())
}

pub(crate) fn maps_namespace_root_to_host_root(mappings: &str) -> bool {
    mappings.lines().any(|line| {
        let mut fields = line.split_whitespace();
        matches!(
            (fields.next(), fields.next(), fields.next(), fields.next()),
            (Some("0"), Some("0"), Some(length), None)
                if length.parse::<u64>().is_ok_and(|length| length > 0)
        )
    })
}
