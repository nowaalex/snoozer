#[path = "../benches/support/output.rs"]
mod output;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use output::{AtomicOutput, partial_output_path};

static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "snoozer-benchmark-output-test-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("create unique fixture directory");
        Self { root }
    }

    fn output(&self) -> PathBuf {
        self.root.join("results.jsonl")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("remove benchmark output fixture");
    }
}

#[test]
fn partial_output_is_unique_and_in_the_final_directory() {
    let final_path = Path::new("target/results.jsonl");
    let left = partial_output_path(final_path, 7, 11).expect("valid partial path");
    let right = partial_output_path(final_path, 7, 12).expect("valid partial path");
    assert_eq!(left.parent(), final_path.parent());
    assert_eq!(right.parent(), final_path.parent());
    assert_ne!(left, right);
}

#[test]
fn dropped_output_preserves_existing_final_file() {
    let fixture = Fixture::new();
    let final_path = fixture.output();
    fs::write(&final_path, b"previous\n").expect("write previous result");
    {
        let mut output = AtomicOutput::create(&final_path).expect("create partial output");
        output
            .write_all(b"incomplete\n")
            .expect("write partial output");
    }
    assert_eq!(
        fs::read(&final_path).expect("read final result"),
        b"previous\n"
    );
    assert_eq!(
        fs::read_dir(&fixture.root).expect("list fixture").count(),
        1
    );
}

#[test]
fn finish_atomically_replaces_the_final_file() {
    let fixture = Fixture::new();
    let final_path = fixture.output();
    fs::write(&final_path, b"previous\n").expect("write previous result");
    let mut output = AtomicOutput::create(&final_path).expect("create partial output");
    output
        .write_all(b"complete\n")
        .expect("write complete output");
    output.finish().expect("publish complete output");

    assert_eq!(
        fs::read(&final_path).expect("read final result"),
        b"complete\n"
    );
    assert_eq!(
        fs::read_dir(&fixture.root).expect("list fixture").count(),
        1
    );
}
