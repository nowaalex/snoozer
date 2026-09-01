#![allow(
    unsafe_code,
    reason = "Linux affinity and RDTSCP intrinsics are isolated in this benchmark-only platform boundary"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::pure::parse_cpu_list;

type AnyError = Box<dyn Error + Send + Sync>;

const CPU_SET_BITS: usize = 1_024;
const CPU_SET_WORDS: usize = CPU_SET_BITS / usize::BITS as usize;
const CALIBRATION_RUNS: usize = 7;
const CALIBRATION_WINDOW: Duration = Duration::from_millis(40);
const MAX_CALIBRATION_SPREAD_PERCENT: f64 = 1.0;
const TSC_SKEW_SAMPLES: u64 = 100_000;
const MAX_CROSS_CORE_SKEW_BOUND_NS: f64 = 100.0;

#[derive(Clone, Debug)]
pub(crate) struct CpuInfo {
    pub(crate) cpu: usize,
    pub(crate) package: u32,
    pub(crate) core: u32,
    pub(crate) siblings: BTreeSet<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct Topology {
    pub(crate) waiter: usize,
    pub(crate) victim: usize,
    pub(crate) producer: usize,
    pub(crate) controller: usize,
    pub(crate) cpus: BTreeMap<usize, CpuInfo>,
    sysfs_root: PathBuf,
}

impl Topology {
    pub(crate) fn discover(
        overrides: Option<[usize; 4]>,
        sysfs_root: PathBuf,
    ) -> Result<Self, AnyError> {
        let cpus = discover_cpu_info(&sysfs_root)?;
        let [waiter, victim, producer, controller] = if let Some(overrides) = overrides {
            overrides
        } else {
            choose_roles(&cpus)?
        };
        let topology = Self {
            waiter,
            victim,
            producer,
            controller,
            cpus,
            sysfs_root,
        };
        topology.validate()?;
        Ok(topology)
    }

    pub(crate) fn selected(&self) -> [usize; 4] {
        [self.waiter, self.victim, self.producer, self.controller]
    }

    pub(crate) fn read_cpuidle(&self) -> Result<Vec<CpuIdleState>, AnyError> {
        let mut result = Vec::new();
        for cpu in self.selected() {
            let cpuidle = self.sysfs_root.join(format!("cpu{cpu}/cpuidle"));
            if !cpuidle.is_dir() {
                return Err(format!("CPU {cpu} has no cpuidle sysfs directory").into());
            }
            let mut exact_poll = 0_u8;
            let mut exact_c1 = 0_u8;
            for entry in fs::read_dir(&cpuidle)? {
                let path = entry?.path();
                if !path.is_dir()
                    || !path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("state"))
                {
                    continue;
                }
                let name = read_trimmed(path.join("name"))?.to_ascii_uppercase();
                let disabled = match read_trimmed(path.join("disable"))?.as_str() {
                    "0" => false,
                    "1" => true,
                    value => {
                        return Err(format!(
                            "CPU {cpu} state {name} has invalid disable value {value:?}"
                        )
                        .into());
                    }
                };
                match name.as_str() {
                    "POLL" => {
                        exact_poll = exact_poll.saturating_add(1);
                    }
                    "C1" => {
                        exact_c1 = exact_c1.saturating_add(1);
                    }
                    _ => {}
                }
                result.push(CpuIdleState {
                    cpu,
                    name,
                    disabled,
                });
            }
            if exact_poll != 1 || exact_c1 != 1 {
                return Err(format!(
                    "CPU {cpu} must expose exactly one POLL and one exact CPU C1 state"
                )
                .into());
            }
        }
        Ok(result)
    }

    pub(crate) fn validate_cpuidle(&self) -> Result<Vec<CpuIdleState>, AnyError> {
        let states = self.read_cpuidle()?;
        for state in &states {
            match state.name.as_str() {
                "POLL" | "C1" if state.disabled => {
                    return Err(
                        format!("CPU {}: exact {} must be enabled", state.cpu, state.name).into(),
                    );
                }
                "POLL" | "C1" => {}
                _ if !state.disabled => {
                    return Err(format!(
                        "CPU {}: {} is enabled; only exact POLL/CPU C1 are allowed",
                        state.cpu, state.name
                    )
                    .into());
                }
                _ => {}
            }
        }
        Ok(states)
    }

