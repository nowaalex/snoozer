#![deny(unsafe_code)]

use std::error::Error;
use std::fmt::{Display, Formatter};

/// A closed name for a built-in wait strategy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Strategy {
    /// Continuously poll the observed atomic.
    BusySpin,
    /// Poll briefly, then yield once.
    SpinThenYield,
    /// AMD `MONITORX`/`MWAITX` without entering a C-state.
    AmdMwaitx,
    /// Poll briefly, then use AMD `MONITORX`/`MWAITX`.
    SpinThenAmdMwaitx,
}

/// Why a requested hardware strategy cannot run on this process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsupportedReason {
    /// The operating system or target architecture is not supported.
    UnsupportedTarget,
    /// The CPU vendor is not AMD.
    NotAmd,
    /// CPUID does not advertise `MONITORX`/`MWAITX`.
    MissingMonitorxMwaitx,
    /// CPUID does not advertise an invariant timestamp counter.
    MissingInvariantTsc,
    /// CPUID does not advertise `RDTSCP`.
    MissingRdtscp,
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
    /// The strategy whose construction failed.
    #[must_use]
    pub const fn strategy(self) -> Strategy {
        self.strategy
    }

    /// The guard that rejected the strategy.
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
