//! Low-latency waiting primitives for dedicated consumer threads.
//!
//! The raw operations return after one strategy-specific wait and can expose
//! an unclassified wake. The filtered operations absorb those wakes until the
//! observed atomic changes or the requested timeout expires.

#![deny(unsafe_code)]
#![warn(missing_docs)]

mod arch;
mod atomic;
mod capabilities;
mod error;
mod parker;
mod strategy;

pub use atomic::WaitableAtomic;
pub use capabilities::{Capabilities, capabilities};
pub use error::{
    HardwareBackend, HardwareWaitError, PreflightFailure, Strategy, UnsupportedReason,
    UnsupportedStrategy,
};
pub use parker::{
    MultiParker, MultiUnparker, NotificationTimeoutResult, ParkResult, ParkTimeoutResult,
    SingleParker, SingleUnparker, multi_pair, single_pair,
};
pub use strategy::{
    BusySpin, HardwareWait, PreflightReport, SpinThenHardwareWait, SpinThenYield, WaitResult,
    WaitStrategy, WaitTimeoutResult, WaitUntilTimeoutResult,
};

/// Diagnostic strategies used only by this repository's benchmark suite.
#[cfg(feature = "benchmark-only")]
#[doc(hidden)]
pub mod benchmark {
    pub use crate::strategy::AmdMwaitxC1;
}
