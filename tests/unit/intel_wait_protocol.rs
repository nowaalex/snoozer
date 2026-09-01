use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::*;

#[test]
fn changed_during_arm_skips_umwait() {
    let config = IntelConfig::new(1_000_000_000);
    let value = AtomicU32::new(0);
    let waited = AtomicBool::new(false);

    assert_eq!(
        umwait_untimed_protocol(
            &config,
            &value,
            0,
            |_| value.store(1, Ordering::Release),
            |cycles| cycles,
            |_| waited.store(true, Ordering::Relaxed),
        ),
        WaitTimeoutResult::Changed(1)
    );
    assert!(!waited.load(Ordering::Relaxed));
}

#[test]
fn changed_during_fake_umwait_is_classified() {
    let config = IntelConfig::new(1_000_000_000);
    let value = AtomicU32::new(0);

    assert_eq!(
        umwait_untimed_protocol(
            &config,
            &value,
            0,
            |_| {},
            |cycles| cycles,
            |_| value.store(1, Ordering::Release),
        ),
        WaitTimeoutResult::Changed(1)
    );
}

#[test]
fn unchanged_after_fake_umwait_is_unclassified() {
    let config = IntelConfig::new(1_000_000_000);
    let value = AtomicU32::new(0);

    assert_eq!(
        umwait_untimed_protocol(&config, &value, 0, |_| {}, |cycles| cycles, |_| {}),
        WaitTimeoutResult::Unclassified
    );
}

#[test]
fn zero_public_deadline_skips_umonitor_and_umwait() {
    let config = IntelConfig::new(1_000_000_000);
    let value = AtomicU32::new(0);
    let armed = AtomicBool::new(false);
    let waited = AtomicBool::new(false);

    assert_eq!(
        umwait_timed_protocol(
            &config,
            &value,
            0,
            Deadline::new(Duration::ZERO),
            |_| armed.store(true, Ordering::Relaxed),
            |_| panic!("an expired public deadline must not read RDTSCP"),
            |_| waited.store(true, Ordering::Relaxed),
        ),
        WaitTimeoutResult::TimedOut
    );
    assert!(!armed.load(Ordering::Relaxed));
    assert!(!waited.load(Ordering::Relaxed));
}

#[test]
fn changed_during_timed_arm_skips_deadline_conversion_and_umwait() {
    let config = IntelConfig::new(1_000_000_000);
    let value = AtomicU32::new(0);
    let waited = AtomicBool::new(false);

    assert_eq!(
        umwait_timed_protocol(
            &config,
            &value,
            0,
            Deadline::new(Duration::from_secs(1)),
            |_| value.store(1, Ordering::Release),
            |_| panic!("a changed recheck must not read RDTSCP"),
            |_| waited.store(true, Ordering::Relaxed),
        ),
        WaitTimeoutResult::Changed(1)
    );
    assert!(!waited.load(Ordering::Relaxed));
}

#[test]
fn umwait_timeout_conversion_uses_a_strict_safety_boundary() {
    let config = IntelConfig {
        timer_hz: 1_000_000_000,
        safety_timeout_cycles: 17,
    };

    assert_eq!(umwait_timeout_cycles(&config, Duration::from_nanos(37)), 37);
    assert_eq!(umwait_timeout_cycles(&config, UMWAIT_SAFETY_TIMEOUT), 17);
    assert_eq!(
        umwait_timeout_cycles(
            &config,
            UMWAIT_SAFETY_TIMEOUT.saturating_add(Duration::from_nanos(1)),
        ),
        17
    );
}

fn intel_capabilities() -> crate::Capabilities {
    crate::Capabilities {
        supported_target: true,
        amd_vendor: false,
        intel_vendor: true,
        monitorx_mwaitx: false,
        waitpkg: true,
        invariant_tsc: true,
        rdtscp: true,
    }
}

#[test]
fn hardware_config_requires_userspace_tsc_before_calibration() {
    assert_eq!(
        prepare_hardware_config(
            &intel_capabilities(),
            || false,
            || panic!("the unselected AMD calibrator must not run"),
            || panic!("calibration must not run with userspace TSC disabled"),
        ),
        Err(unsupported(UnsupportedReason::TscAccessDisabled))
    );
}

#[test]
fn hardware_config_selects_only_the_native_calibrator() {
    let timer_hz = 2_400_000_000;
    assert_eq!(
        prepare_hardware_config(
            &intel_capabilities(),
            || true,
            || panic!("the unselected AMD calibrator must not run"),
            || Some(timer_hz),
        ),
        Ok(HardwareConfig::Intel(IntelConfig::new(timer_hz)))
    );

    assert_eq!(
        prepare_hardware_config(
            &intel_capabilities(),
            || true,
            || panic!("the unselected AMD calibrator must not run"),
            || None,
        ),
        Err(unsupported(UnsupportedReason::UnstableTimerCalibration))
    );
}
