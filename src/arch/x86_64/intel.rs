//! Intel WAITPKG capability detection and instruction boundary.

#![allow(unsafe_code)]

use std::arch::asm;
use std::arch::x86_64::{__cpuid, __cpuid_count};
use std::time::{Duration, Instant};

pub(crate) const INTEL_VENDOR_EBX: u32 = u32::from_le_bytes(*b"Genu");
pub(crate) const INTEL_VENDOR_EDX: u32 = u32::from_le_bytes(*b"ineI");
pub(crate) const INTEL_VENDOR_ECX: u32 = u32::from_le_bytes(*b"ntel");
pub(crate) const WAITPKG_BIT: u32 = 1 << 5;
pub(crate) const RDTSCP_BIT: u32 = 1 << 27;
pub(crate) const INVARIANT_TSC_BIT: u32 = 1 << 8;
pub(crate) const UMWAIT_C01_CONTROL: u32 = 1;

const WAITPKG_LEAF: u32 = 0x0000_0007;
const EXTENDED_CPUID_BASE: u32 = 0x8000_0000;
const EXTENDED_FEATURES_LEAF: u32 = 0x8000_0001;
const INVARIANT_TSC_LEAF: u32 = 0x8000_0007;
const PR_GET_TSC: i32 = 25;
const PR_TSC_ENABLE: i32 = 1;
const TIMER_CALIBRATION_INTERVAL: Duration = Duration::from_millis(2);
pub(crate) const TIMER_CALIBRATION_SAMPLES: usize = 3;
const TIMER_CALIBRATION_ATTEMPTS: usize = 9;
const MAX_CALIBRATION_SPREAD_PERCENT: u64 = 20;
const CONSERVATIVE_FREQUENCY_PERCENT: u64 = 80;

