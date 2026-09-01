//! Low-latency waiting primitives for dedicated consumer threads.
//!
//! The raw operations return after one strategy-specific wait and can expose
//! an unclassified wake. The filtered operations absorb those wakes until the
//! observed atomic changes or the requested timeout expires.

#![deny(unsafe_code)]

mod arch;
mod atomic;
mod capabilities;
mod error;
mod parker;
mod strategy;

pub use atomic::WaitableAtomic;
pub use capabilities::{Capabilities, capabilities};
pub use error::{Strategy, UnsupportedReason, UnsupportedStrategy};
pub use parker::{
    NotificationTimeoutResult, ParkResult, ParkTimeoutResult, Parker, Unparker, pair,
};
pub use strategy::{
    AmdMwaitx, BusySpin, SpinThenAmdMwaitx, SpinThenYield, WaitResult, WaitStrategy,
    WaitTimeoutResult, WaitUntilTimeoutResult,
};

/// Diagnostic strategies used only by this repository's benchmark suite.
#[cfg(feature = "benchmark-only")]
#[doc(hidden)]
pub mod benchmark {
    pub use crate::strategy::AmdMwaitxC1;
}
