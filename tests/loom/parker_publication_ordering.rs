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
