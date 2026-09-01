use super::*;

#[test]
fn atomic_loads_and_monitored_addresses_are_exact() {
    let small = AtomicU32::new(17);
    let large = AtomicU64::new(29);

    assert_eq!(small.__load_acquire(), 17);
    assert_eq!(large.__load_acquire(), 29);
    assert_eq!(
        small.__monitored_address(),
        small.as_ptr().cast_const().cast()
    );
    assert_eq!(
        large.__monitored_address(),
        large.as_ptr().cast_const().cast()
    );
}
