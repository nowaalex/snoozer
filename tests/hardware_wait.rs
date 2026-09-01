use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use snoozer::{HardwareWait, WaitResult, WaitStrategy, WaitUntilTimeoutResult};

const HARDWARE_TEST_TIMEOUT: Duration = Duration::from_secs(2);
const REQUIRE_HARDWARE_WAIT_ENV: &str = "SNOOZER_REQUIRE_HARDWARE_WAIT";

fn hardware_is_required(value: Option<&OsStr>) -> Result<bool, String> {
    match value {
        None => Ok(false),
        Some(value) if value == OsStr::new("1") => Ok(true),
        Some(value) => Err(format!(
            "{REQUIRE_HARDWARE_WAIT_ENV} must be unset or exactly 1, got {value:?}"
        )),
    }
}

fn hardware_strategy(test_name: &str) -> Option<HardwareWait> {
    let required = hardware_is_required(std::env::var_os(REQUIRE_HARDWARE_WAIT_ENV).as_deref())
        .unwrap_or_else(|error| panic!("{error}"));
    let initialized = HardwareWait::preflight().and_then(|_| HardwareWait::new());
    match initialized {
        Ok(strategy) => Some(strategy),
        Err(error) if required => panic!(
            "{test_name} requires a verified hardware wait because \
             {REQUIRE_HARDWARE_WAIT_ENV}=1, but preflight failed: {error}"
        ),
        Err(error) => {
            eprintln!(
                "SKIP {test_name}: hardware wait preflight failed: {error}; set \
                 {REQUIRE_HARDWARE_WAIT_ENV}=1 to require target-hardware evidence"
            );
            None
        }
    }
}

#[test]
fn hardware_gate_value_is_strict_and_defaults_to_optional() {
    assert_eq!(hardware_is_required(None), Ok(false));
    assert_eq!(hardware_is_required(Some(OsStr::new("1"))), Ok(true));
    for invalid in ["", "0", "true", "yes"] {
        assert!(hardware_is_required(Some(OsStr::new(invalid))).is_err());
    }
}

#[test]
fn preflight_is_cached_and_new_is_cheap_after_success() {
    let Some(strategy) = hardware_strategy("preflight_is_cached_and_new_is_cheap_after_success")
    else {
        return;
    };
    let first = HardwareWait::preflight().expect("successful preflight must stay cached");
    let second = HardwareWait::preflight().expect("repeated preflight must return cached success");
    assert_eq!(first, second);
    assert_eq!(strategy.backend(), first.backend());
    assert_eq!(
        HardwareWait::new()
            .expect("new must read cached config")
            .backend(),
        first.backend()
    );
    assert!(first.verified_store_wakes() > 0);
    assert!(first.attempts() >= first.verified_store_wakes());
    assert!(!first.baseline_wait().is_zero());
}

#[test]
fn concurrent_preflight_callers_observe_one_cached_verdict() {
    let callers = (0..8)
        .map(|_| std::thread::spawn(HardwareWait::preflight))
        .collect::<Vec<_>>();
    let verdicts = callers
        .into_iter()
        .map(|caller| caller.join().expect("preflight caller must not panic"))
        .collect::<Vec<_>>();

    for verdict in &verdicts[1..] {
        assert_eq!(*verdict, verdicts[0]);
    }
}

#[test]
fn hardware_wait_executes_one_bounded_raw_wait_on_an_equal_atomic() {
    let Some(strategy) =
        hardware_strategy("hardware_wait_executes_one_bounded_raw_wait_on_an_equal_atomic")
    else {
        return;
    };
    let state = AtomicU32::new(0);
    let started = Instant::now();

    assert_eq!(strategy.wait_if_equal(&state, 0), WaitResult::Unclassified);
    assert!(
        started.elapsed() < HARDWARE_TEST_TIMEOUT,
        "raw hardware wait did not return within its safety bound"
    );
    assert_eq!(state.load(Ordering::Acquire), 0);
}

#[test]
fn hardware_wait_observes_a_release_store() {
    let Some(strategy) = hardware_strategy("hardware_wait_observes_a_release_store") else {
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
fn hardware_wait_public_timeout_is_bounded() {
    let Some(strategy) = hardware_strategy("hardware_wait_public_timeout_is_bounded") else {
        return;
    };
    let state = AtomicU32::new(0);

    assert_eq!(
        strategy.wait_until_different_timeout(&state, 0, Duration::from_millis(5)),
        WaitUntilTimeoutResult::TimedOut
    );
}
