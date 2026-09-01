//! The only benchmark module coupled to the public crate API.
//!
//! Keeping these conversions in one file makes API drift obvious during merge;
//! the timing engine itself remains generic and contains no dynamic dispatch.

use std::sync::atomic::AtomicU64;

use snoozer::{MultiParker, ParkResult, SingleParker, WaitResult, WaitStrategy};

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

pub(crate) trait BenchParker {
    fn wait_raw(&mut self) -> bool;

    fn wait_filtered(&mut self);
}

macro_rules! impl_bench_parker {
    ($parker:ident) => {
        impl<S: WaitStrategy> BenchParker for $parker<S> {
            #[inline]
            fn wait_raw(&mut self) -> bool {
                match self.park() {
                    ParkResult::Notified => true,
                    ParkResult::Unclassified => false,
                }
            }

            #[inline]
            fn wait_filtered(&mut self) {
                self.park_until_notified();
            }
        }
    };
}

impl_bench_parker!(SingleParker);
impl_bench_parker!(MultiParker);

#[inline]
pub(crate) fn wait_parker_raw<P: BenchParker>(parker: &mut P) -> bool {
    parker.wait_raw()
}

#[inline]
pub(crate) fn wait_parker_filtered<P: BenchParker>(parker: &mut P) {
    parker.wait_filtered();
}
