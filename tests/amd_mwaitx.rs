#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use snoozer::{AmdMwaitx, WaitStrategy, WaitUntilTimeoutResult};

const HARDWARE_TEST_TIMEOUT: Duration = Duration::from_secs(2);

#[test]
fn supported_mwaitx_wakes_for_a_monitored_store() {
    let Ok(strategy) = AmdMwaitx::new() else {
        return;
    };
    let state = Arc::new(AtomicU32::new(0));
    let producer_state = Arc::clone(&state);
    let producer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5));
        producer_state.store(1, Ordering::Release);
    });

    assert_eq!(
        strategy.wait_until_different_timeout(&*state, 0, HARDWARE_TEST_TIMEOUT),
        WaitUntilTimeoutResult::Changed(1)
    );
    assert!(producer.join().is_ok());
}

#[test]
fn supported_mwaitx_hardware_timeout_is_bounded() {
    let Ok(strategy) = AmdMwaitx::new() else {
        return;
    };
    let state = AtomicU32::new(0);

    assert_eq!(
        strategy.wait_until_different_timeout(&state, 0, Duration::from_millis(5)),
        WaitUntilTimeoutResult::TimedOut
    );
}
