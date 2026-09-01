#![deny(unsafe_code)]

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use crate::{WaitStrategy, WaitTimeoutResult, WaitUntilTimeoutResult};

const EMPTY_TOKEN: u32 = 0;
const NOTIFIED_TOKEN: u32 = 1;

macro_rules! publish_token {
    ($token:expr, $notification_bits:expr) => {
        $token.fetch_or($notification_bits, Ordering::Release)
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

struct ParkerCore<S> {
    shared: Arc<Shared>,
    strategy: S,
}

/// The consumer end of a single-producer notification pair.
///
/// `Single` describes the producer count. This type always supports exactly
/// one consumer, just like [`MultiParker`]. It is `Send` but intentionally not
/// `Sync`, and every park operation requires exclusive access so the consumer
/// token can never be taken concurrently.
///
/// ```compile_fail
/// use snoozer::{BusySpin, SingleParker};
///
/// fn require_sync<T: Sync>() {}
/// require_sync::<SingleParker<BusySpin>>();
/// ```
pub struct SingleParker<S> {
    core: ParkerCore<S>,
    not_sync: PhantomData<Cell<()>>,
}

/// The sole producer end of a single-producer notification pair.
///
/// This handle is `Send`, but intentionally neither `Sync` nor `Clone`.
/// [`SingleUnparker::unpark`] requires exclusive access, preserving the
/// one-producer contract in safe Rust.
///
/// ```compile_fail
/// use snoozer::{SingleUnparker, single_pair, BusySpin};
///
/// fn require_sync<T: Sync>() {}
/// let (_, _unparker): (_, SingleUnparker) = single_pair(BusySpin);
/// require_sync::<SingleUnparker>();
/// ```
///
/// ```compile_fail
/// use snoozer::{SingleUnparker, single_pair, BusySpin};
///
/// fn require_clone<T: Clone>() {}
/// let (_, _unparker): (_, SingleUnparker) = single_pair(BusySpin);
/// require_clone::<SingleUnparker>();
/// ```
///
/// ```compile_fail
/// use snoozer::{BusySpin, single_pair};
///
/// let (_, unparker) = single_pair(BusySpin);
/// unparker.unpark(); // The sole producer handle requires exclusive access.
/// ```
pub struct SingleUnparker {
    shared: Arc<Shared>,
    not_sync: PhantomData<Cell<()>>,
}

/// The consumer end of a multi-producer notification pair.
///
/// `Multi` describes the producer count, never the consumer count. There is
/// still exactly one consumer. This type is `Send` but intentionally not
/// `Sync`, and every park operation requires exclusive access.
///
/// ```compile_fail
/// use snoozer::{BusySpin, MultiParker};
///
/// fn require_sync<T: Sync>() {}
/// require_sync::<MultiParker<BusySpin>>();
/// ```
pub struct MultiParker<S> {
    core: ParkerCore<S>,
    not_sync: PhantomData<Cell<()>>,
}

/// A producer end of a multi-producer notification pair.
///
/// This handle is `Send`, `Sync`, and `Clone`, allowing any number of producer
/// threads. `Multi` never permits multiple consumers; [`MultiParker`] remains
/// a single-consumer handle.
#[derive(Clone)]
pub struct MultiUnparker {
    shared: Arc<Shared>,
}

/// Creates a pair with exactly one consumer and exactly one producer.
///
/// The producer handle is deliberately not clonable or shareable. Use
/// [`multi_pair`] when more than one producer must publish notifications.
#[must_use]
pub fn single_pair<S: WaitStrategy>(strategy: S) -> (SingleParker<S>, SingleUnparker) {
    let shared = Arc::new(Shared {
        token: CacheLineToken(AtomicU32::new(EMPTY_TOKEN)),
    });
    (
        SingleParker {
            core: ParkerCore {
                shared: Arc::clone(&shared),
                strategy,
            },
            not_sync: PhantomData,
        },
        SingleUnparker {
            shared,
            not_sync: PhantomData,
        },
    )
}

/// Creates a pair with exactly one consumer and any number of producers.
///
/// `Multi` refers only to producers. Cloning the returned [`MultiUnparker`]
/// adds producers; it does not make [`MultiParker`] safe for multiple
/// consumers.
#[must_use]
pub fn multi_pair<S: WaitStrategy>(strategy: S) -> (MultiParker<S>, MultiUnparker) {
    let shared = Arc::new(Shared {
        token: CacheLineToken(AtomicU32::new(EMPTY_TOKEN)),
    });
    (
        MultiParker {
            core: ParkerCore {
                shared: Arc::clone(&shared),
                strategy,
            },
            not_sync: PhantomData,
        },
        MultiUnparker { shared },
    )
}

impl<S: WaitStrategy> ParkerCore<S> {
    fn park(&mut self) -> ParkResult {
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

    fn park_until_notified(&mut self) {
        if self.take_notification() {
            return;
        }

        let _observed = self
            .strategy
            .wait_until_different(&self.shared.token.0, EMPTY_TOKEN);
        // This core has exactly one consumer, and producers only change the
        // token from EMPTY_TOKEN to NOTIFIED_TOKEN. Once the filtered wait has
        // observed a change, no other safe caller can consume it first.
        let consumed = self.take_notification();
        debug_assert!(consumed);
    }

    fn park_timeout(&mut self, timeout: Duration) -> ParkTimeoutResult {
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

    fn park_until_notified_timeout(&mut self, timeout: Duration) -> NotificationTimeoutResult {
        if self.take_notification() {
            return NotificationTimeoutResult::Notified;
        }

        match self
            .strategy
            .wait_until_different_timeout(&self.shared.token.0, EMPTY_TOKEN, timeout)
        {
            WaitUntilTimeoutResult::Changed(_observed) => {
                // The single-consumer invariant makes this token exclusively
                // ours after the filtered wait observes it.
                let consumed = self.take_notification();
                debug_assert!(consumed);
                NotificationTimeoutResult::Notified
            }
            WaitUntilTimeoutResult::TimedOut => {
                if self.take_notification() {
                    NotificationTimeoutResult::Notified
                } else {
                    NotificationTimeoutResult::TimedOut
                }
            }
        }
    }

    #[inline]
    fn take_notification(&self) -> bool {
        if self.shared.token.0.load(Ordering::Relaxed) != NOTIFIED_TOKEN {
            return false;
        }

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

macro_rules! impl_parker {
    ($parker:ident) => {
        impl<S: WaitStrategy> $parker<S> {
            /// Returns the configured strategy.
            #[must_use]
            pub const fn strategy(&self) -> &S {
                &self.core.strategy
            }

            /// Consumes a ready token or performs one raw wait attempt.
            ///
            /// [`ParkResult::Unclassified`] does not prove that work is
            /// available and does not synchronize with a producer.
            /// Application state must be rechecked with an Acquire operation.
            #[must_use]
            pub fn park(&mut self) -> ParkResult {
                self.core.park()
            }

            /// Absorbs unclassified wakes and returns only after consuming a
            /// notification token.
            pub fn park_until_notified(&mut self) {
                self.core.park_until_notified();
            }

            /// Consumes a ready token or performs one raw wait attempt bounded
            /// by `timeout`.
            #[must_use]
            pub fn park_timeout(&mut self, timeout: Duration) -> ParkTimeoutResult {
                self.core.park_timeout(timeout)
            }

            /// Absorbs unclassified wakes until a token is consumed or
            /// `timeout` expires.
            ///
            /// A token already present at entry wins over a zero timeout. If a
            /// producer publishes at the timeout boundary, one final token
            /// check decides the result without losing that notification.
            #[must_use]
            pub fn park_until_notified_timeout(
                &mut self,
                timeout: Duration,
            ) -> NotificationTimeoutResult {
                self.core.park_until_notified_timeout(timeout)
            }
        }
    };
}

impl_parker!(SingleParker);
impl_parker!(MultiParker);

impl SingleUnparker {
    /// Publishes one notification token with Release ordering.
    ///
    /// Multiple calls before the consumer takes the token coalesce into one.
    #[inline]
    pub fn unpark(&mut self) {
        self.shared.token.0.store(NOTIFIED_TOKEN, Ordering::Release);
    }
}

impl MultiUnparker {
    /// Publishes one notification token with Release ordering.
    ///
    /// Concurrent calls from cloned handles coalesce into one token. The
    /// atomic read-modify-write operations form a release sequence, so the
    /// consumer's Acquire operation observes state published by every
    /// producer whose notification has coalesced into that token.
    #[inline]
    pub fn unpark(&self) {
        // Every producer performs an RMW even when the token is already set.
        // Consecutive RMWs form a release sequence, so the consumer's Acquire
        // CAS synchronizes with every coalesced producer publication rather
        // than only the last producer to overwrite the token.
        publish_token!(self.shared.token.0, NOTIFIED_TOKEN);
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
    fn single_producer_store_publishes_before_notification() {
        loom::model(|| {
            let token = Arc::new(AtomicU32::new(0));
            let payload = Arc::new(AtomicUsize::new(0));

            let producer_token = Arc::clone(&token);
            let producer_payload = Arc::clone(&payload);
            let producer = thread::spawn(move || {
                producer_payload.store(17, Ordering::Relaxed);
                producer_token.store(1, Ordering::Release);
            });

            while token
                .compare_exchange(1, 0, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                thread::yield_now();
            }
            assert_eq!(payload.load(Ordering::Relaxed), 17);
            assert!(producer.join().is_ok());
        });
    }

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
                publish_token!(first_token, 1);
                first_completed.fetch_or(FIRST_PRODUCER, Ordering::Relaxed);
            });

            let second_token = Arc::clone(&token);
            let second_completed = Arc::clone(&completed);
            let second_value = Arc::clone(&second_payload);
            let second = thread::spawn(move || {
                second_value.store(22, Ordering::Relaxed);
                publish_token!(second_token, 1);
                second_completed.fetch_or(SECOND_PRODUCER, Ordering::Relaxed);
            });

            // This Relaxed test-only marker guarantees progress but creates no
            // synchronization edge to either payload. Only the token's final
            // Acquire may make both Relaxed publications visible.
            while completed.load(Ordering::Relaxed) != BOTH_PRODUCERS {
                thread::yield_now();
            }

            assert_eq!(
                token.compare_exchange(1, 0, Ordering::Acquire, Ordering::Relaxed,),
                Ok(1)
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

    use crate::strategy::{TestGatePoint, TestGateStrategy, TestTimeoutGateStrategy};
    use crate::{BusySpin, SpinThenYield};

    use super::*;

    #[test]
    fn single_notification_before_park_is_consumed() {
        let (mut parker, mut unparker) = single_pair(SpinThenYield::new(0));
        unparker.unpark();
        assert_eq!(parker.park(), ParkResult::Notified);
    }

    #[test]
    fn single_notifications_coalesce_into_one_token() {
        let (mut parker, mut unparker) = single_pair(SpinThenYield::new(0));
        unparker.unpark();
        unparker.unpark();
        assert_eq!(parker.park(), ParkResult::Notified);
        assert_eq!(parker.park(), ParkResult::Unclassified);
    }

    #[test]
    fn multi_notifications_coalesce_into_one_token() {
        let (mut parker, unparker) = multi_pair(SpinThenYield::new(0));
        unparker.unpark();
        unparker.unpark();
        assert_eq!(parker.park(), ParkResult::Notified);
        assert_eq!(parker.park(), ParkResult::Unclassified);
    }

    #[test]
    fn filtered_park_ignores_unclassified_wakes() {
        let (mut parker, mut unparker) = single_pair(SpinThenYield::new(0));
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
        let (mut parker, mut unparker) = single_pair(BusySpin);
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
            let (mut parker, mut unparker) = single_pair(strategy);
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
        let (mut parker, mut unparker) = single_pair(BusySpin);
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
    fn final_notification_wins_at_filtered_timeout_boundary() {
        let reached = Arc::new(Barrier::new(2));
        let released = Arc::new(Barrier::new(2));
        let strategy = TestTimeoutGateStrategy::new(Arc::clone(&reached), Arc::clone(&released));
        let (mut parker, mut unparker) = single_pair(strategy);
        let consumer = std::thread::spawn(move || {
            parker.park_until_notified_timeout(Duration::from_millis(1))
        });

        reached.wait();
        unparker.unpark();
        released.wait();
        match consumer.join() {
            Ok(result) => assert_eq!(result, NotificationTimeoutResult::Notified),
            Err(_) => panic!("consumer thread panicked"),
        }
    }

    #[test]
    fn public_handles_have_the_positive_auto_traits() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        fn assert_clone<T: Clone>() {}

        assert_send::<SingleParker<BusySpin>>();
        assert_send::<SingleUnparker>();
        assert_send::<MultiParker<BusySpin>>();
        assert_send::<MultiUnparker>();
        assert_sync::<MultiUnparker>();
        assert_clone::<MultiUnparker>();
    }
}
