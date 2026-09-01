#![deny(unsafe_code)]

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::{WaitStrategy, WaitTimeoutResult};

const EMPTY_TOKEN: u32 = 0;
const NOTIFIED_TOKEN: u32 = 1;

#[repr(align(128))]
struct CacheLineToken(AtomicU32);

struct Shared {
    token: CacheLineToken,
}

/// Result of a raw park operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParkResult {
    /// A stored notification token was consumed with Acquire ordering.
    Notified,
    /// The strategy stopped waiting without a notification token.
    Unclassified,
}

/// Result of a bounded raw park operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParkTimeoutResult {
    /// A stored notification token was consumed with Acquire ordering.
    Notified,
    /// The strategy stopped waiting before the timeout without a token.
    Unclassified,
    /// The timeout expired without a token.
    TimedOut,
}

/// Result of a bounded filtered park operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationTimeoutResult {
    /// A stored notification token was consumed with Acquire ordering.
    Notified,
    /// The timeout expired without a token.
    TimedOut,
}

/// The single-consumer end of a notification pair.
///
/// `Parker` is `Send` but intentionally not `Sync`. Its wait methods require
/// exclusive access, so one consumer owns the token protocol at a time.
pub struct Parker<S> {
    shared: Arc<Shared>,
    strategy: S,
    not_sync: PhantomData<Cell<()>>,
}

/// The clonable producer end of a notification pair.
#[derive(Clone)]
pub struct Unparker {
    shared: Arc<Shared>,
}

/// Creates a single-consumer parker and its clonable producer handle.
#[must_use]
pub fn pair<S: WaitStrategy>(strategy: S) -> (Parker<S>, Unparker) {
    let shared = Arc::new(Shared {
        token: CacheLineToken(AtomicU32::new(EMPTY_TOKEN)),
    });
    (
        Parker {
            shared: Arc::clone(&shared),
            strategy,
            not_sync: PhantomData,
        },
        Unparker { shared },
    )
}

impl<S: WaitStrategy> Parker<S> {
    /// Returns the configured strategy.
    #[must_use]
    pub const fn strategy(&self) -> &S {
        &self.strategy
    }

    /// Consumes a ready token or performs one raw wait attempt.
    ///
    /// [`ParkResult::Unclassified`] does not prove that work is available and
    /// does not synchronize with a producer. Application state must be
    /// rechecked with an Acquire operation.
    #[must_use]
    pub fn park(&mut self) -> ParkResult {
        if self.take_notification() {
            return ParkResult::Notified;
        }

        let _wait_result = self
            .strategy
            .wait_if_equal(&self.shared.token.0, EMPTY_TOKEN);
        if self.take_notification() {
            ParkResult::Notified
        } else {
            ParkResult::Unclassified
        }
    }

    /// Absorbs unclassified wakes and returns only after consuming a token.
    pub fn park_until_notified(&mut self) {
        while self.park() != ParkResult::Notified {}
    }

    /// Consumes a ready token or performs one raw wait attempt bounded by
    /// `timeout`.
    #[must_use]
    pub fn park_timeout(&mut self, timeout: Duration) -> ParkTimeoutResult {
        if self.take_notification() {
            return ParkTimeoutResult::Notified;
        }

        let wait_result =
            self.strategy
                .wait_if_equal_timeout(&self.shared.token.0, EMPTY_TOKEN, timeout);
        if self.take_notification() {
            return ParkTimeoutResult::Notified;
        }

        match wait_result {
            WaitTimeoutResult::TimedOut => ParkTimeoutResult::TimedOut,
            WaitTimeoutResult::Changed(_) | WaitTimeoutResult::Unclassified => {
                ParkTimeoutResult::Unclassified
            }
        }
    }

