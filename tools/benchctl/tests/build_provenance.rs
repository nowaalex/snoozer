use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;

#[test]
fn cargo_build_writes_receipt_and_rejects_dirty_retry() {
    let fixture_base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/benchctl-test-tmp");
    fs::create_dir_all(&fixture_base).expect("create build fixture base");
    let temporary = tempfile::Builder::new()
        .prefix("benchctl-build-")
        .tempdir_in(fixture_base)
        .expect("create build fixture");
    let repository = temporary.path().join("repository");
    let home = temporary.path().join("home");
    let receipt = temporary.path().join("receipt.json");
    fs::create_dir_all(repository.join("benches")).expect("create fixture bench directory");
    fs::create_dir_all(&home).expect("create fixture home");
    fs::write(
        repository.join("Cargo.toml"),
        "[workspace]\n\n[package]\nname = \"fixture-build\"\nversion = \"0.0.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n\n[[bench]]\nname = \"sample\"\nharness = false\n",
    )
    .expect("write fixture manifest");
    fs::write(repository.join("benches/sample.rs"), "fn main() {}\n")
        .expect("write fixture benchmark");
    fs::write(
        repository.join("build.rs"),
        r#"fn main() {
    println!("cargo:rerun-if-env-changed=BENCHCTL_TEST_MUTATION_READY");
    if let Ok(ready) = std::env::var("BENCHCTL_TEST_MUTATION_READY") {
        std::fs::write(ready, "ready\n").expect("publish build-script readiness");
        let release = std::env::var("BENCHCTL_TEST_MUTATION_RELEASE")
            .expect("mutation release path");
        while !std::path::Path::new(&release).exists() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
"#,
    )
    .expect("write fixture build script");
    fs::write(
        repository.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"fixture-build\"\nversion = \"0.0.0\"\n",
    )
    .expect("write fixture lockfile");
    fs::write(
        repository.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.98.0\"\nprofile = \"minimal\"\n",
    )
    .expect("write fixture toolchain");
    fs::write(repository.join(".gitignore"), "/target\n").expect("write fixture ignore");
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

    let first = build_command(&repository, &home, &receipt)
        .output()
        .expect("run fixture build");
    assert!(first.status.success(), "{}", stderr(&first));
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(&receipt).expect("read generated build receipt"))
            .expect("decode generated build receipt");
    assert_eq!(value["version"], "benchctl-build-receipt-v1");
    assert_eq!(value["bench"], "sample");
    assert!(
        value["package_id"]
            .as_str()
            .is_some_and(|package| package.contains("fixture-build"))
    );
    assert_eq!(
        value["benchctl_executable_sha256"].as_str().map(str::len),
        Some(64)
    );
    assert_eq!(value["source_clean"], true);
    let executable = value["executable"]
        .as_str()
        .expect("receipt executable path");
    assert!(Path::new(executable).is_file());

    fs::write(repository.join("dirty-input"), "dirty\n").expect("dirty fixture checkout");
    let dirty = build_command(&repository, &home, &temporary.path().join("dirty.json"))
        .output()
        .expect("run dirty fixture build");
    assert!(!dirty.status.success());
    assert!(stderr(&dirty).contains("clean working tree"));

    fs::remove_file(repository.join("dirty-input")).expect("clean fixture checkout");
    let ignored = repository.join("target/ignored");
    fs::create_dir_all(&ignored).expect("create ignored manifest directory");
    fs::write(
        ignored.join("Cargo.toml"),
        "[package]\nname='ignored'\nversion='0.0.0'\nedition='2024'\n\n[[bench]]\nname='sample'\nharness=false\n",
    )
    .expect("write ignored manifest");
    let ignored_output = build_command_for_manifest(
        &ignored.join("Cargo.toml"),
        &home,
        &temporary.path().join("ignored.json"),
    )
    .output()
    .expect("run ignored-manifest build");
    assert!(!ignored_output.status.success());
    assert!(stderr(&ignored_output).contains("not tracked by Git"));

    let ignored_dependency = repository.join("target/ignored-dependency");
    fs::create_dir_all(ignored_dependency.join("src")).expect("create ignored path dependency");
    fs::write(
        ignored_dependency.join("Cargo.toml"),
        "[package]\nname='ignored-dependency'\nversion='0.0.0'\nedition='2024'\n",
    )
    .expect("write ignored dependency manifest");
    fs::write(ignored_dependency.join("src/lib.rs"), "pub fn value() {}\n")
        .expect("write ignored dependency target");
    fs::write(
        repository.join("Cargo.toml"),
        "[workspace]\n\n[package]\nname = \"fixture-build\"\nversion = \"0.0.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n\n[dependencies]\nignored-dependency = { path = \"target/ignored-dependency\" }\n\n[[bench]]\nname = \"sample\"\nharness = false\n",
    )
    .expect("reference ignored path dependency");
    fs::write(
        repository.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"fixture-build\"\nversion = \"0.0.0\"\ndependencies = [\n \"ignored-dependency\",\n]\n\n[[package]]\nname = \"ignored-dependency\"\nversion = \"0.0.0\"\n",
    )
    .expect("lock ignored path dependency");
    git(&repository, &["add", "Cargo.toml", "Cargo.lock"]);
    git(
        &repository,
        &[
            "-c",
            "user.name=Benchctl Fixture",
            "-c",
            "user.email=benchctl@example.invalid",
            "commit",
            "-qm",
            "reference ignored dependency",
        ],
    );
    let ignored_dependency_output = build_command(
        &repository,
        &home,
        &temporary.path().join("ignored-dependency.json"),
    )
    .output()
    .expect("run ignored-dependency build");
    assert!(!ignored_dependency_output.status.success());
    assert!(stderr(&ignored_dependency_output).contains("not tracked by Git"));

    fs::write(
        repository.join("Cargo.toml"),
        "[workspace]\n\n[package]\nname = \"fixture-build\"\nversion = \"0.0.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n\n[[bench]]\nname = \"sample\"\nharness = false\n",
    )
    .expect("remove ignored path dependency");
    fs::write(
        repository.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"fixture-build\"\nversion = \"0.0.0\"\n",
    )
    .expect("restore fixture lockfile");
    git(&repository, &["add", "Cargo.toml", "Cargo.lock"]);
    git(
        &repository,
        &[
            "-c",
            "user.name=Benchctl Fixture",
            "-c",
            "user.email=benchctl@example.invalid",
            "commit",
            "-qm",
            "remove ignored dependency",
        ],
    );

    let mutation_ready = temporary.path().join("mutation-ready");
    let mutation_release = temporary.path().join("mutation-release");
    let mutation_receipt = temporary.path().join("mutation.json");
    let mut mutation = build_command(&repository, &home, &mutation_receipt);
    mutation
        .args(["--timeout", "30"])
        .env("BENCHCTL_TEST_MUTATION_READY", &mutation_ready)
        .env("BENCHCTL_TEST_MUTATION_RELEASE", &mutation_release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = mutation.spawn().expect("start mutation-detection build");
    wait_file(&mutation_ready);
    fs::write(
        repository.join("benches/sample.rs"),
        "fn main() { println!(\"mutated\"); }\n",
    )
    .expect("mutate tracked source during Cargo build");
    fs::write(&mutation_release, "release\n").expect("release mutation build script");
    let output = child.wait_with_output().expect("reap mutation build");
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("working tree changed during Cargo build"),
        "{}",
        stderr(&output)
    );
    assert!(!mutation_receipt.exists());
    fs::write(repository.join("benches/sample.rs"), "fn main() {}\n")
        .expect("restore tracked fixture source");

    let fake_bin = temporary.path().join("fake-bin");
    let cargo_child = temporary.path().join("cargo-child.pid");
    let real_cargo = find_in_path("cargo");
    fs::create_dir(&fake_bin).expect("create fake binary directory");
    let fake_cargo = fake_bin.join("cargo");
    fs::write(
        &fake_cargo,
        "#!/bin/sh\nset -eu\nif [ \"${BENCHCTL_TEST_HANG_PHASE:-bench}\" != \"$1\" ]; then\n  exec \"$BENCHCTL_TEST_REAL_CARGO\" \"$@\"\nfi\nsleep 30 &\nchild=$!\nprintf '%s\\n' \"$child\" >\"$BENCHCTL_TEST_CARGO_CHILD\"\nwait \"$child\"\n",
    )
    .expect("write fake Cargo");
    fs::set_permissions(&fake_cargo, fs::Permissions::from_mode(0o755))
        .expect("make fake Cargo executable");
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        env::var("PATH").expect("test PATH")
    );
    let timed_receipt = temporary.path().join("timed.json");
    let started = Instant::now();
    let timed = build_command(&repository, &home, &timed_receipt)
        .args(["--timeout", "3"])
        .env("PATH", path)
        .env("BENCHCTL_TEST_REAL_CARGO", &real_cargo)
        .env("BENCHCTL_TEST_CARGO_CHILD", &cargo_child)
        .output()
        .expect("run timed fixture build");
    assert!(!timed.status.success());
    assert!(started.elapsed() < Duration::from_secs(8));
    assert!(
        stderr(&timed).contains("build timeout elapsed"),
        "{}",
        stderr(&timed)
    );
    assert!(!timed_receipt.exists());
    let pid = fs::read_to_string(&cargo_child)
        .expect("fake Cargo published child PID")
        .trim()
        .to_owned();
    wait_process_gone(&pid, "timeout");

    let signal_child = temporary.path().join("signal-cargo-child.pid");
    let mut signalled = build_command(&repository, &home, &temporary.path().join("signalled.json"));
    signalled
        .args(["--timeout", "30"])
        .env(
            "PATH",
            format!("{}:{}", fake_bin.display(), env::var("PATH").unwrap()),
        )
        .env("BENCHCTL_TEST_REAL_CARGO", &real_cargo)
        .env("BENCHCTL_TEST_CARGO_CHILD", &signal_child)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = signalled.spawn().expect("start cancellable build");
    wait_file(&signal_child);
    thread::sleep(Duration::from_millis(50));
    killpg(Pid::from_raw(child.id() as i32), Signal::SIGTERM)
        .expect("signal build coordinator group");
    let output = child.wait_with_output().expect("reap cancelled build");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("cancelled by signal"));
    let pid = fs::read_to_string(&signal_child)
        .expect("fake Cargo published signal child PID")
        .trim()
        .to_owned();
    wait_process_gone(&pid, "handled signal");

    let killed_child = temporary.path().join("killed-cargo-child.pid");
    let mut killed = build_command(&repository, &home, &temporary.path().join("killed.json"));
    killed
        .args(["--timeout", "30"])
        .env(
            "PATH",
            format!("{}:{}", fake_bin.display(), env::var("PATH").unwrap()),
        )
        .env("BENCHCTL_TEST_REAL_CARGO", &real_cargo)
        .env("BENCHCTL_TEST_CARGO_CHILD", &killed_child)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = killed.spawn().expect("start crash-guarded build");
    wait_file(&killed_child);
    kill(Pid::from_raw(child.id() as i32), Signal::SIGKILL).expect("kill build coordinator");
    let output = child
        .wait_with_output()
        .expect("reap killed build coordinator");
    assert_eq!(output.status.signal(), Some(Signal::SIGKILL as i32));
    let pid = fs::read_to_string(&killed_child)
        .expect("fake Cargo published killed child PID")
        .trim()
        .to_owned();
    wait_process_gone(&pid, "coordinator SIGKILL");

    let metadata_signal_child = temporary.path().join("metadata-signal-child.pid");
    let mut metadata_signalled = build_command(
        &repository,
        &home,
        &temporary.path().join("metadata-signalled.json"),
    );
    metadata_signalled
        .args(["--timeout", "30"])
        .env(
            "PATH",
            format!("{}:{}", fake_bin.display(), env::var("PATH").unwrap()),
        )
        .env("BENCHCTL_TEST_REAL_CARGO", &real_cargo)
        .env("BENCHCTL_TEST_HANG_PHASE", "metadata")
        .env("BENCHCTL_TEST_CARGO_CHILD", &metadata_signal_child)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = metadata_signalled
        .spawn()
        .expect("start cancellable Cargo metadata");
    wait_file(&metadata_signal_child);
    killpg(Pid::from_raw(child.id() as i32), Signal::SIGTERM)
        .expect("signal metadata coordinator group");
    let output = child
        .wait_with_output()
        .expect("reap cancelled Cargo metadata");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("cancelled by signal"));
    let pid = fs::read_to_string(&metadata_signal_child)
        .expect("fake metadata published signal child PID")
        .trim()
        .to_owned();
    wait_process_gone(&pid, "metadata handled signal");

    let metadata_killed_child = temporary.path().join("metadata-killed-child.pid");
    let mut metadata_killed = build_command(
        &repository,
        &home,
        &temporary.path().join("metadata-killed.json"),
    );
    metadata_killed
        .args(["--timeout", "30"])
        .env(
            "PATH",
            format!("{}:{}", fake_bin.display(), env::var("PATH").unwrap()),
        )
        .env("BENCHCTL_TEST_REAL_CARGO", &real_cargo)
        .env("BENCHCTL_TEST_HANG_PHASE", "metadata")
        .env("BENCHCTL_TEST_CARGO_CHILD", &metadata_killed_child)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = metadata_killed
        .spawn()
        .expect("start crash-guarded Cargo metadata");
    wait_file(&metadata_killed_child);
    kill(Pid::from_raw(child.id() as i32), Signal::SIGKILL)
        .expect("kill Cargo metadata coordinator");
    let output = child
        .wait_with_output()
        .expect("reap killed Cargo metadata coordinator");
    assert_eq!(output.status.signal(), Some(Signal::SIGKILL as i32));
    let pid = fs::read_to_string(&metadata_killed_child)
        .expect("fake metadata published killed child PID")
        .trim()
        .to_owned();
    wait_process_gone(&pid, "metadata coordinator SIGKILL");
}

fn build_command(repository: &Path, home: &Path, receipt: &Path) -> Command {
    build_command_for_manifest(&repository.join("Cargo.toml"), home, receipt)
}

fn build_command_for_manifest(manifest: &Path, home: &Path, receipt: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_benchctl"));
    command
        .args(["build", "cargo-bench", "--manifest-path"])
        .arg(manifest)
        .args(["--bench", "sample", "--receipt"])
        .arg(receipt)
        .env("HOME", home)
        .env_remove("CARGO_HOME");
    if env::var_os("RUSTUP_HOME").is_none()
        && let Some(original_home) = env::var_os("HOME")
    {
        command.env("RUSTUP_HOME", Path::new(&original_home).join(".rustup"));
    }
    for name in [
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
    ] {
        command.env_remove(name);
    }
    command
}

fn wait_file(path: &Path) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "file wait timed out"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_process_gone(pid: &str, reason: &str) {
    for _ in 0..100 {
        if !Path::new("/proc").join(pid).exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("fake Cargo descendant {pid} survived {reason}");
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

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn find_in_path(name: &str) -> std::path::PathBuf {
    env::split_paths(&env::var_os("PATH").expect("test PATH"))
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        .expect("find executable in PATH")
}
