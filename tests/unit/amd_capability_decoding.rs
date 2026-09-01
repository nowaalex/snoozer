use super::*;

fn supported_registers() -> CapabilityRegisters {
    CapabilityRegisters {
        vendor_ebx: AMD_VENDOR_EBX,
        vendor_edx: AMD_VENDOR_EDX,
        vendor_ecx: AMD_VENDOR_ECX,
        extended_features_ecx: Some(MONITORX_MWAITX_BIT),
        extended_features_edx: Some(RDTSCP_BIT),
        invariant_features_edx: Some(INVARIANT_TSC_BIT),
    }
}

#[test]
fn capability_decoding_requires_every_exact_register_bit() {
    assert_eq!(MONITORX_MWAITX_BIT, 0x2000_0000);
    assert_eq!(RDTSCP_BIT, 0x0800_0000);
    assert_eq!(INVARIANT_TSC_BIT, 0x0000_0100);

    let supported = decode_capabilities(supported_registers());
    assert!(supported.supported_target);
    assert!(supported.amd_vendor);
    assert!(supported.monitorx_mwaitx);
    assert!(supported.invariant_tsc);
    assert!(supported.rdtscp);
    assert!(timer_calibration_is_usable(&supported));

    let not_amd = decode_capabilities(CapabilityRegisters {
        vendor_ebx: 0,
        ..supported_registers()
    });
    assert!(!not_amd.amd_vendor);
    assert!(!timer_calibration_is_usable(&not_amd));

    let missing_features = decode_capabilities(CapabilityRegisters {
        extended_features_ecx: None,
        extended_features_edx: None,
        invariant_features_edx: None,
        ..supported_registers()
    });
    assert!(!missing_features.monitorx_mwaitx);
    assert!(!missing_features.invariant_tsc);
    assert!(!missing_features.rdtscp);

    let zeroed_features = decode_capabilities(CapabilityRegisters {
        extended_features_ecx: Some(0),
        extended_features_edx: Some(0),
        invariant_features_edx: Some(0),
        ..supported_registers()
    });
    assert!(!zeroed_features.monitorx_mwaitx);
    assert!(!zeroed_features.invariant_tsc);
    assert!(!zeroed_features.rdtscp);

    for registers in [
        CapabilityRegisters {
            extended_features_ecx: Some(0),
            ..supported_registers()
        },
        CapabilityRegisters {
            extended_features_edx: Some(0),
            ..supported_registers()
        },
        CapabilityRegisters {
            invariant_features_edx: Some(0),
            ..supported_registers()
        },
    ] {
        let missing_one = decode_capabilities(registers);
        assert!(!timer_calibration_is_usable(&missing_one));
    }
}

#[test]
fn extended_leaf_boundaries_are_interpreted_exactly() {
    assert_eq!(
        extended_leaf_availability(EXTENDED_FEATURES_LEAF - 1),
        (false, false)
    );
    assert_eq!(
        extended_leaf_availability(EXTENDED_FEATURES_LEAF),
        (true, false)
    );
    assert_eq!(extended_leaf_availability(INVARIANT_TSC_LEAF), (true, true));
}
