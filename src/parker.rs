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
///
/// ```
/// use snoozer::{BusySpin, single_pair};
///
/// let (mut parker, mut unparker) = single_pair(BusySpin);
/// unparker.unpark();
/// parker.park_until_notified();
/// ```
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
///
/// ```
/// use snoozer::{BusySpin, multi_pair};
///
/// let (mut parker, unparker) = multi_pair(BusySpin);
/// let another_producer = unparker.clone();
/// another_producer.unpark();
/// parker.park_until_notified();
/// ```
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
            ///
            /// ```
            /// use snoozer::{BusySpin, single_pair};
            ///
            /// let (parker, _) = single_pair(BusySpin);
            /// assert_eq!(*parker.strategy(), BusySpin);
            /// ```
            #[must_use]
            pub const fn strategy(&self) -> &S {
                &self.core.strategy
            }

            /// Consumes a ready token or performs one raw wait attempt.
            ///
            /// [`ParkResult::Unclassified`] does not prove that work is
            /// available and does not synchronize with a producer.
            /// Application state must be rechecked with an Acquire operation.
            ///
            /// ```
            /// use snoozer::{BusySpin, ParkResult, single_pair};
            ///
            /// let (mut parker, mut unparker) = single_pair(BusySpin);
            /// unparker.unpark();
            /// assert_eq!(parker.park(), ParkResult::Notified);
            /// ```
            #[must_use]
            pub fn park(&mut self) -> ParkResult {
                self.core.park()
            }

            /// Absorbs unclassified wakes and returns only after consuming a
            /// notification token.
            ///
            /// ```
            /// use snoozer::{BusySpin, single_pair};
            ///
            /// let (mut parker, mut unparker) = single_pair(BusySpin);
            /// unparker.unpark();
            /// parker.park_until_notified();
            /// ```
            pub fn park_until_notified(&mut self) {
                self.core.park_until_notified();
            }

            /// Consumes a ready token or performs one raw wait attempt bounded
            /// by `timeout`.
            ///
            /// ```
            /// use snoozer::{BusySpin, ParkTimeoutResult, single_pair};
            /// use std::time::Duration;
            ///
            /// let (mut parker, mut unparker) = single_pair(BusySpin);
            /// unparker.unpark();
            /// assert_eq!(parker.park_timeout(Duration::ZERO), ParkTimeoutResult::Notified);
            /// ```
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
            ///
            /// ```
            /// use snoozer::{BusySpin, NotificationTimeoutResult, single_pair};
            /// use std::time::Duration;
            ///
            /// let (mut parker, mut unparker) = single_pair(BusySpin);
            /// unparker.unpark();
            /// assert_eq!(
            ///     parker.park_until_notified_timeout(Duration::ZERO),
            ///     NotificationTimeoutResult::Notified,
            /// );
            /// ```
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
    ///
    /// ```
    /// use snoozer::{BusySpin, single_pair};
    ///
    /// let (_, mut unparker) = single_pair(BusySpin);
    /// unparker.unpark();
    /// ```
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
    ///
    /// ```
    /// use snoozer::{BusySpin, multi_pair};
    ///
    /// let (_, unparker) = multi_pair(BusySpin);
    /// unparker.unpark();
    /// ```
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
#[path = "../tests/loom/parker_publication_ordering.rs"]
mod parker_publication_ordering;

#[cfg(test)]
#[path = "../tests/unit/parker_notification_protocol.rs"]
mod parker_notification_protocol;
