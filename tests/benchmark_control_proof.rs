#[path = "../benches/support/control_proof.rs"]
mod control_proof;

use std::fs;
use std::os::fd::AsRawFd;
use std::process::Command;

#[test]
fn host_root_mapping_parser_is_fail_closed() {
    assert!(control_proof::maps_namespace_root_to_host_root(
        "         0          0 4294967295\n"
    ));
    assert!(!control_proof::maps_namespace_root_to_host_root(
        "         0       1000          1\n"
    ));
    assert!(!control_proof::maps_namespace_root_to_host_root(
        "malformed\n"
    ));
}

#[test]
fn forged_user_namespace_pipe_is_rejected_by_the_consumer() {
    let uid_map = fs::read_to_string("/proc/self/uid_map").expect("read test uid_map");
    if !rustix::process::geteuid().is_root()
        || control_proof::maps_namespace_root_to_host_root(&uid_map)
    {
        let probe = Command::new("unshare")
            .args(["--user", "--map-root-user", "true"])
            .output();
        if !probe.is_ok_and(|output| output.status.success()) {
            return;
        }
        let output = Command::new("unshare")
            .args(["--user", "--map-root-user"])
            .arg(std::env::current_exe().expect("resolve test executable"))
            .args([
                "--exact",
                "forged_user_namespace_pipe_is_rejected_by_the_consumer",
                "--nocapture",
            ])
            .output()
            .expect("run proof consumer in a user namespace");
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let (reader, writer) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
        .expect("create forged userns proof pipe");
    let proof = br#"{"version":"benchctl-production-control-v1","operation_id":"forged-operation","build_id":"forged-build"}"#;
    assert_eq!(
        rustix::io::write(&writer, proof).expect("write forged proof"),
        proof.len()
    );
    drop(writer);
    let error = control_proof::validate_descriptor(
        reader.as_raw_fd() as u32,
        "forged-operation",
        "forged-build",
    )
    .expect_err("user-namespace pipe must not prove host-root coordination");
    assert!(error.contains("host UID 0"), "{error}");
}
