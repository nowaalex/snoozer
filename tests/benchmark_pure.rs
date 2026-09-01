#[path = "../benches/support/pure.rs"]
mod pure;

use std::collections::BTreeSet;
use std::time::Duration;

use pure::{
    CPU_SYSFS_ROOT, GapSchedule, RESULT_SCHEMA_VERSION, capture_generation_before_start,
    correct_latency, json_escape, latency_rank_key, parse_cpu_list, percentile_sorted,
    resolve_cpu_sysfs_root,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

const BURSTY_SEED: u64 = 0x5a17_9d3c_e821_4b6f;

#[test]
fn benchmark_schema_is_v2() {
    assert_eq!(RESULT_SCHEMA_VERSION, "snoozer-wake-latency-v2");
}

#[test]
fn waiter_captures_generation_before_announcing_ready() {
    let generation = Arc::new(AtomicU64::new(0));
    let ready = Arc::new(AtomicUsize::new(0));
    let go = Arc::new(AtomicBool::new(false));
    let worker_generation = Arc::clone(&generation);
    let worker_ready = Arc::clone(&ready);
    let worker_go = Arc::clone(&go);
    let worker = std::thread::spawn(move || {
        capture_generation_before_start(&worker_generation, &worker_ready, &worker_go)
    });

    while ready.load(Ordering::Acquire) != 1 {
        std::hint::spin_loop();
    }
    generation.store(1, Ordering::Release);
    go.store(true, Ordering::Release);

    assert_eq!(worker.join().expect("waiter startup thread"), 0);
}

#[test]
fn official_sysfs_root_is_anchored_and_custom_root_is_rejected() {
    assert_eq!(
        resolve_cpu_sysfs_root(None, false).expect("canonical official root"),
        PathBuf::from(CPU_SYSFS_ROOT)
    );
    assert!(resolve_cpu_sysfs_root(Some(PathBuf::from("/fixture")), false).is_err());
}

#[test]
fn smoke_mode_retains_custom_sysfs_root_support() {
    let fixture = PathBuf::from("/fixture");
    assert_eq!(
        resolve_cpu_sysfs_root(Some(fixture.clone()), true).expect("smoke fixture root"),
        fixture
    );
}

#[test]
fn latency_ranking_preserves_cycle_precision_when_nanoseconds_tie() {
    let left_p99_cycles = 100;
    let right_p99_cycles = 101;
    let cycles_per_ns = 4.0;
    let rounded_ns = |cycles| (cycles as f64 / cycles_per_ns).round() as u64;
    assert_eq!(rounded_ns(left_p99_cycles), rounded_ns(right_p99_cycles));

    let left = latency_rank_key(80, left_p99_cycles, 400);
    let right = latency_rank_key(40, right_p99_cycles, 200);
    assert!(left < right);
}

#[test]
fn percentile_uses_nearest_rank() {
    assert_eq!(percentile_sorted(&[1, 2, 3, 4], 0.50), 2);
    assert_eq!(percentile_sorted(&[1, 2, 3, 4], 0.99), 4);
}

#[test]
fn bursty_schedule_is_reproducible_and_bounded() {
    let mut left = GapSchedule::new(BURSTY_SEED);
    let mut right = GapSchedule::new(BURSTY_SEED);
    for _ in 0..10_000 {
        let left_value = left.next();
        assert_eq!(left_value, right.next());
        assert!(left_value <= Duration::from_micros(1_000));
    }
}

#[test]
fn json_strings_escape_controls() {
    assert_eq!(json_escape("a\"b\\c\n"), "a\\\"b\\\\c\\n");
}

#[test]
fn positive_tsc_offset_is_subtracted() {
    assert_eq!(correct_latency(120, 20), Some(100));
    assert_eq!(correct_latency(10, 20), None);
}

#[test]
fn negative_tsc_offset_is_added() {
    assert_eq!(correct_latency(80, -20), Some(100));
}

#[test]
fn parses_linux_cpu_lists() {
    assert_eq!(
        parse_cpu_list("0-2,7,9-10").expect("valid fixture"),
        BTreeSet::from([0, 1, 2, 7, 9, 10])
    );
}

#[test]
fn rejects_descending_cpu_range() {
    assert!(parse_cpu_list("4-2").is_err());
}

#[test]
fn metadata_schema_is_v2() {
    assert_eq!(RESULT_SCHEMA_VERSION, "snoozer-wake-latency-v2");
    assert_ne!(RESULT_SCHEMA_VERSION, "snoozer-wake-latency-v1");
}
