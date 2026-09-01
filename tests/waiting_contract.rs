use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use snoozer::{
    BusySpin, SpinThenYield, WaitResult, WaitStrategy, WaitTimeoutResult, WaitUntilTimeoutResult,
};

#[test]
fn raw_and_filtered_waits_have_distinct_contracts() {
    let state = Arc::new(AtomicU32::new(0));

    assert_eq!(
        SpinThenYield::new(0).wait_if_equal(&*state, 0),
        WaitResult::Unclassified
    );

    let producer_state = Arc::clone(&state);
    let producer = std::thread::spawn(move || {
        producer_state.store(1, Ordering::Release);
    });
    assert_eq!(SpinThenYield::new(0).wait_until_different(&*state, 0), 1);
    assert!(producer.join().is_ok());
}

#[test]
fn initial_change_wins_over_a_zero_timeout() {
    let state = AtomicU64::new(42);
    assert_eq!(
        BusySpin.wait_if_equal_timeout(&state, 41, Duration::ZERO),
        WaitTimeoutResult::Changed(42)
    );
    assert_eq!(
        BusySpin.wait_until_different_timeout(&state, 42, Duration::ZERO),
        WaitUntilTimeoutResult::TimedOut
    );
}
