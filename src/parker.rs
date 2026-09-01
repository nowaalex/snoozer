#![deny(unsafe_code)]

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::{WaitStrategy, WaitTimeoutResult};

const EMPTY_TOKEN: u32 = 0;
const NOTIFIED_TOKEN: u32 = 1;

macro_rules! publish_token {
    ($token:expr, $notification_bits:expr, $release_ordering:expr) => {
        $token.fetch_or($notification_bits, $release_ordering)
    };
}

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
        self.park_until_notified_timeout_with(timeout, || started.elapsed())
    }

    fn park_until_notified_timeout_with<F>(
        &mut self,
        timeout: Duration,
        mut elapsed: F,
    ) -> NotificationTimeoutResult
    where
        F: FnMut() -> Duration,
    {
        loop {
            let elapsed = elapsed();
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
        // Every producer performs an RMW even when the token is already set.
        // Consecutive RMWs form a release sequence, so the consumer's Acquire
        // CAS synchronizes with every coalesced producer publication rather
        // than only the last producer to overwrite the token.
        publish_token!(self.shared.token.0, NOTIFIED_TOKEN, Ordering::Release);
    }
}

#[cfg(test)]
mod loom_tests {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use loom::thread;

    const FIRST_PRODUCER: u32 = 1 << 0;
    const SECOND_PRODUCER: u32 = 1 << 1;
    const BOTH_PRODUCERS: u32 = FIRST_PRODUCER | SECOND_PRODUCER;

    #[test]
    fn coalesced_release_sequence_carries_every_producer_publication() {
        loom::model(|| {
            let token = Arc::new(AtomicU32::new(0));
            let completed = Arc::new(AtomicU32::new(0));
            let first_payload = Arc::new(AtomicUsize::new(0));
            let second_payload = Arc::new(AtomicUsize::new(0));

            let first_token = Arc::clone(&token);
            let first_completed = Arc::clone(&completed);
            let first_value = Arc::clone(&first_payload);
            let first = thread::spawn(move || {
                first_value.store(11, Ordering::Relaxed);
                publish_token!(first_token, FIRST_PRODUCER, Ordering::Release);
                first_completed.fetch_or(FIRST_PRODUCER, Ordering::Relaxed);
            });

            let second_token = Arc::clone(&token);
            let second_completed = Arc::clone(&completed);
            let second_value = Arc::clone(&second_payload);
            let second = thread::spawn(move || {
                second_value.store(22, Ordering::Relaxed);
                publish_token!(second_token, SECOND_PRODUCER, Ordering::Release);
                second_completed.fetch_or(SECOND_PRODUCER, Ordering::Relaxed);
            });

            // This Relaxed test-only marker guarantees progress but creates no
            // synchronization edge to either payload. Only the token's final
            // Acquire may make both Relaxed publications visible.
            while completed.load(Ordering::Relaxed) != BOTH_PRODUCERS {
                thread::yield_now();
            }

            assert_eq!(
                token.compare_exchange(BOTH_PRODUCERS, 0, Ordering::Acquire, Ordering::Relaxed,),
                Ok(BOTH_PRODUCERS)
            );
            assert_eq!(first_payload.load(Ordering::Relaxed), 11);
            assert_eq!(second_payload.load(Ordering::Relaxed), 22);

            assert!(first.join().is_ok());
            assert!(second.join().is_ok());
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};

    use crate::strategy::{TestBudgetStrategy, TestGatePoint, TestGateStrategy};
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

    #[test]
    fn filtered_timeout_passes_only_the_remaining_budget() {
        let timeout = Duration::from_millis(50);
        let (strategy, observed_budgets) = TestBudgetStrategy::new();
        let (mut parker, _unparker) = pair(strategy);
        let mut elapsed = [0, 10, 20, 50].into_iter().map(Duration::from_millis);

        assert_eq!(
            parker.park_until_notified_timeout_with(timeout, || {
                elapsed.next().unwrap_or(timeout)
            }),
            NotificationTimeoutResult::TimedOut
        );
        let observed = match observed_budgets.lock() {
            Ok(observed) => observed,
            Err(poisoned) => poisoned.into_inner(),
        };
        assert_eq!(
            observed.as_slice(),
            [
                Duration::from_millis(50),
                Duration::from_millis(40),
                Duration::from_millis(30),
            ]
        );
        assert!(observed.windows(2).all(|window| window[1] < window[0]));
    }
}
