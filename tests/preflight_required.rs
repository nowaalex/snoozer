use snoozer::{HardwareWait, HardwareWaitError};

#[test]
fn construction_before_preflight_fails_without_initializing_hardware() {
    assert_eq!(
        HardwareWait::new(),
        Err(HardwareWaitError::PreflightRequired)
    );
}
