use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, OnceLock};

use super::*;

fn amd_capabilities() -> crate::Capabilities {
    crate::Capabilities {
        supported_target: true,
        amd_vendor: true,
        intel_vendor: false,
        monitorx_mwaitx: true,
        waitpkg: false,
        invariant_tsc: true,
        rdtscp: true,
    }
}

fn intel_capabilities() -> crate::Capabilities {
    crate::Capabilities {
        amd_vendor: false,
        intel_vendor: true,
        monitorx_mwaitx: false,
        waitpkg: true,
        ..amd_capabilities()
    }
}

#[test]
fn backend_selection_is_native_and_fail_closed() {
    assert_eq!(
        select_backend(&amd_capabilities()),
        Ok(HardwareBackend::AmdMwaitx)
    );
    assert_eq!(
        select_backend(&intel_capabilities()),
        Ok(HardwareBackend::IntelUmwait)
    );
    assert_eq!(
        select_backend(&crate::Capabilities {
            monitorx_mwaitx: false,
            ..amd_capabilities()
        }),
        Err(UnsupportedReason::MissingMonitorxMwaitx)
    );
    assert_eq!(
        select_backend(&crate::Capabilities {
            waitpkg: false,
            ..intel_capabilities()
        }),
        Err(UnsupportedReason::MissingWaitpkg)
    );
    assert_eq!(
        select_backend(&crate::Capabilities {
            supported_target: false,
            ..amd_capabilities()
        }),
        Err(UnsupportedReason::UnsupportedTarget)
    );
    assert_eq!(
        select_backend(&crate::Capabilities {
            amd_vendor: false,
            ..amd_capabilities()
        }),
        Err(UnsupportedReason::UnsupportedCpuVendor)
    );
    assert_eq!(
        select_backend(&crate::Capabilities {
            intel_vendor: true,
            ..amd_capabilities()
        }),
        Err(UnsupportedReason::UnsupportedCpuVendor)
    );
}

#[test]
fn concurrent_initializers_execute_the_probe_once() {
    let state = Arc::new(OnceLock::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(9));
    let callers = (0..8)
        .map(|_| {
            let state = Arc::clone(&state);
            let calls = Arc::clone(&calls);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                initialize_once(&state, || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok(41_u32)
                })
            })
        })
        .collect::<Vec<_>>();

    start.wait();
    for caller in callers {
        assert_eq!(
            caller.join().expect("initializer caller must not panic"),
            Ok(41)
        );
    }
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn completed_failure_is_cached_without_retry() {
    let state = OnceLock::new();
    let calls = AtomicUsize::new(0);
    let expected = HardwareWaitError::PreflightFailed {
        backend: HardwareBackend::IntelUmwait,
        reason: PreflightFailure::Inconclusive,
    };

    assert_eq!(
        initialize_once(&state, || {
            calls.fetch_add(1, Ordering::Relaxed);
            Err(expected)
        }),
        Err(expected)
    );
    assert_eq!(
        initialize_once(&state, || {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(99_u32)
        }),
        Err(expected)
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn initializer_panic_becomes_a_permanent_typed_failure() {
    let state = OnceLock::new();
    assert_eq!(
        initialize_once(&state, || -> Result<u32, HardwareWaitError> {
            panic!("injected preflight panic")
        }),
        Err(HardwareWaitError::PreflightPanicked)
    );
    assert_eq!(
        initialize_once(&state, || Ok(7_u32)),
        Err(HardwareWaitError::PreflightPanicked)
    );
}

#[test]
fn preflight_report_accessors_return_every_exact_field() {
    let report = PreflightReport {
        backend: HardwareBackend::IntelUmwait,
        attempts: 13,
        verified_store_wakes: 7,
        baseline_wait: std::time::Duration::from_micros(29),
    };

    assert_eq!(report.backend(), HardwareBackend::IntelUmwait);
    assert_eq!(report.attempts(), 13);
    assert_eq!(report.verified_store_wakes(), 7);
    assert_eq!(report.baseline_wait(), std::time::Duration::from_micros(29));
}

#[test]
fn preflight_orchestration_preserves_the_prepared_config_and_probe_report() {
    let config = HardwareConfig::Amd(AmdConfig::new(2_400_000_000));
    let report = PreflightReport {
        backend: HardwareBackend::AmdMwaitx,
        attempts: 11,
        verified_store_wakes: 3,
        baseline_wait: std::time::Duration::from_micros(41),
    };

    assert_eq!(
        prepare_and_probe_hardware_wait(
            || Ok(config),
            |prepared| {
                assert_eq!(prepared, config);
                Ok(report)
            },
        ),
        Ok(PreparedHardwareWait { config, report })
    );
}

#[test]
fn trial_evidence_requires_a_publication_before_a_fast_changed_return() {
    let wait_finished = Instant::now();
    let published_before = wait_finished
        .checked_sub(Duration::from_micros(2))
        .expect("a recent instant must be representable");
    let published_after = wait_finished + Duration::from_micros(2);
    let fast = Duration::from_micros(3);
    let threshold = Duration::from_micros(4);

    assert_eq!(
        classify_preflight_trial(Some(published_before), wait_finished, 1, fast, threshold),
        PreflightTrialEvidence {
            helper_overlapped: true,
            verified_store_wake: true,
        }
    );
    for evidence in [
        classify_preflight_trial(None, wait_finished, 1, fast, threshold),
        classify_preflight_trial(Some(published_after), wait_finished, 1, fast, threshold),
        classify_preflight_trial(Some(published_before), wait_finished, 0, fast, threshold),
        classify_preflight_trial(
            Some(published_before),
            wait_finished,
            1,
            threshold,
            threshold,
        ),
    ] {
        assert!(!evidence.verified_store_wake);
    }
}

#[test]
fn preflight_conclusion_distinguishes_scheduling_from_missing_store_wakes() {
    let backend = HardwareBackend::AmdMwaitx;
    let baseline_wait = Duration::from_micros(37);
    assert_eq!(
        conclude_preflight(backend, 16, 0, 0, baseline_wait),
        Err(HardwareWaitError::PreflightFailed {
            backend,
            reason: PreflightFailure::Inconclusive,
        })
    );
    assert_eq!(
        conclude_preflight(backend, 16, 1, 4, baseline_wait),
        Err(HardwareWaitError::PreflightFailed {
            backend,
            reason: PreflightFailure::StoreWakeNotObserved,
        })
    );

    let report = conclude_preflight(backend, 9, 2, 3, baseline_wait)
        .expect("the required store-wake evidence must pass");
    assert_eq!(report.backend(), backend);
    assert_eq!(report.attempts(), 9);
    assert_eq!(report.verified_store_wakes(), 2);
    assert_eq!(report.baseline_wait(), baseline_wait);
}
