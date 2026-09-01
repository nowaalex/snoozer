use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use snoozer::{BusySpin, NotificationTimeoutResult, ParkResult, SpinThenYield, pair};

#[test]
fn early_notification_is_not_lost_and_duplicates_coalesce() {
    let (mut parker, unparker) = pair(SpinThenYield::new(0));
    unparker.unpark();
    unparker.unpark();

    assert_eq!(parker.park(), ParkResult::Notified);
    assert_eq!(parker.park(), ParkResult::Unclassified);
}

#[test]
fn notification_during_wait_is_not_lost() {
    let published = Arc::new(AtomicUsize::new(0));
    let consumer_state = Arc::clone(&published);
    let (mut parker, unparker) = pair(BusySpin);

    let consumer = std::thread::spawn(move || {
        parker.park_until_notified();
        consumer_state.load(Ordering::Acquire)
    });
    published.store(7, Ordering::Release);
    unparker.unpark();

    match consumer.join() {
        Ok(value) => assert_eq!(value, 7),
        Err(_) => panic!("consumer thread panicked"),
    }
}

#[test]
fn timeout_does_not_consume_a_later_notification() {
    let (mut parker, unparker) = pair(SpinThenYield::new(0));
    assert_eq!(
        parker.park_until_notified_timeout(Duration::ZERO),
        NotificationTimeoutResult::TimedOut
    );

    unparker.unpark();
    assert_eq!(parker.park(), ParkResult::Notified);
}

#[test]
fn coalesced_token_acquires_every_concurrent_producer_publication() {
    let (mut parker, unparker) = pair(SpinThenYield::new(0));
    let published = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
    let completed = Arc::new(AtomicUsize::new(0));
    let first = unparker.clone();
    let second = unparker.clone();
    let first_published = Arc::clone(&published);
    let second_published = Arc::clone(&published);
    let first_completed = Arc::clone(&completed);
    let second_completed = Arc::clone(&completed);
    let first_producer = std::thread::spawn(move || {
        first_published[0].store(11, Ordering::Relaxed);
        first.unpark();
        first_completed.fetch_add(1, Ordering::Relaxed);
    });
    let second_producer = std::thread::spawn(move || {
        second_published[1].store(22, Ordering::Relaxed);
        second.unpark();
        second_completed.fetch_add(1, Ordering::Relaxed);
    });

    // Relaxed completion polling establishes no publication happens-before
    // edge. The only Acquire that may expose both payloads is token
    // consumption, and thread joins deliberately happen after the assertions.
    while completed.load(Ordering::Relaxed) != 2 {
        std::hint::spin_loop();
    }

    assert_eq!(parker.park(), ParkResult::Notified);
    assert_eq!(published[0].load(Ordering::Relaxed), 11);
    assert_eq!(published[1].load(Ordering::Relaxed), 22);
    assert_eq!(parker.park(), ParkResult::Unclassified);

    assert!(first_producer.join().is_ok());
    assert!(second_producer.join().is_ok());
}
