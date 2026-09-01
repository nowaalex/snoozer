#![deny(unsafe_code)]

use std::sync::OnceLock;

use crate::arch;

/// Cached hardware facts used to guard the AMD strategy.
///
/// This structure is non-exhaustive so later Intel and Arm backends can add
/// capability fields without breaking callers that inspect this snapshot.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Capabilities {
    /// The process is running on the supported Linux x86-64 target.
    pub supported_target: bool,
    /// CPUID reports an AMD processor vendor.
    pub amd_vendor: bool,
    /// CPUID advertises AMD `MONITORX`/`MWAITX`.
    pub monitorx_mwaitx: bool,
    /// CPUID advertises an invariant timestamp counter.
    pub invariant_tsc: bool,
    /// CPUID advertises ordered `RDTSCP` reads.
    pub rdtscp: bool,
    /// Calibrated conservative MWAITX timer frequency.
    pub mwaitx_timer_hz: Option<u64>,
}

static CAPABILITIES: OnceLock<Capabilities> = OnceLock::new();

/// Returns the process-wide cached hardware capability snapshot.
#[must_use]
pub fn capabilities() -> &'static Capabilities {
    CAPABILITIES.get_or_init(arch::detect_capabilities)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_is_cached() {
        let first = capabilities() as *const Capabilities;
        let second = capabilities() as *const Capabilities;
        assert_eq!(first, second);
    }
}