unsafe extern "C" {
    fn prctl(option: i32, ...) -> i32;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IntelCapabilityRegisters {
    pub(crate) vendor_ebx: u32,
    pub(crate) vendor_edx: u32,
    pub(crate) vendor_ecx: u32,
    pub(crate) waitpkg_features_ecx: Option<u32>,
    pub(crate) extended_features_edx: Option<u32>,
    pub(crate) invariant_features_edx: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IntelCapabilities {
    pub(crate) intel_vendor: bool,
    pub(crate) waitpkg: bool,
    pub(crate) invariant_tsc: bool,
    pub(crate) rdtscp: bool,
    pub(crate) timer_hz: Option<u64>,
}

pub(crate) fn detect_capabilities() -> IntelCapabilities {
    let vendor = __cpuid(0);
    let waitpkg_features =
        waitpkg_leaf_is_available(vendor.eax).then(|| __cpuid_count(WAITPKG_LEAF, 0));
    let extended_max = __cpuid(EXTENDED_CPUID_BASE).eax;
    let (has_extended_features, has_invariant_features) = extended_leaf_availability(extended_max);
    let extended_features = has_extended_features.then(|| __cpuid(EXTENDED_FEATURES_LEAF));
    let invariant_features = has_invariant_features.then(|| __cpuid(INVARIANT_TSC_LEAF));

    let registers = IntelCapabilityRegisters {
        vendor_ebx: vendor.ebx,
        vendor_edx: vendor.edx,
        vendor_ecx: vendor.ecx,
        waitpkg_features_ecx: waitpkg_features.map(|features| features.ecx),
        extended_features_edx: extended_features.map(|features| features.edx),
        invariant_features_edx: invariant_features.map(|features| features.edx),
    };
    decode_capabilities(registers, None)
}

pub(crate) fn calibrate_timer_hz() -> Option<u64> {
    let detected = detect_capabilities();
    if timer_calibration_is_usable(&detected) {
        collect_tsc_frequencies(sample_tsc_frequency_once).and_then(conservative_timer_hz)
    } else {
        None
    }
}

pub(crate) fn userspace_tsc_is_enabled() -> bool {
    let mut setting = 0_i32;
    // SAFETY: PR_GET_TSC expects one writable `int *`; remaining variadic
    // arguments are ignored. A failed or unknown query is rejected closed
    // before any RDTSCP or UMWAIT deadline is attempted.
    let status = unsafe {
        prctl(
            PR_GET_TSC,
            &mut setting as *mut i32,
            0_usize,
            0_usize,
            0_usize,
        )
    };
    status == 0 && setting == PR_TSC_ENABLE
}

pub(crate) const fn waitpkg_leaf_is_available(basic_max: u32) -> bool {
    basic_max >= WAITPKG_LEAF
}

pub(crate) const fn extended_leaf_availability(extended_max: u32) -> (bool, bool) {
    (
        extended_max >= EXTENDED_FEATURES_LEAF,
        extended_max >= INVARIANT_TSC_LEAF,
    )
}

pub(crate) fn decode_capabilities(
    registers: IntelCapabilityRegisters,
    calibrated_timer_hz: Option<u64>,
) -> IntelCapabilities {
    let intel_vendor = registers.vendor_ebx == INTEL_VENDOR_EBX
        && registers.vendor_edx == INTEL_VENDOR_EDX
        && registers.vendor_ecx == INTEL_VENDOR_ECX;
    let waitpkg = registers
        .waitpkg_features_ecx
        .is_some_and(|features| features & WAITPKG_BIT != 0);
    let rdtscp = registers
        .extended_features_edx
        .is_some_and(|features| features & RDTSCP_BIT != 0);
    let invariant_tsc = registers
        .invariant_features_edx
        .is_some_and(|features| features & INVARIANT_TSC_BIT != 0);
    let timer_hz = (intel_vendor && waitpkg && invariant_tsc && rdtscp)
        .then_some(calibrated_timer_hz)
        .flatten();
    IntelCapabilities {
        intel_vendor,
        waitpkg,
        invariant_tsc,
        rdtscp,
        timer_hz,
    }
}

pub(crate) const fn timer_calibration_is_usable(capabilities: &IntelCapabilities) -> bool {
    capabilities.intel_vendor
        && capabilities.waitpkg
        && capabilities.invariant_tsc
        && capabilities.rdtscp
}

pub(crate) fn collect_tsc_frequencies<F>(
    mut sample_once: F,
) -> Option<[u64; TIMER_CALIBRATION_SAMPLES]>
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

pub(crate) fn tsc_frequency_from_observation(
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

pub(crate) fn frequency_hz(ticks: u64, elapsed_nanos: u128) -> u64 {
    let frequency = u128::from(ticks).saturating_mul(1_000_000_000) / elapsed_nanos.max(1);
    u64::try_from(frequency).unwrap_or(u64::MAX).max(1)
}

pub(crate) fn conservative_timer_hz(mut samples: [u64; TIMER_CALIBRATION_SAMPLES]) -> Option<u64> {
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
pub(crate) fn read_tsc_ordered() -> (u64, u32) {
    let low: u32;
    let high: u32;
    let auxiliary: u32;
    // SAFETY: the caller checked RDTSCP before selecting this boundary.
    // RDTSCP orders the read after older loads, LFENCE prevents later work
    // moving before it, and TSC_AUX identifies the logical processor.
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

pub(crate) const fn tsc_from_halves(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) + low as u64
}

pub(crate) const fn split_tsc_deadline(deadline: u64) -> (u32, u32) {
    (deadline as u32, (deadline >> 32) as u32)
}

#[inline]
pub(crate) fn umonitor(address: *const ()) {
    // SAFETY: the caller checked Intel WAITPKG support and passes the address
    // of a live AtomicU32 or AtomicU64 that remains alive through UMWAIT.
    unsafe {
        asm!(
            ".byte 0xf3, 0x0f, 0xae, 0xf0",
            in("rax") address,
            options(nostack)
        );
    }
}

#[inline]
pub(crate) fn umwait_c01(deadline: u64) {
    let (deadline_low, deadline_high) = split_tsc_deadline(deadline);
    // SAFETY: the caller checked Intel WAITPKG support and performed UMONITOR
    // plus an Acquire recheck. EDX:EAX is an absolute TSC deadline, and ECX=1
    // requests the faster-wakeup C0.1 state rather than C0.2.
    unsafe {
        asm!(
            ".byte 0xf2, 0x0f, 0xae, 0xf1",
            in("eax") deadline_low,
            in("edx") deadline_high,
            in("ecx") UMWAIT_C01_CONTROL,
            options(nostack)
        );
    }
}
