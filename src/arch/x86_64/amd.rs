//! The only production boundary that contains inline assembly or unsafe code.

#![allow(unsafe_code)]

use std::arch::asm;
use std::arch::x86_64::{__cpuid, __cpuid_count};
use std::time::{Duration, Instant};

use crate::Capabilities;

const AMD_VENDOR_EBX: u32 = u32::from_le_bytes(*b"Auth");
const AMD_VENDOR_EDX: u32 = u32::from_le_bytes(*b"enti");
const AMD_VENDOR_ECX: u32 = u32::from_le_bytes(*b"cAMD");
const EXTENDED_CPUID_BASE: u32 = 0x8000_0000;
const EXTENDED_FEATURES_LEAF: u32 = 0x8000_0001;
const INVARIANT_TSC_LEAF: u32 = 0x8000_0007;
const MONITORX_MWAITX_BIT: u32 = 1 << 29;
const RDTSCP_BIT: u32 = 1 << 27;
const INVARIANT_TSC_BIT: u32 = 1 << 8;
const TIMER_CALIBRATION_INTERVAL: Duration = Duration::from_millis(2);
const TIMER_CALIBRATION_SAMPLES: usize = 3;
const TIMER_CALIBRATION_ATTEMPTS: usize = 9;
const MAX_CALIBRATION_SPREAD_PERCENT: u64 = 20;
const CONSERVATIVE_FREQUENCY_PERCENT: u64 = 80;

pub(crate) fn detect_capabilities() -> Capabilities {
    let vendor = __cpuid(0);
    let amd_vendor = vendor.ebx == AMD_VENDOR_EBX
        && vendor.edx == AMD_VENDOR_EDX
        && vendor.ecx == AMD_VENDOR_ECX;

    let extended_max = __cpuid(EXTENDED_CPUID_BASE).eax;
    let extended_features =
        (extended_max >= EXTENDED_FEATURES_LEAF).then(|| __cpuid_count(EXTENDED_FEATURES_LEAF, 0));
    let invariant_features =
        (extended_max >= INVARIANT_TSC_LEAF).then(|| __cpuid_count(INVARIANT_TSC_LEAF, 0));

    let monitorx_mwaitx =
        extended_features.is_some_and(|features| features.ecx & MONITORX_MWAITX_BIT != 0);
    let rdtscp = extended_features.is_some_and(|features| features.edx & RDTSCP_BIT != 0);
    let invariant_tsc =
        invariant_features.is_some_and(|features| features.edx & INVARIANT_TSC_BIT != 0);
    let mwaitx_timer_hz = if amd_vendor && monitorx_mwaitx && invariant_tsc && rdtscp {
        calibrate_tsc_lower_bound()
    } else {
        None
    };

    Capabilities {
        supported_target: true,
        amd_vendor,
        monitorx_mwaitx,
        invariant_tsc,
        rdtscp,
        mwaitx_timer_hz,
    }
}

fn calibrate_tsc_lower_bound() -> Option<u64> {
    let mut samples = [0_u64; TIMER_CALIBRATION_SAMPLES];
    let mut collected = 0;
    for _ in 0..TIMER_CALIBRATION_ATTEMPTS {
        let started = Instant::now();
        let (first, first_aux) = read_tsc_ordered();
        while started.elapsed() < TIMER_CALIBRATION_INTERVAL {
            std::hint::spin_loop();
        }
        let (last, last_aux) = read_tsc_ordered();
        let elapsed = started.elapsed();
        if first_aux != last_aux {
            continue;
        }

        let ticks = last.saturating_sub(first);
        let nanos = elapsed.as_nanos().max(1);
        let frequency = u128::from(ticks).saturating_mul(1_000_000_000) / nanos;
        samples[collected] = u64::try_from(frequency).unwrap_or(u64::MAX).max(1);
        collected += 1;
        if collected == TIMER_CALIBRATION_SAMPLES {
            break;
        }
    }

    if collected != TIMER_CALIBRATION_SAMPLES {
        return None;
    }
    samples.sort_unstable();
    let minimum = samples[0];
    let maximum = samples[TIMER_CALIBRATION_SAMPLES - 1];
    if maximum.saturating_sub(minimum).saturating_mul(100)
        > maximum.saturating_mul(MAX_CALIBRATION_SPREAD_PERCENT)
    {
        return None;
    }
    Some(minimum.saturating_mul(CONSERVATIVE_FREQUENCY_PERCENT) / 100)
}

#[inline]
fn read_tsc_ordered() -> (u64, u32) {
    let low: u32;
    let high: u32;
    let auxiliary: u32;
    // SAFETY: capability detection calls calibration only after CPUID reports
    // RDTSCP and invariant TSC. LFENCE prevents later work moving before the
    // sample, and TSC_AUX lets calibration discard migrated samples.
    unsafe {
        asm!(
            "rdtscp",
            "lfence",
            lateout("eax") low,
            lateout("edx") high,
            lateout("ecx") auxiliary,
            options(nomem, nostack)
        );
    }
    ((u64::from(high) << 32) | u64::from(low), auxiliary)
}

#[inline]
pub(crate) fn monitorx(address: *const ()) {
    // SAFETY: AmdMwaitx construction has checked CPUID. The caller passes a
    // live AtomicU32 or AtomicU64 address and keeps it alive across MWAITX.
    unsafe {
        asm!(
            ".byte 0x0f, 0x01, 0xfa",
            in("rax") address,
            in("ecx") 0_u32,
            in("edx") 0_u32,
            options(nostack)
        );
    }
}

#[inline]
pub(crate) fn mwaitx(timeout_cycles: u32, c_state_hint: u32) {
    // LLVM may reserve RBX, so a reversible exchange supplies EBX without
    // naming RBX as an operand. EAX=0xF is the production no-C-state hint;
    // EAX=0 is reachable only through the benchmark-only diagnostic type.
    // SAFETY: AmdMwaitx construction checked CPUID and the caller performed
    // MONITORX plus an Acquire recheck. ECX enables the timer and EBX is
    // nonzero, bounding the wait. Both exchanges restore the full RBX value.
    unsafe {
        asm!(
            "xchg {timeout}, rbx",
            ".byte 0x0f, 0x01, 0xfb",
            "xchg {timeout}, rbx",
            timeout = inout(reg) u64::from(timeout_cycles) => _,
            in("eax") c_state_hint,
            in("ecx") 1_u32 << 1,
            options(nostack)
        );
    }
}