    /// Absorbs unclassified wakes until a token is consumed or `timeout`
    /// expires.
    #[must_use]
    pub fn park_until_notified_timeout(&mut self, timeout: Duration) -> NotificationTimeoutResult {
        if self.take_notification() {
            return NotificationTimeoutResult::Notified;
        }

        let started = Instant::now();
        loop {
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return if self.take_notification() {
                    NotificationTimeoutResult::Notified
                } else {
                    NotificationTimeoutResult::TimedOut
                };
            }

            match self.park_timeout(timeout - elapsed) {
                ParkTimeoutResult::Notified => return NotificationTimeoutResult::Notified,
                ParkTimeoutResult::TimedOut => {
                    return if self.take_notification() {
                        NotificationTimeoutResult::Notified
                    } else {
                        NotificationTimeoutResult::TimedOut
                    };
                }
                ParkTimeoutResult::Unclassified => {}
            }
        }
    }

    #[inline]
    fn take_notification(&self) -> bool {
        self.shared
            .token
            .0
            .compare_exchange(
                NOTIFIED_TOKEN,
                EMPTY_TOKEN,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
    }
}

impl Unparker {
    /// Publishes one notification token with Release ordering.
    ///
    /// Multiple calls before the consumer takes the token coalesce into one.
    #[inline]
    pub fn unpark(&self) {
        self.shared.token.0.store(NOTIFIED_TOKEN, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};

    use crate::strategy::{TestGatePoint, TestGateStrategy};
    use crate::{BusySpin, SpinThenYield};

    use super::*;

    #[test]
    fn notification_before_park_is_consumed() {
        let (mut parker, unparker) = pair(SpinThenYield::new(0));
        unparker.unpark();
        assert_eq!(parker.park(), ParkResult::Notified);
    }

    #[test]
    fn notifications_coalesce_into_one_token() {
        let (mut parker, unparker) = pair(SpinThenYield::new(0));
        unparker.unpark();
        unparker.unpark();
        assert_eq!(parker.park(), ParkResult::Notified);
        assert_eq!(parker.park(), ParkResult::Unclassified);
    }

    #[test]
    fn filtered_park_ignores_unclassified_wakes() {
        let (mut parker, unparker) = pair(SpinThenYield::new(0));
        let started = Arc::new(Barrier::new(2));
        let consumer_started = Arc::clone(&started);
        let consumer = std::thread::spawn(move || {
            consumer_started.wait();
            parker.park_until_notified();
        });

        started.wait();
        unparker.unpark();
        assert!(consumer.join().is_ok());
    }

    #[test]
    fn notification_synchronizes_published_state() {
        let published = Arc::new(AtomicBool::new(false));
        let producer_value = Arc::clone(&published);
        let (mut parker, unparker) = pair(BusySpin);
        let producer = std::thread::spawn(move || {
            producer_value.store(true, Ordering::Relaxed);
            unparker.unpark();
        });

        parker.park_until_notified();
        assert!(published.load(Ordering::Relaxed));
        assert!(producer.join().is_ok());
    }

    #[test]
    fn notifications_are_not_lost_in_any_arm_wait_window() {
        for point in [
            TestGatePoint::AfterArmBeforeRecheck,
            TestGatePoint::AfterRecheckBeforeWait,
            TestGatePoint::DuringWait,
        ] {
            let reached = Arc::new(Barrier::new(2));
            let released = Arc::new(Barrier::new(2));
            let strategy =
                TestGateStrategy::new(point, Arc::clone(&reached), Arc::clone(&released));
            let (mut parker, unparker) = pair(strategy);
            let consumer = std::thread::spawn(move || parker.park());

            reached.wait();
            unparker.unpark();
            released.wait();
            match consumer.join() {
                Ok(result) => assert_eq!(result, ParkResult::Notified),
                Err(_) => panic!("consumer thread panicked"),
            }
        }
    }

    #[test]
    fn zero_timeout_consumes_a_ready_token_first() {
        let (mut parker, unparker) = pair(BusySpin);
        unparker.unpark();
        assert_eq!(
            parker.park_until_notified_timeout(Duration::ZERO),
            NotificationTimeoutResult::Notified
        );
        assert_eq!(
            parker.park_until_notified_timeout(Duration::ZERO),
            NotificationTimeoutResult::TimedOut
        );
    }
}
