#![deny(unsafe_code)]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
use crate::Capabilities;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod x86_64;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) use x86_64::amd::{detect_capabilities, monitorx, mwaitx};

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(crate) fn detect_capabilities() -> Capabilities {
    Capabilities::default()
}
