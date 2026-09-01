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

#[derive(Clone, Copy)]
struct CapabilityRegisters {
    vendor_ebx: u32,
    vendor_edx: u32,
    vendor_ecx: u32,
    extended_features_ecx: Option<u32>,
    extended_features_edx: Option<u32>,
    invariant_features_edx: Option<u32>,
}

pub(crate) fn detect_capabilities() -> Capabilities {
    let vendor = __cpuid(0);
    let extended_max = __cpuid(EXTENDED_CPUID_BASE).eax;
    let (has_extended_features, has_invariant_features) = extended_leaf_availability(extended_max);
    let extended_features = has_extended_features.then(|| __cpuid_count(EXTENDED_FEATURES_LEAF, 0));
    let invariant_features = has_invariant_features.then(|| __cpuid_count(INVARIANT_TSC_LEAF, 0));
    let registers = CapabilityRegisters {
        vendor_ebx: vendor.ebx,
        vendor_edx: vendor.edx,
        vendor_ecx: vendor.ecx,
        extended_features_ecx: extended_features.map(|features| features.ecx),
        extended_features_edx: extended_features.map(|features| features.edx),
        invariant_features_edx: invariant_features.map(|features| features.edx),
    };
    decode_capabilities(registers)
}

pub(crate) fn calibrate_timer_hz() -> Option<u64> {
    let detected = detect_capabilities();
    if timer_calibration_is_usable(&detected) {
        collect_tsc_frequencies(sample_tsc_frequency_once).and_then(conservative_timer_hz)
    } else {
        None
    }
}

fn extended_leaf_availability(extended_max: u32) -> (bool, bool) {
    (
        extended_max >= EXTENDED_FEATURES_LEAF,
        extended_max >= INVARIANT_TSC_LEAF,
    )
}

fn decode_capabilities(registers: CapabilityRegisters) -> Capabilities {
    let amd_vendor = registers.vendor_ebx == AMD_VENDOR_EBX
        && registers.vendor_edx == AMD_VENDOR_EDX
        && registers.vendor_ecx == AMD_VENDOR_ECX;
    let monitorx_mwaitx = registers
        .extended_features_ecx
        .is_some_and(|features| features & MONITORX_MWAITX_BIT != 0);
    let rdtscp = registers
        .extended_features_edx
        .is_some_and(|features| features & RDTSCP_BIT != 0);
    let invariant_tsc = registers
        .invariant_features_edx
        .is_some_and(|features| features & INVARIANT_TSC_BIT != 0);
    Capabilities {
        supported_target: true,
        amd_vendor,
        intel_vendor: false,
        monitorx_mwaitx,
        waitpkg: false,
        invariant_tsc,
        rdtscp,
    }
}

fn timer_calibration_is_usable(capabilities: &Capabilities) -> bool {
    capabilities.amd_vendor
        && capabilities.monitorx_mwaitx
        && capabilities.invariant_tsc
        && capabilities.rdtscp
}

fn collect_tsc_frequencies<F>(mut sample_once: F) -> Option<[u64; TIMER_CALIBRATION_SAMPLES]>
where
    F: FnMut() -> Option<u64>,
{
    let mut samples = [0_u64; TIMER_CALIBRATION_SAMPLES];
    let mut collected = 0;
    for _ in 0..TIMER_CALIBRATION_ATTEMPTS {
        if let Some(sample) = sample_once() {
            samples[collected] = sample;
            collected += 1;
            if collected == TIMER_CALIBRATION_SAMPLES {
                return Some(samples);
            }
        }
    }
    None
}

fn sample_tsc_frequency_once() -> Option<u64> {
    let started = Instant::now();
    let (first, first_aux) = read_tsc_ordered();
    while started.elapsed() < TIMER_CALIBRATION_INTERVAL {
        std::hint::spin_loop();
    }
    let (last, last_aux) = read_tsc_ordered();
    tsc_frequency_from_observation(
        first,
        first_aux,
        last,
        last_aux,
        started.elapsed().as_nanos(),
    )
}

fn tsc_frequency_from_observation(
    first: u64,
    first_aux: u32,
    last: u64,
    last_aux: u32,
    elapsed_nanos: u128,
) -> Option<u64> {
    if first_aux != last_aux || last <= first || elapsed_nanos == 0 {
        return None;
    }
    Some(frequency_hz(last - first, elapsed_nanos))
}

fn frequency_hz(ticks: u64, elapsed_nanos: u128) -> u64 {
    let frequency = u128::from(ticks).saturating_mul(1_000_000_000) / elapsed_nanos.max(1);
    u64::try_from(frequency).unwrap_or(u64::MAX).max(1)
}

fn conservative_timer_hz(mut samples: [u64; TIMER_CALIBRATION_SAMPLES]) -> Option<u64> {
    samples.sort_unstable();
    let minimum = samples[0];
    let maximum = samples[TIMER_CALIBRATION_SAMPLES - 1];
    if maximum.saturating_sub(minimum).saturating_mul(100)
        > maximum.saturating_mul(MAX_CALIBRATION_SPREAD_PERCENT)
    {
        return None;
    }
    let lower_bound = minimum.saturating_mul(CONSERVATIVE_FREQUENCY_PERCENT) / 100;
    (lower_bound > 0).then_some(lower_bound)
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
    (tsc_from_halves(high, low), auxiliary)
}

fn tsc_from_halves(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) + u64::from(low)
}

#[inline]
pub(crate) fn monitorx(address: *const ()) {
    // SAFETY: HardwareWait preflight has checked CPUID. The caller passes a
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
    // naming RBX as an operand. EAX=0xF0 is the production no-C-state hint;
    // EAX=0 is reachable only through the benchmark-only diagnostic type.
    // SAFETY: HardwareWait preflight checked CPUID and the caller performed
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

#[cfg(test)]
#[path = "../../../tests/unit/amd_capability_decoding.rs"]
mod amd_capability_decoding;

#[cfg(test)]
#[path = "../../../tests/unit/amd_timer_calibration.rs"]
mod amd_timer_calibration;
