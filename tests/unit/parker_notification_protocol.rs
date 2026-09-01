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
        let strategy = TestGateStrategy::new(point, Arc::clone(&reached), Arc::clone(&released));
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
    let consumer =
        std::thread::spawn(move || parker.park_until_notified_timeout(Duration::from_millis(1)));

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
