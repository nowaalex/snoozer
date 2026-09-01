#[allow(dead_code)]
mod support;

use std::fs::{self, OpenOptions};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::thread;
use std::time::Duration;

use support::{Rig, read, stderr, stdout};

#[test]
fn legacy_global_record_is_recovered_with_exact_readback_then_removed() {
    let rig = Rig::new("1");
    let legacy = LegacyFixture::new(&rig, false);
    let status = legacy.status(&rig);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(stdout(&status).contains("legacy-snoozer\tLegacyRecoverable"));

    let recovered = legacy.recover(&rig);
    assert!(recovered.status.success(), "{}", stderr(&recovered));
    assert!(stdout(&recovered).contains("legacy-snoozer: restored"));
    assert!(
        !legacy.marker.exists(),
        "legacy marker must be removed last"
    );
    assert!(
        !legacy.dirty.exists(),
        "private dirty marker must be cleaned first"
    );
    assert!(
        !legacy.manifest.exists(),
        "manifest must be cleaned before the global marker"
    );
    legacy.assert_originals();

    // The legacy marker is the authoritative recovery record. Once its removal
    // was durably published, repeat recovery has no state to guess about.
    let repeated = legacy.recover(&rig);
    assert!(!repeated.status.success());
    assert!(stderr(&repeated).contains("no recoverable operation"));
}

#[test]
fn malformed_legacy_marker_is_retained_without_a_write() {
    let rig = Rig::new("1");
    let legacy = LegacyFixture::new(&rig, false);
    fs::write(
        &legacy.marker,
        format!(
            "version=SNOOZER_GLOBAL_DIRTY_V999\nsysfs_root={}\nstate_root={}\nmanifest={}\nuid={}\nboot_id={}\n",
            rig.sysfs.display(),
            legacy.state.display(),
            legacy.manifest.display(),
            current_uid(),
            boot_id(),
        ),
    )
    .expect("replace marker with unsupported version");
    secure_file(&legacy.marker);
    let before = legacy.values();

    let recovered = legacy.recover(&rig);
    assert!(!recovered.status.success());
    assert!(stderr(&recovered).contains("unsupported legacy global dirty-owner version"));
    assert!(legacy.marker.exists());
    assert!(legacy.dirty.exists());
    assert!(legacy.manifest.exists());
    assert_eq!(
        legacy.values(),
        before,
        "malformed evidence must not be written"
    );
}

#[test]
fn legacy_restore_conflict_retains_marker_and_external_value() {
    let rig = Rig::new("1");
    let legacy = LegacyFixture::new(&rig, true);
    let conflict = legacy.entries[0].0.clone();
    fs::write(&conflict, "1\n").expect("simulate external write");

    let recovered = legacy.recover(&rig);
    assert!(!recovered.status.success());
    assert!(stderr(&recovered).contains("restore conflict"));
    assert!(legacy.marker.exists());
    assert!(legacy.dirty.exists());
    assert!(legacy.manifest.exists());
    assert_eq!(read(&conflict), "1");
}

#[test]
fn custom_root_lock_symlink_is_rejected_before_any_recovery_write() {
    let rig = Rig::new("1");
    let legacy = LegacyFixture::new(&rig, false);
    let target = rig.sysfs.join("missing-lock-target");
    symlink(&target, rig.sysfs.join(".snoozer-cpuidle.lock")).expect("plant lock symlink");
    let before = legacy.values();

    let recovered = legacy.recover(&rig);
    assert!(!recovered.status.success());
    assert!(stderr(&recovered).contains("symbolic links are rejected"));
    assert_eq!(legacy.values(), before);
    assert!(legacy.marker.exists());
}

#[test]
fn legacy_active_run_lock_blocks_restore_until_its_guardian_releases_it() {
    let rig = Rig::new("1");
    let legacy = LegacyFixture::new(&rig, false);
    let active = rig.state.join("active-run.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&active)
        .expect("create active-run lock");
    secure_file(&active);
    lock.lock().expect("hold active-run lock");
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        drop(lock);
    });
    let recovered = legacy.recover(&rig);
    release.join().expect("release legacy active lock");
    assert!(recovered.status.success(), "{}", stderr(&recovered));
    legacy.assert_originals();
    assert!(!legacy.marker.exists());
}

struct LegacyFixture {
    state: PathBuf,
    manifest: PathBuf,
    dirty: PathBuf,
    marker: PathBuf,
    entries: Vec<(PathBuf, String, String)>,
}

