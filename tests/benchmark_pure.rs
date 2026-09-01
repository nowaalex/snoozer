#[path = "../benches/support/pure.rs"]
mod pure;

use std::collections::BTreeSet;
use std::time::Duration;

use pure::{
    GapSchedule, RESULT_SCHEMA_VERSION, correct_latency, json_escape, parse_cpu_list,
    percentile_sorted,
};

const BURSTY_SEED: u64 = 0x5a17_9d3c_e821_4b6f;

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
