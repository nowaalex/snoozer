#![deny(unsafe_code)]

use std::error::Error;
use std::fmt::{Display, Formatter};

/// Identifies a built-in wait strategy.
///
/// New architecture backends may be selected behind [`Strategy::HardwareWait`]
/// without changing callers.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Strategy {
    /// Continuously poll the observed atomic.
    BusySpin,
    /// Poll briefly, then yield once.
    SpinThenYield,
    /// Use the hardware-address monitor selected by process-wide preflight.
    HardwareWait,
    /// Poll briefly, then use the selected hardware-address monitor.
    SpinThenHardwareWait,
}

/// Hardware backend selected for this process.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HardwareBackend {
    /// AMD `MONITORX`/`MWAITX`, with production C-state entry disabled.
    AmdMwaitx,
    /// Intel `UMONITOR`/`UMWAIT`, requesting the fast C0.1 state.
    IntelUmwait,
}

/// Why the target cannot construct a hardware wait strategy.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsupportedReason {
    /// The operating system or target architecture is not supported.
    UnsupportedTarget,
    /// The CPU vendor has no backend in this release.
    UnsupportedCpuVendor,
    /// CPUID does not advertise AMD `MONITORX`/`MWAITX`.
    MissingMonitorxMwaitx,
    /// CPUID does not advertise Intel `UMONITOR`/`UMWAIT`.
    MissingWaitpkg,
    /// CPUID does not advertise an invariant timestamp counter.
    MissingInvariantTsc,
    /// CPUID does not advertise ordered `RDTSCP` reads.
    MissingRdtscp,
    /// Linux has disabled user-space timestamp-counter reads for this thread.
    TscAccessDisabled,
    /// A stable timer frequency could not be established.
    UnstableTimerCalibration,
}

/// Construction error for a strategy that cannot safely execute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsupportedStrategy {
    pub(crate) strategy: Strategy,
    pub(crate) reason: UnsupportedReason,
}

impl UnsupportedStrategy {
    /// Returns the strategy whose construction failed.
    ///
    /// ```no_run
    /// # use snoozer::{HardwareWait, HardwareWaitError};
    /// if let Err(HardwareWaitError::Unsupported(error)) = HardwareWait::preflight() {
    ///     let strategy = error.strategy();
    /// }
    /// ```
    #[must_use]
    pub const fn strategy(self) -> Strategy {
        self.strategy
    }

    /// Returns the guard that rejected the strategy.
    ///
    /// ```no_run
    /// # use snoozer::{HardwareWait, HardwareWaitError};
    /// if let Err(HardwareWaitError::Unsupported(error)) = HardwareWait::preflight() {
    ///     let reason = error.reason();
    /// }
    /// ```
    #[must_use]
    pub const fn reason(self) -> UnsupportedReason {
        self.reason
    }
}

impl Display for UnsupportedStrategy {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "strategy {:?} is unavailable: {:?}",
            self.strategy, self.reason
        )
    }
}

impl Error for UnsupportedStrategy {}

/// Why the one-time functional preflight rejected a supported backend.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreflightFailure {
    /// Hardware waits repeatedly returned too quickly to have blocked.
    WaitDidNotBlock,
    /// A store to the monitored address was not observed to wake the waiter.
    StoreWakeNotObserved,
    /// Scheduling did not provide a conclusive waiter/producer overlap.
    Inconclusive,
}

/// Failure to initialize or construct the process-wide hardware wait backend.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HardwareWaitError {
    /// [`crate::HardwareWait::preflight`] has not run in this process.
    PreflightRequired,
    /// The one-time initializer panicked; the failure is cached permanently.
    PreflightPanicked,
    /// Static capability checks rejected the target.
    Unsupported(UnsupportedStrategy),
    /// Static checks passed, but the functional probe failed closed.
    PreflightFailed {
        /// Backend exercised by the probe.
        backend: HardwareBackend,
        /// Functional condition that was not established.
        reason: PreflightFailure,
    },
}

impl From<UnsupportedStrategy> for HardwareWaitError {
    fn from(value: UnsupportedStrategy) -> Self {
        Self::Unsupported(value)
    }
}

impl Display for HardwareWaitError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PreflightRequired => formatter
                .write_str("hardware wait preflight must run once before constructing a strategy"),
            Self::PreflightPanicked => formatter.write_str(
                "hardware wait preflight panicked; hardware waiting remains unavailable",
            ),
            Self::Unsupported(error) => Display::fmt(error, formatter),
            Self::PreflightFailed { backend, reason } => write!(
                formatter,
                "hardware wait preflight for {backend:?} failed: {reason:?}"
            ),
        }
    }
}

impl Error for HardwareWaitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unsupported(error) => Some(error),
            Self::PreflightRequired | Self::PreflightPanicked | Self::PreflightFailed { .. } => {
                None
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/hardware_wait_errors.rs"]
mod hardware_wait_errors;