impl LegacyFixture {
    fn new(rig: &Rig, conflict_first_entry: bool) -> Self {
        fs::create_dir_all(&rig.state).expect("create legacy private state");
        fs::set_permissions(&rig.state, fs::Permissions::from_mode(0o700))
            .expect("secure legacy private state");

        let mut entries = Vec::new();
        for cpu in 0..4 {
            entries.extend(cpu_states(rig, cpu));
        }
        if conflict_first_entry {
            entries[0].1 = "0".to_owned();
            entries[0].2 = "0".to_owned();
        }
        for (path, _original, desired) in &entries {
            fs::write(path, format!("{desired}\n")).expect("apply legacy desired value");
        }

        let manifest = rig.state.join("manifest.legacy");
        let mut contents = format!(
            "version=SNOOZER_CPUIDLE_V2\nsysfs_root={}\npid=999999\nuid={}\nstarted_epoch=0\ncpus=0,1,2,3\n",
            rig.sysfs.display(),
            current_uid(),
        );
        for (path, original, desired) in &entries {
            let (cpu, state, name) = state_identity(path);
            contents.push_str(&format!(
                "state|{}|{original}|{desired}|{name}|{cpu}|{state}\n",
                path.display()
            ));
        }
        fs::write(&manifest, contents).expect("write legacy manifest");
        secure_file(&manifest);
        let dirty = rig.state.join("dirty");
        fs::write(&dirty, format!("{}\n", manifest.display())).expect("write legacy dirty marker");
        secure_file(&dirty);

        let marker = rig.sysfs.join(".snoozer-cpuidle.dirty");
        fs::write(
            &marker,
            format!(
                "version=SNOOZER_GLOBAL_DIRTY_V1\nsysfs_root={}\nstate_root={}\nmanifest={}\nuid={}\nboot_id={}\n",
                rig.sysfs.display(),
                rig.state.display(),
                manifest.display(),
                current_uid(),
                boot_id(),
            ),
        )
        .expect("write legacy global marker");
        secure_file(&marker);
        Self {
            state: rig.state.clone(),
            manifest,
            dirty,
            marker,
            entries,
        }
    }

    fn recover(&self, rig: &Rig) -> Output {
        rig.command()
            .arg("recover")
            .arg("--sysfs-root")
            .arg(&rig.sysfs)
            .arg("--state-root")
            .arg(&rig.state)
            .output()
            .expect("recover legacy fixture")
    }

    fn status(&self, rig: &Rig) -> Output {
        rig.command()
            .arg("status")
            .arg("--sysfs-root")
            .arg(&rig.sysfs)
            .arg("--state-root")
            .arg(&rig.state)
            .output()
            .expect("inspect legacy fixture")
    }

    fn assert_originals(&self) {
        for (path, original, _) in &self.entries {
            assert_eq!(read(path), *original, "{}", path.display());
        }
    }

    fn values(&self) -> Vec<String> {
        self.entries.iter().map(|(path, _, _)| read(path)).collect()
    }
}

fn cpu_states(rig: &Rig, cpu: usize) -> Vec<(PathBuf, String, String)> {
    if cpu == 0 {
        return vec![
            (rig.poll.clone(), "1".to_owned(), "0".to_owned()),
            (rig.c1.clone(), "1".to_owned(), "0".to_owned()),
            (rig.deep.clone(), "0".to_owned(), "1".to_owned()),
        ];
    }
    [
        (0, "POLL", "1", "0"),
        (1, "C1", "1", "0"),
        (2, "C2", "0", "1"),
    ]
    .into_iter()
    .map(|(state, name, original, desired)| {
        let directory = rig.sysfs.join(format!("cpu{cpu}/cpuidle/state{state}"));
        fs::create_dir_all(&directory).expect("create legacy fake cpuidle state");
        fs::write(directory.join("name"), format!("{name}\n")).expect("write legacy state name");
        let disable = directory.join("disable");
        fs::write(&disable, format!("{original}\n")).expect("write legacy state value");
        (disable, original.to_owned(), desired.to_owned())
    })
    .collect()
}

fn state_identity(path: &Path) -> (usize, usize, String) {
    let state_directory = path.parent().expect("state directory");
    let state = state_directory
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("state"))
        .expect("state index")
        .parse()
        .expect("numeric state index");
    let cpu = state_directory
        .parent()
        .and_then(Path::parent)
        .and_then(|cpu| cpu.file_name())
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("cpu"))
        .expect("cpu index")
        .parse()
        .expect("numeric CPU index");
    let name = read(&state_directory.join("name"));
    (cpu, state, name)
}

fn secure_file(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("secure legacy file");
}

fn current_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .expect("read boot identity")
        .trim()
        .to_owned()
}
