use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use snoozer::{AmdMwaitx, WaitResult, WaitStrategy, WaitUntilTimeoutResult};

const HARDWARE_TEST_TIMEOUT: Duration = Duration::from_secs(2);
const REQUIRE_AMD_MWAITX_ENV: &str = "SNOOZER_REQUIRE_AMD_MWAITX";

fn hardware_is_required(value: Option<&OsStr>) -> Result<bool, String> {
    match value {
        None => Ok(false),
        Some(value) if value == OsStr::new("1") => Ok(true),
        Some(value) => Err(format!(
            "{REQUIRE_AMD_MWAITX_ENV} must be unset or exactly 1, got {value:?}"
        )),
    }
}

#[derive(Debug, Eq, PartialEq)]
enum UnavailableHardware {
    Skip(String),
    Fail(String),
}

fn classify_unavailable_hardware(
    required: bool,
    test_name: &str,
    error: &str,
) -> UnavailableHardware {
    if required {
        UnavailableHardware::Fail(format!(
            "{test_name} requires AMD MONITORX/MWAITX because {REQUIRE_AMD_MWAITX_ENV}=1, but the strategy is unavailable: {error}"
        ))
    } else {
        UnavailableHardware::Skip(format!(
            "{test_name}: AMD MONITORX/MWAITX is unavailable: {error}; set {REQUIRE_AMD_MWAITX_ENV}=1 to require target-hardware evidence"
        ))
    }
}

fn hardware_strategy(test_name: &str) -> Option<AmdMwaitx> {
    let required = hardware_is_required(std::env::var_os(REQUIRE_AMD_MWAITX_ENV).as_deref())
        .unwrap_or_else(|error| panic!("{error}"));
    match AmdMwaitx::new() {
        Ok(strategy) => Some(strategy),
        Err(error) => {
            match classify_unavailable_hardware(required, test_name, &error.to_string()) {
                UnavailableHardware::Skip(message) => {
                    eprintln!("SKIP {message}");
                    None
                }
                UnavailableHardware::Fail(message) => panic!("{message}"),
            }
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
fn unsupported_hardware_is_skipped_by_default_and_fails_when_required() {
    let skipped = classify_unavailable_hardware(false, "probe", "unsupported target");
    let failed = classify_unavailable_hardware(true, "probe", "unsupported target");

    assert!(matches!(skipped, UnavailableHardware::Skip(_)));
    assert!(matches!(failed, UnavailableHardware::Fail(_)));
    for outcome in [skipped, failed] {
        let message = match outcome {
            UnavailableHardware::Skip(message) | UnavailableHardware::Fail(message) => message,
        };
        assert!(message.contains("probe"));
        assert!(message.contains("unsupported target"));
        assert!(message.contains(REQUIRE_AMD_MWAITX_ENV));
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
#[test]
fn strict_gate_classifies_this_target_as_unsupported() {
    let error = AmdMwaitx::new().expect_err("this target must reject the AMD hardware strategy");
    assert!(matches!(
        classify_unavailable_hardware(true, "portable_probe", &error.to_string()),
        UnavailableHardware::Fail(_)
    ));
}

#[test]
fn supported_mwaitx_executes_one_bounded_raw_wait_on_an_equal_atomic() {
    let Some(strategy) =
        hardware_strategy("supported_mwaitx_executes_one_bounded_raw_wait_on_an_equal_atomic")
    else {
        return;
    };
    let state = AtomicU32::new(0);
    let started = Instant::now();

    assert_eq!(strategy.wait_if_equal(&state, 0), WaitResult::Unclassified);
    assert!(
        started.elapsed() < HARDWARE_TEST_TIMEOUT,
        "raw MWAITX did not return within its safety bound"
    );
    assert_eq!(state.load(Ordering::Acquire), 0);
}

#[test]
fn supported_mwaitx_wakes_for_a_monitored_store() {
    let Some(strategy) = hardware_strategy("supported_mwaitx_wakes_for_a_monitored_store") else {
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
    let Some(strategy) = hardware_strategy("supported_mwaitx_hardware_timeout_is_bounded") else {
        return;
    };
    let state = AtomicU32::new(0);

    assert_eq!(
        strategy.wait_until_different_timeout(&state, 0, Duration::from_millis(5)),
        WaitUntilTimeoutResult::TimedOut
    );
}
