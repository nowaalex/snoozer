use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use super::*;

#[test]
fn deadlines_report_a_bounded_remaining_interval_and_expire() {
    let timeout = Duration::from_secs(60);
    let deadline = Deadline::new(timeout);
    let remaining = deadline
        .remaining()
        .expect("fresh deadline must remain live");
    assert!(remaining <= timeout);
    assert!(!remaining.is_zero());

    let expired = Deadline {
        started: Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("one second must be representable"),
        timeout: Duration::from_millis(1),
    };
    assert_eq!(expired.remaining(), None);
}

#[test]
fn public_strategy_identity_and_spin_configuration_are_exact() {
    let yielding = SpinThenYield::new(17);
    assert_eq!(yielding.spin_iterations(), 17);
    assert_eq!(WaitStrategy::strategy(&BusySpin), Strategy::BusySpin);
    assert_eq!(WaitStrategy::strategy(&yielding), Strategy::SpinThenYield);

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        let hardware = HardwareWait {
            config: HardwareConfig::Amd(AmdConfig::new(1)),
        };
        let hybrid = SpinThenHardwareWait {
            spin_iterations: 19,
            hardware,
        };
        assert_eq!(hybrid.spin_iterations(), 19);
        assert_eq!(StrategyImpl::strategy(&hardware), Strategy::HardwareWait);
        assert_eq!(
            StrategyImpl::strategy(&hybrid),
            Strategy::SpinThenHardwareWait
        );

        let already_changed = AtomicU32::new(2);
        assert_eq!(
            WaitStrategy::wait_if_equal(&hardware, &already_changed, 1),
            WaitResult::Changed(2)
        );
        assert_eq!(
            WaitStrategy::wait_if_equal(&hybrid, &already_changed, 1),
            WaitResult::Changed(2)
        );

        #[cfg(feature = "benchmark-only")]
        {
            let diagnostic = AmdMwaitxC1 {
                config: match hardware.config {
                    HardwareConfig::Amd(config) => config,
                    HardwareConfig::Intel(_) => unreachable!(),
                },
            };
            assert_eq!(StrategyImpl::strategy(&diagnostic), Strategy::HardwareWait);
            assert_eq!(
                WaitStrategy::wait_if_equal(&diagnostic, &already_changed, 1),
                WaitResult::Changed(2)
            );
        }
    }
}

#[test]
fn pure_wait_classification_helpers_preserve_the_contract() {
    let changed = AtomicU32::new(7);
    assert_eq!(
        spin_prefix(&changed, 6, 0, None),
        Some(WaitTimeoutResult::Changed(7))
    );
    assert_eq!(
        classify_after_wait(&changed, 6, None),
        WaitTimeoutResult::Changed(7)
    );

    let equal = AtomicU32::new(7);
    assert_eq!(spin_prefix(&equal, 7, 0, None), None);
    assert_eq!(
        classify_after_wait(&equal, 7, None),
        WaitTimeoutResult::Unclassified
    );
    assert_eq!(
        spin_prefix(&equal, 7, 0, Some(Deadline::new(Duration::ZERO))),
        Some(WaitTimeoutResult::TimedOut)
    );
}

#[test]
fn raw_yield_exposes_an_unclassified_wake() {
    let value = AtomicU32::new(7);
    assert_eq!(
        SpinThenYield::new(0).wait_if_equal(&value, 7),
        WaitResult::Unclassified
    );
    assert_eq!(
        SpinThenYield::new(1).wait_if_equal(&value, 7),
        WaitResult::Unclassified
    );
}

#[test]
fn both_atomic_widths_are_supported() {
    let small = AtomicU32::new(2);
    let large = AtomicU64::new(3);
    assert_eq!(
        SpinThenYield::new(0).wait_if_equal(&small, 1),
        WaitResult::Changed(2)
    );
    assert_eq!(
        SpinThenYield::new(0).wait_if_equal(&large, 1),
        WaitResult::Changed(3)
    );
}

#[test]
fn filtered_wait_absorbs_unclassified_wakes() {
    let value = Arc::new(AtomicU32::new(0));
    let start = Arc::new(Barrier::new(2));
    let consumer_value = Arc::clone(&value);
    let consumer_start = Arc::clone(&start);
    let consumer = std::thread::spawn(move || {
        consumer_start.wait();
        SpinThenYield::new(0).wait_until_different(&*consumer_value, 0)
    });

    start.wait();
    value.store(1, Ordering::Release);
    match consumer.join() {
        Ok(value) => assert_eq!(value, 1),
        Err(_) => panic!("consumer thread panicked"),
    }
}

#[test]
fn zero_timeout_still_reports_an_already_changed_value() {
    let value = AtomicU32::new(9);
    assert_eq!(
        BusySpin.wait_until_different_timeout(&value, 8, Duration::ZERO),
        WaitUntilTimeoutResult::Changed(9)
    );
    assert_eq!(
        BusySpin.wait_until_different_timeout(&value, 9, Duration::ZERO),
        WaitUntilTimeoutResult::TimedOut
    );
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn u64_cycle_conversion_scales_before_dividing_into_a_nontrivial_result() {
    assert_eq!(
        duration_to_cycles_u64(Duration::from_nanos(37), 1_000_000_000),
        37
    );
    assert_eq!(duration_to_cycles_u64(Duration::ZERO, 1_000_000_000), 1);
}
