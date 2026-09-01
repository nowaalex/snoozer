#![deny(unsafe_code)]

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

mod sealed {
    pub trait Sealed {}
}

/// An atomic integer that can be observed by a hardware wait strategy.
///
/// This trait is sealed because the strategy relies on the address remaining
/// valid and on its loads being atomic. It is implemented for [`AtomicU32`]
/// and [`AtomicU64`].
///
/// Hardware may monitor a larger granule than the atomic itself. When extra
/// wakes matter, place a frequently watched atomic away from unrelated hot
/// writes, preferably in separately aligned storage. This is a performance
/// recommendation, not a memory-safety requirement or a guarantee about a
/// processor's monitoring granule.
pub trait WaitableAtomic: sealed::Sealed + Send + Sync {
    /// The integer stored by this atomic.
    type Value: Copy + Eq;

    #[doc(hidden)]
    fn __load_acquire(&self) -> Self::Value;

    #[doc(hidden)]
    fn __monitored_address(&self) -> *const ();
}

impl sealed::Sealed for AtomicU32 {}

impl WaitableAtomic for AtomicU32 {
    type Value = u32;

    #[inline]
    fn __load_acquire(&self) -> Self::Value {
        self.load(Ordering::Acquire)
    }

    #[inline]
    fn __monitored_address(&self) -> *const () {
        self.as_ptr().cast_const().cast()
    }
}

impl sealed::Sealed for AtomicU64 {}

impl WaitableAtomic for AtomicU64 {
    type Value = u64;

    #[inline]
    fn __load_acquire(&self) -> Self::Value {
        self.load(Ordering::Acquire)
    }

    #[inline]
    fn __monitored_address(&self) -> *const () {
        self.as_ptr().cast_const().cast()
    }
}

#[cfg(test)]
#[path = "../tests/unit/waitable_atomic_internals.rs"]
mod waitable_atomic_internals;
