#![deny(unsafe_code)]

use std::sync::OnceLock;

use crate::arch;

/// Cached, side-effect-free hardware facts used to select an x86 backend.
///
/// This snapshot performs CPUID discovery only. Timer calibration and the
/// functional wait probe belong exclusively to [`crate::HardwareWait::preflight`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Capabilities {
    /// The process is running on the supported Linux x86-64 target.
    pub supported_target: bool,
    /// CPUID reports an AMD processor vendor.
    pub amd_vendor: bool,
    /// CPUID reports an Intel processor vendor.
    pub intel_vendor: bool,
    /// CPUID advertises AMD `MONITORX`/`MWAITX`.
    pub monitorx_mwaitx: bool,
    /// CPUID advertises Intel `UMONITOR`/`UMWAIT`.
    pub waitpkg: bool,
    /// CPUID advertises an invariant timestamp counter.
    pub invariant_tsc: bool,
    /// CPUID advertises ordered `RDTSCP` reads.
    pub rdtscp: bool,
}

static CAPABILITIES: OnceLock<Capabilities> = OnceLock::new();

/// Returns cached CPUID hardware facts without running a hardware wait.
///
/// ```
/// let facts = snoozer::capabilities();
/// if facts.amd_vendor {
///     assert!(!facts.intel_vendor);
/// }
/// ```
#[must_use]
pub fn capabilities() -> &'static Capabilities {
    CAPABILITIES.get_or_init(arch::detect_capabilities)
}

#[cfg(test)]
#[path = "../tests/unit/capability_cache.rs"]
mod capability_cache;
