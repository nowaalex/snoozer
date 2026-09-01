#![deny(unsafe_code)]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
use crate::Capabilities;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod x86_64;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) use x86_64::amd::{calibrate_timer_hz as calibrate_amd_timer_hz, monitorx, mwaitx};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) use x86_64::detect_capabilities;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) use x86_64::intel::{
    calibrate_timer_hz as calibrate_intel_timer_hz, read_tsc_ordered, umonitor, umwait_c01,
    userspace_tsc_is_enabled,
};

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(crate) fn detect_capabilities() -> Capabilities {
    Capabilities::default()
}
