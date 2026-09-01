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
fn notifications_from_multiple_producers_still_form_one_token() {
    let (mut parker, unparker) = pair(SpinThenYield::new(0));
    let first = unparker.clone();
    let second = unparker.clone();
    let first_producer = std::thread::spawn(move || first.unpark());
    let second_producer = std::thread::spawn(move || second.unpark());

    assert!(first_producer.join().is_ok());
    assert!(second_producer.join().is_ok());
    assert_eq!(parker.park(), ParkResult::Notified);
    assert_eq!(parker.park(), ParkResult::Unclassified);
}
