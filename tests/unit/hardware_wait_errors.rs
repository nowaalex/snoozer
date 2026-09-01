use super::*;

#[test]
fn unsupported_error_exposes_typed_fields_and_display_context() {
    let error = UnsupportedStrategy {
        strategy: Strategy::HardwareWait,
        reason: UnsupportedReason::MissingMonitorxMwaitx,
    };

    assert_eq!(error.strategy(), Strategy::HardwareWait);
    assert_eq!(error.reason(), UnsupportedReason::MissingMonitorxMwaitx);
    assert_eq!(
        error.to_string(),
        "strategy HardwareWait is unavailable: MissingMonitorxMwaitx"
    );
}

#[test]
fn preflight_errors_preserve_typed_context() {
    let required = HardwareWaitError::PreflightRequired;
    assert!(required.to_string().contains("preflight"));

    let failed = HardwareWaitError::PreflightFailed {
        backend: HardwareBackend::IntelUmwait,
        reason: PreflightFailure::StoreWakeNotObserved,
    };
    assert!(failed.to_string().contains("IntelUmwait"));
    assert!(failed.to_string().contains("StoreWakeNotObserved"));
}

#[test]
fn hardware_wait_error_sources_preserve_the_nested_error_only() {
    let nested = UnsupportedStrategy {
        strategy: Strategy::HardwareWait,
        reason: UnsupportedReason::MissingWaitpkg,
    };
    let unsupported = HardwareWaitError::Unsupported(nested);
    let source = std::error::Error::source(&unsupported)
        .expect("Unsupported must expose its UnsupportedStrategy source");
    assert_eq!(source.downcast_ref::<UnsupportedStrategy>(), Some(&nested));

    for error in [
        HardwareWaitError::PreflightRequired,
        HardwareWaitError::PreflightPanicked,
        HardwareWaitError::PreflightFailed {
            backend: HardwareBackend::AmdMwaitx,
            reason: PreflightFailure::WaitDidNotBlock,
        },
    ] {
        assert!(std::error::Error::source(&error).is_none());
    }
}
