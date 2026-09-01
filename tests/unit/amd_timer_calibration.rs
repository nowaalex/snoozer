use super::*;

#[test]
fn calibration_collection_and_tsc_observations_are_pure_and_exact() {
    let mut observations = [None, Some(10), None, Some(20), Some(30)].into_iter();
    assert_eq!(
        collect_tsc_frequencies(|| observations.next().flatten()),
        Some([10, 20, 30])
    );

    let mut attempts = 0_usize;
    assert_eq!(
        collect_tsc_frequencies(|| {
            attempts += 1;
            (attempts <= 2).then_some(attempts as u64)
        }),
        None
    );
    assert_eq!(attempts, TIMER_CALIBRATION_ATTEMPTS);

    assert_eq!(
        tsc_frequency_from_observation(100, 7, 124, 7, 10),
        Some(2_400_000_000)
    );
    assert_eq!(tsc_frequency_from_observation(100, 7, 124, 8, 10), None);
    assert_eq!(tsc_frequency_from_observation(124, 7, 100, 7, 10), None);
    assert_eq!(tsc_frequency_from_observation(100, 7, 100, 7, 10), None);
    assert_eq!(tsc_frequency_from_observation(100, 7, 124, 7, 0), None);

    assert_eq!(
        tsc_from_halves(0x0123_4567, 0x89ab_cdef),
        0x0123_4567_89ab_cdef
    );
    assert_eq!(tsc_from_halves(0, u32::MAX), u64::from(u32::MAX));
}

#[test]
fn calibration_math_is_scaled_conservative_and_rejects_instability() {
    assert_eq!(frequency_hz(24, 10), 2_400_000_000);
    assert_eq!(frequency_hz(0, 0), 1);
    assert_eq!(
        conservative_timer_hz([2_400_000_000, 2_500_000_000, 2_450_000_000]),
        Some(1_920_000_000)
    );
    assert_eq!(conservative_timer_hz([1_000, 1_000, 2_000]), None);
    assert_eq!(conservative_timer_hz([800, 900, 1_000]), Some(640));
    assert_eq!(conservative_timer_hz([1, 1, 1]), None);
}
