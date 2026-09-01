use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::*;

#[test]
fn mwaitx_c_state_hints_match_the_architectural_literals() {
    assert_eq!(NO_C_STATE_HINT, 0xf0);

    #[cfg(feature = "benchmark-only")]
    assert_eq!(C1_STATE_HINT, 0);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn untimed_mwaitx_protocol_arms_rechecks_waits_and_classifies() {
    let config = AmdConfig::new(1_000_000_000);

    let changed_during_arm = AtomicU32::new(0);
    let waited_after_arm_change = AtomicBool::new(false);
    assert_eq!(
        mwaitx_untimed_protocol(
            &config,
            NO_C_STATE_HINT,
            &changed_during_arm,
            0,
            |_| changed_during_arm.store(1, Ordering::Release),
            |_, _| waited_after_arm_change.store(true, Ordering::Relaxed),
        ),
        WaitTimeoutResult::Changed(1)
    );
    assert!(!waited_after_arm_change.load(Ordering::Relaxed));

    let changed_during_wait = AtomicU32::new(0);
    let observed_cycles = AtomicU32::new(0);
    let observed_hint = AtomicU32::new(0);
    assert_eq!(
        mwaitx_untimed_protocol(
            &config,
            NO_C_STATE_HINT,
            &changed_during_wait,
            0,
            |_| {},
            |cycles, hint| {
                observed_cycles.store(cycles, Ordering::Relaxed);
                observed_hint.store(hint, Ordering::Relaxed);
                changed_during_wait.store(1, Ordering::Release);
            },
        ),
        WaitTimeoutResult::Changed(1)
    );
    assert_eq!(observed_cycles.load(Ordering::Relaxed), 1_000_000);
    assert_eq!(observed_hint.load(Ordering::Relaxed), 0xf0);

    let unchanged = AtomicU32::new(0);
    assert_eq!(
        mwaitx_untimed_protocol(&config, NO_C_STATE_HINT, &unchanged, 0, |_| {}, |_, _| {},),
        WaitTimeoutResult::Unclassified
    );
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn timed_mwaitx_protocol_honors_public_deadlines_and_safety_cap() {
    let config = AmdConfig::new(1_000_000_000);
    let unchanged = AtomicU32::new(0);

    let armed_after_timeout = AtomicBool::new(false);
    assert_eq!(
        mwaitx_timed_protocol(
            &config,
            NO_C_STATE_HINT,
            &unchanged,
            0,
            Deadline::new(Duration::ZERO),
            |_| armed_after_timeout.store(true, Ordering::Relaxed),
            |_, _| {},
        ),
        WaitTimeoutResult::TimedOut
    );
    assert!(!armed_after_timeout.load(Ordering::Relaxed));

    let observed_cycles = AtomicU32::new(0);
    assert_eq!(
        mwaitx_timed_protocol(
            &config,
            NO_C_STATE_HINT,
            &unchanged,
            0,
            Deadline::new(Duration::from_secs(1)),
            |_| {},
            |cycles, _| observed_cycles.store(cycles, Ordering::Relaxed),
        ),
        WaitTimeoutResult::Unclassified
    );
    assert_eq!(observed_cycles.load(Ordering::Relaxed), 1_000_000);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn mwaitx_duration_conversion_is_bounded_and_scaled() {
    assert_eq!(duration_to_cycles(Duration::ZERO, 1_000_000_000), 1);
    assert_eq!(
        duration_to_cycles(Duration::from_nanos(37), 1_000_000_000),
        37
    );
    assert_eq!(
        duration_to_cycles(Duration::from_secs(u64::MAX), u64::MAX),
        u32::MAX
    );

    let config = AmdConfig::new(1_000_000_000);
    assert_eq!(mwaitx_timeout_cycles(&config, Duration::from_nanos(37)), 37);
    assert_eq!(
        mwaitx_timeout_cycles(&config, MWAITX_SAFETY_TIMEOUT),
        1_000_000
    );
    assert_eq!(
        mwaitx_timeout_cycles(&config, Duration::from_secs(1)),
        1_000_000
    );

    // Make the branch choice observable independently of the conversion:
    // an exact safety-cap budget must reuse the value cached by AmdConfig.
    let sentinel_config = AmdConfig {
        timer_hz: 1_000_000_000,
        safety_timeout_cycles: 17,
    };
    assert_eq!(
        mwaitx_timeout_cycles(&sentinel_config, MWAITX_SAFETY_TIMEOUT),
        17
    );
}

#[test]
fn forced_capability_failures_are_typed_without_executing_assembly() {
    let supported = crate::Capabilities {
        supported_target: true,
        amd_vendor: true,
        intel_vendor: false,
        monitorx_mwaitx: true,
        waitpkg: false,
        invariant_tsc: true,
        rdtscp: true,
    };
    assert_eq!(require_shared_timer_capabilities(&supported), Ok(()));

    assert_eq!(
        require_shared_timer_capabilities(&crate::Capabilities {
            invariant_tsc: false,
            ..supported
        }),
        Err(unsupported(UnsupportedReason::MissingInvariantTsc))
    );
    assert_eq!(
        require_shared_timer_capabilities(&crate::Capabilities {
            rdtscp: false,
            ..supported
        }),
        Err(unsupported(UnsupportedReason::MissingRdtscp))
    );
}
