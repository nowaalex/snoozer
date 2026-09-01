//! The only benchmark module coupled to the public crate API.
//!
//! Keeping these conversions in one file makes API drift obvious during merge;
//! the timing engine itself remains generic and contains no dynamic dispatch.

use std::sync::atomic::AtomicU64;

use snoozer::{ParkResult, Parker, WaitResult, WaitStrategy};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Observation {
    Changed(u64),
    Unclassified,
}

#[inline]
pub(crate) fn wait_direct_raw<S: WaitStrategy>(
    strategy: &S,
    atomic: &AtomicU64,
    expected: u64,
) -> Observation {
    match strategy.wait_if_equal(atomic, expected) {
        WaitResult::Changed(value) => Observation::Changed(value),
        WaitResult::Unclassified => Observation::Unclassified,
    }
}

#[inline]
pub(crate) fn wait_direct_filtered<S: WaitStrategy>(
    strategy: &S,
    atomic: &AtomicU64,
    expected: u64,
) -> u64 {
    strategy.wait_until_different(atomic, expected)
}

#[inline]
pub(crate) fn wait_parker_raw<S: WaitStrategy>(parker: &mut Parker<S>) -> bool {
    match parker.park() {
        ParkResult::Notified => true,
        ParkResult::Unclassified => false,
    }
}

#[inline]
pub(crate) fn wait_parker_filtered<S: WaitStrategy>(parker: &mut Parker<S>) {
    parker.park_until_notified();
}