    fn validate(&self) -> Result<(), AnyError> {
        let mut assigned = BTreeSet::new();
        for cpu in self.selected() {
            if cpu >= CPU_SET_BITS {
                return Err(format!("CPU {cpu} exceeds the Linux affinity mask limit").into());
            }
            if !self.cpus.contains_key(&cpu) {
                return Err(format!("CPU {cpu} is not online in the selected sysfs tree").into());
            }
            if !assigned.insert(cpu) {
                return Err(format!("CPU {cpu} is assigned to more than one role").into());
            }
        }
        let waiter = &self.cpus[&self.waiter];
        let victim = &self.cpus[&self.victim];
        let producer = &self.cpus[&self.producer];
        let controller = &self.cpus[&self.controller];
        if !same_core(waiter, victim) {
            return Err("waiter and victim must be SMT siblings".into());
        }
        if [victim, producer, controller]
            .iter()
            .any(|cpu| cpu.package != waiter.package)
        {
            return Err("all benchmark roles must use one physical CPU package".into());
        }
        if same_core(waiter, producer)
            || same_core(waiter, controller)
            || same_core(producer, controller)
        {
            return Err(
                "producer and controller must each use a physical core separate from the waiter"
                    .into(),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CpuIdleState {
    pub(crate) cpu: usize,
    pub(crate) name: String,
    pub(crate) disabled: bool,
}

fn choose_roles(cpus: &BTreeMap<usize, CpuInfo>) -> Result<[usize; 4], AnyError> {
    let waiter = cpus
        .values()
        .find(|candidate| candidate.siblings.len() >= 2)
        .ok_or("no online SMT sibling pair was found")?;
    let victim = waiter
        .siblings
        .iter()
        .copied()
        .find(|cpu| *cpu != waiter.cpu && cpus.contains_key(cpu))
        .ok_or("the waiter sibling is not online")?;
    let producer = cpus
        .values()
        .find(|candidate| candidate.package == waiter.package && !same_core(candidate, waiter))
        .ok_or("no separate physical core is available for the producer")?;
    let controller = cpus
        .values()
        .find(|candidate| {
            candidate.package == waiter.package
                && !same_core(candidate, waiter)
                && !same_core(candidate, producer)
        })
        .ok_or("no third physical core is available for the controller")?;
    Ok([waiter.cpu, victim, producer.cpu, controller.cpu])
}

fn same_core(left: &CpuInfo, right: &CpuInfo) -> bool {
    left.package == right.package && left.core == right.core
}

fn discover_cpu_info(root: &Path) -> Result<BTreeMap<usize, CpuInfo>, AnyError> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(raw_cpu) = name.strip_prefix("cpu") else {
            continue;
        };
        let Ok(cpu) = raw_cpu.parse::<usize>() else {
            continue;
        };
        let topology = entry.path().join("topology");
        if !topology.is_dir() || !cpu_is_online(root, cpu)? {
            continue;
        }
        result.insert(
            cpu,
            CpuInfo {
                cpu,
                package: read_trimmed(topology.join("physical_package_id"))?.parse()?,
                core: read_trimmed(topology.join("core_id"))?.parse()?,
                siblings: parse_cpu_list(&read_trimmed(topology.join("thread_siblings_list"))?)?,
            },
        );
    }
    if result.is_empty() {
        return Err("Linux sysfs contains no online CPU topology".into());
    }
    Ok(result)
}

fn cpu_is_online(root: &Path, cpu: usize) -> Result<bool, io::Error> {
    let online = root.join(format!("cpu{cpu}/online"));
    if !online.exists() {
        return Ok(true);
    }
    Ok(read_trimmed(online)? == "1")
}

fn read_trimmed(path: impl AsRef<Path>) -> Result<String, io::Error> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

#[repr(C)]
struct LinuxCpuSet {
    words: [usize; CPU_SET_WORDS],
}

unsafe extern "C" {
    fn sched_setaffinity(pid: i32, cpusetsize: usize, mask: *const LinuxCpuSet) -> i32;
}

pub(crate) fn pin_current(cpu: usize) -> Result<(), io::Error> {
    set_current_affinity(&[cpu])
}

pub(crate) fn set_current_affinity(cpus: &[usize]) -> Result<(), io::Error> {
    if cpus.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "affinity mask must contain at least one CPU",
        ));
    }
    if cpus.iter().any(|&cpu| cpu >= CPU_SET_BITS) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CPU exceeds the affinity mask limit",
        ));
    }
    let mut set = LinuxCpuSet {
        words: [0; CPU_SET_WORDS],
    };
    for &cpu in cpus {
        set.words[cpu / usize::BITS as usize] |= 1_usize << (cpu % usize::BITS as usize);
    }
    // SAFETY: `set` has the Linux x86_64 `cpu_set_t` layout and remains alive
    // for the call. A pid of zero targets only the calling thread.
    let result = unsafe { sched_setaffinity(0, size_of::<LinuxCpuSet>(), &raw const set) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TscClock {
    pub(crate) cycles_per_ns: f64,
    pub(crate) spread_percent: f64,
}

impl TscClock {
    pub(crate) fn preflight() -> Result<Self, AnyError> {
        require_tsc_capabilities()?;
        let clocksource =
            read_trimmed("/sys/devices/system/clocksource/clocksource0/current_clocksource")?;
        if clocksource != "tsc" {
            return Err(format!("Linux clocksource is {clocksource:?}, not \"tsc\"").into());
        }

        let mut ratios = Vec::with_capacity(CALIBRATION_RUNS);
        for _ in 0..CALIBRATION_RUNS {
            let wall_start = Instant::now();
            let (cycle_start, aux_start) = stamp();
            std::thread::sleep(CALIBRATION_WINDOW);
            let (cycle_end, aux_end) = stamp();
            if aux_start != aux_end || cycle_end <= cycle_start {
                return Err(
                    "CPU migration or non-monotonic TSC occurred during calibration".into(),
                );
            }
            ratios.push((cycle_end - cycle_start) as f64 / wall_start.elapsed().as_nanos() as f64);
        }
        ratios.sort_by(f64::total_cmp);
        let median = ratios[ratios.len() / 2];
        let minimum = ratios[0];
        let maximum = ratios[ratios.len() - 1];
        let spread_percent = (maximum - minimum) / median * 100.0;
        if !median.is_finite() || median <= 0.0 || spread_percent > MAX_CALIBRATION_SPREAD_PERCENT {
            return Err(format!(
                "TSC calibration is unstable: {spread_percent:.3}% spread exceeds {MAX_CALIBRATION_SPREAD_PERCENT:.3}%"
            )
            .into());
        }
        Ok(Self {
            cycles_per_ns: median,
            spread_percent,
        })
    }

    pub(crate) fn cycles_to_ns(self, cycles: u64) -> u64 {
        (cycles as f64 / self.cycles_per_ns).round() as u64
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TscSkew {
    pub(crate) producer_offset_cycles: i64,
    pub(crate) waiter_offset_cycles: i64,
    pub(crate) producer_uncertainty_cycles: u64,
    pub(crate) waiter_uncertainty_cycles: u64,
    pub(crate) producer_to_waiter_bound_ns: f64,
    pub(crate) correction_cycles: i64,
}

impl TscSkew {
    pub(crate) fn preflight(
        producer_cpu: usize,
        waiter_cpu: usize,
        controller_cpu: usize,
        clock: TscClock,
    ) -> Result<Self, AnyError> {
        pin_current(producer_cpu)?;
        let estimate = estimate_remote_tsc_offset(waiter_cpu);
        let restore = pin_current(controller_cpu);
        restore?;
        let (waiter_offset, waiter_uncertainty) = estimate?;
        let producer_offset = 0_i64;
        let producer_uncertainty = 0_u64;
        let bound_cycles = i128::from(waiter_offset)
            .unsigned_abs()
            .saturating_add(u128::from(waiter_uncertainty));
        let bound_ns = bound_cycles as f64 / clock.cycles_per_ns;
        if bound_ns > MAX_CROSS_CORE_SKEW_BOUND_NS {
            return Err(format!(
                "producer/waiter TSC skew bound is {bound_ns:.1}ns, above the {MAX_CROSS_CORE_SKEW_BOUND_NS:.1}ns limit"
            )
            .into());
        }
        let correction_cycles = waiter_offset
            .checked_sub(producer_offset)
            .ok_or("producer/waiter TSC correction overflowed i64")?;
        Ok(Self {
            producer_offset_cycles: producer_offset,
            waiter_offset_cycles: waiter_offset,
            producer_uncertainty_cycles: producer_uncertainty,
            waiter_uncertainty_cycles: waiter_uncertainty,
            producer_to_waiter_bound_ns: bound_ns,
            correction_cycles,
        })
    }
}

#[repr(align(128))]
struct TscHandshake {
    request: AtomicU64,
    response: AtomicU64,
    remote_cycles: AtomicU64,
    ready: AtomicBool,
    failed: AtomicBool,
}

fn estimate_remote_tsc_offset(cpu: usize) -> Result<(i64, u64), AnyError> {
    let handshake = Arc::new(TscHandshake {
        request: AtomicU64::new(0),
        response: AtomicU64::new(0),
        remote_cycles: AtomicU64::new(0),
        ready: AtomicBool::new(false),
        failed: AtomicBool::new(false),
    });
    let remote = Arc::clone(&handshake);
    let worker = std::thread::spawn(move || -> Result<(), io::Error> {
        pin_current(cpu)?;
        let (_, expected_aux) = stamp();
        remote.ready.store(true, Ordering::Release);
        for sequence in 1..=TSC_SKEW_SAMPLES {
            while remote.request.load(Ordering::Acquire) != sequence {
                std::hint::spin_loop();
            }
            let (cycles, auxiliary) = stamp();
            if auxiliary != expected_aux {
                remote.failed.store(true, Ordering::Release);
            }
            remote.remote_cycles.store(cycles, Ordering::Relaxed);
            remote.response.store(sequence, Ordering::Release);
        }
        Ok(())
    });
    let startup = Instant::now();
    while !handshake.ready.load(Ordering::Acquire) {
        if worker.is_finished() || startup.elapsed() >= Duration::from_secs(5) {
            return Err("TSC skew worker did not reach its start barrier".into());
        }
        std::hint::spin_loop();
    }

    let (_, controller_aux) = stamp();
    let mut best_round_trip = u64::MAX;
    let mut best_offset = 0_i128;
    for sequence in 1..=TSC_SKEW_SAMPLES {
        let (before, before_aux) = stamp();
        handshake.request.store(sequence, Ordering::Release);
        while handshake.response.load(Ordering::Acquire) != sequence {
            std::hint::spin_loop();
        }
        let remote_cycles = handshake.remote_cycles.load(Ordering::Relaxed);
        let (after, after_aux) = stamp();
        if before_aux != controller_aux || after_aux != controller_aux || after < before {
            return Err("controller migrated during TSC skew preflight".into());
        }
        let round_trip = after - before;
        if round_trip < best_round_trip {
            best_round_trip = round_trip;
            let midpoint = u128::from(before) + u128::from(round_trip / 2);
            best_offset = i128::from(remote_cycles) - midpoint as i128;
        }
    }
    match worker.join() {
        Ok(result) => result?,
        Err(_) => return Err("TSC skew worker panicked".into()),
    }
    if handshake.failed.load(Ordering::Acquire) {
        return Err("remote thread migrated during TSC skew preflight".into());
    }
    let offset = i64::try_from(best_offset).map_err(|_| "TSC offset does not fit i64")?;
    Ok((offset, best_round_trip / 2))
}

#[cfg(target_arch = "x86_64")]
fn require_tsc_capabilities() -> Result<(), AnyError> {
    use std::arch::x86_64::__cpuid;

    let maximum_extended = __cpuid(0x8000_0000).eax;
    if maximum_extended < 0x8000_0007 {
        return Err("CPU lacks the invariant-TSC capability leaf".into());
    }
    let feature_leaf = __cpuid(0x8000_0001);
    let power_leaf = __cpuid(0x8000_0007);
    let rdtscp = feature_leaf.edx & (1 << 27) != 0;
    let invariant_tsc = power_leaf.edx & (1 << 8) != 0;
    if !rdtscp || !invariant_tsc {
        return Err(format!(
            "timing requires RDTSCP and invariant TSC (rdtscp={rdtscp}, invariant_tsc={invariant_tsc})"
        )
        .into());
    }
    Ok(())
}

#[cfg(target_arch = "x86_64")]
#[inline]
pub(crate) fn stamp() -> (u64, u32) {
    use std::arch::x86_64::{__rdtscp, _mm_lfence};

    let mut auxiliary = 0_u32;
    // SAFETY: The benchmark preflight requires RDTSCP before any timed trial.
    let cycles = unsafe { __rdtscp(&mut auxiliary) };
    // SAFETY: LFENCE has no memory-safety preconditions and prevents later work
    // from moving before the timestamp.
    unsafe { _mm_lfence() };
    (cycles, auxiliary)
}

#[cfg(not(target_arch = "x86_64"))]
fn require_tsc_capabilities() -> Result<(), AnyError> {
    Err("the first benchmark implementation requires Linux x86_64".into())
}

#[cfg(not(target_arch = "x86_64"))]
pub(crate) fn stamp() -> (u64, u32) {
    unreachable!("the platform preflight rejects non-x86_64 targets")
}

pub(crate) fn cpu_metadata(cpu: usize) -> CpuMetadata {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let section = cpuinfo
        .split("\n\n")
        .find(|section| field(section, "processor").is_some_and(|value| value == cpu.to_string()))
        .unwrap_or_default();
    CpuMetadata {
        model: field(section, "model name").unwrap_or_else(|| "unknown".to_owned()),
        microcode: field(section, "microcode").unwrap_or_else(|| "unknown".to_owned()),
        kernel: read_trimmed("/proc/sys/kernel/osrelease").unwrap_or_else(|_| "unknown".to_owned()),
    }
}

pub(crate) fn cpu_power_policy(cpu: usize, sysfs_root: &Path) -> Result<CpuPowerPolicy, AnyError> {
    let policy = sysfs_root.join(format!("cpu{cpu}/cpufreq"));
    let governor_path = policy.join("scaling_governor");
    let energy_preference_path = policy.join("energy_performance_preference");
    let governor = read_trimmed(&governor_path)
        .map_err(|error| format!("cannot read {}: {error}", governor_path.display()))?;
    let energy_preference = read_trimmed(&energy_preference_path)
        .map_err(|error| format!("cannot read {}: {error}", energy_preference_path.display()))?;
    if governor.is_empty() || energy_preference.is_empty() {
        return Err(format!("CPU {cpu} exposes an empty governor or energy preference").into());
    }
    Ok(CpuPowerPolicy {
        cpu,
        governor,
        energy_preference,
    })
}

fn field(section: &str, key: &str) -> Option<String> {
    section.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.trim() == key).then(|| value.trim().to_owned())
    })
}

#[derive(Clone, Debug)]
pub(crate) struct CpuMetadata {
    pub(crate) model: String,
    pub(crate) microcode: String,
    pub(crate) kernel: String,
}

#[derive(Clone, Debug)]
pub(crate) struct CpuPowerPolicy {
    pub(crate) cpu: usize,
    pub(crate) governor: String,
    pub(crate) energy_preference: String,
}
