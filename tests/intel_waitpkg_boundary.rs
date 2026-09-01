#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

#[path = "../src/arch/x86_64/intel.rs"]
#[allow(dead_code)]
mod intel;

use intel::{
    INTEL_VENDOR_EBX, INTEL_VENDOR_ECX, INTEL_VENDOR_EDX, INVARIANT_TSC_BIT,
    IntelCapabilityRegisters, RDTSCP_BIT, UMWAIT_C01_CONTROL, WAITPKG_BIT, collect_tsc_frequencies,
    conservative_timer_hz, decode_capabilities, extended_leaf_availability, frequency_hz,
    split_tsc_deadline, timer_calibration_is_usable, tsc_frequency_from_observation,
    tsc_from_halves, waitpkg_leaf_is_available,
};

fn supported_registers() -> IntelCapabilityRegisters {
    IntelCapabilityRegisters {
        vendor_ebx: INTEL_VENDOR_EBX,
        vendor_edx: INTEL_VENDOR_EDX,
        vendor_ecx: INTEL_VENDOR_ECX,
        waitpkg_features_ecx: Some(WAITPKG_BIT),
        extended_features_edx: Some(RDTSCP_BIT),
        invariant_features_edx: Some(INVARIANT_TSC_BIT),
    }
}

#[test]
fn intel_capability_decoding_requires_each_exact_register_bit() {
    assert_eq!(WAITPKG_BIT, 0x0000_0020);
    assert_eq!(RDTSCP_BIT, 0x0800_0000);
    assert_eq!(INVARIANT_TSC_BIT, 0x0000_0100);

    let supported = decode_capabilities(supported_registers(), Some(2_400_000_000));
    assert!(supported.intel_vendor);
    assert!(supported.waitpkg);
    assert!(supported.invariant_tsc);
    assert!(supported.rdtscp);
    assert_eq!(supported.timer_hz, Some(2_400_000_000));
    assert!(timer_calibration_is_usable(&supported));

    for registers in [
        IntelCapabilityRegisters {
            vendor_ebx: 0,
            ..supported_registers()
        },
        IntelCapabilityRegisters {
            vendor_edx: 0,
            ..supported_registers()
        },
        IntelCapabilityRegisters {
            vendor_ecx: 0,
            ..supported_registers()
        },
    ] {
        let not_intel = decode_capabilities(registers, Some(2_400_000_000));
        assert!(!not_intel.intel_vendor);
        assert_eq!(not_intel.timer_hz, None);
        assert!(!timer_calibration_is_usable(&not_intel));
    }

    for registers in [
        IntelCapabilityRegisters {
            waitpkg_features_ecx: None,
            ..supported_registers()
        },
        IntelCapabilityRegisters {
            waitpkg_features_ecx: Some(0),
            ..supported_registers()
        },
        IntelCapabilityRegisters {
            extended_features_edx: None,
            ..supported_registers()
        },
        IntelCapabilityRegisters {
            extended_features_edx: Some(0),
            ..supported_registers()
        },
        IntelCapabilityRegisters {
            invariant_features_edx: None,
            ..supported_registers()
        },
        IntelCapabilityRegisters {
            invariant_features_edx: Some(0),
            ..supported_registers()
        },
    ] {
        let missing_one = decode_capabilities(registers, Some(2_400_000_000));
        assert_eq!(missing_one.timer_hz, None);
        assert!(!timer_calibration_is_usable(&missing_one));
    }
}

#[test]
fn tsc_calibration_is_conservative_and_rejects_unstable_or_migrated_samples() {
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
    assert_eq!(attempts, 9);

    assert_eq!(
        tsc_frequency_from_observation(100, 7, 124, 7, 10),
        Some(2_400_000_000)
    );
    assert_eq!(tsc_frequency_from_observation(100, 7, 124, 8, 10), None);
    assert_eq!(tsc_frequency_from_observation(124, 7, 100, 7, 10), None);
    assert_eq!(tsc_frequency_from_observation(100, 7, 100, 7, 10), None);
    assert_eq!(tsc_frequency_from_observation(100, 7, 124, 7, 0), None);

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

#[test]
fn cpuid_leaf_boundaries_are_exact() {
    assert!(!waitpkg_leaf_is_available(6));
    assert!(waitpkg_leaf_is_available(7));
    assert_eq!(extended_leaf_availability(0x8000_0000), (false, false));
    assert_eq!(extended_leaf_availability(0x8000_0001), (true, false));
    assert_eq!(extended_leaf_availability(0x8000_0007), (true, true));
}

#[test]
fn umwait_operands_preserve_the_absolute_tsc_deadline_and_c01_control() {
    assert_eq!(UMWAIT_C01_CONTROL, 1);
    assert_eq!(
        split_tsc_deadline(0x0123_4567_89ab_cdef),
        (0x89ab_cdef, 0x0123_4567)
    );
    assert_eq!(
        tsc_from_halves(0x0123_4567, 0x89ab_cdef),
        0x0123_4567_89ab_cdef
    );
}

#[test]
fn live_instruction_entry_points_have_the_expected_safe_signatures() {
    std::hint::black_box(intel::detect_capabilities as fn() -> intel::IntelCapabilities);
    std::hint::black_box(intel::read_tsc_ordered as fn() -> (u64, u32));
    std::hint::black_box(intel::umonitor as fn(*const ()));
    std::hint::black_box(intel::umwait_c01 as fn(u64));
}

#[test]
fn instruction_encodings_and_clobber_contract_are_explicit() {
    let source = include_str!("../src/arch/x86_64/intel.rs");
    let monitor_start = source
        .find("pub(crate) fn umonitor")
        .expect("UMONITOR boundary must exist");
    let wait_start = source
        .find("pub(crate) fn umwait_c01")
        .expect("UMWAIT boundary must exist");
    let monitor = &source[monitor_start..wait_start];
    let wait = &source[wait_start..];

    assert!(monitor.contains(".byte 0xf3, 0x0f, 0xae, 0xf0"));
    assert!(monitor.contains("in(\"rax\") address"));
    assert!(wait.contains(".byte 0xf2, 0x0f, 0xae, 0xf1"));
    assert!(wait.contains("in(\"ecx\") UMWAIT_C01_CONTROL"));
    assert!(wait.contains("in(\"eax\") deadline_low"));
    assert!(wait.contains("in(\"edx\") deadline_high"));

    for forbidden in ["nomem", "readonly", "preserves_flags"] {
        assert!(!monitor.contains(forbidden));
        assert!(!wait.contains(forbidden));
    }
}
