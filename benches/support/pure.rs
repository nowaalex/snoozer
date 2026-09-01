use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

pub(crate) const RESULT_SCHEMA_VERSION: &str = "snoozer-wake-latency-v2";
pub(crate) const CPU_SYSFS_ROOT: &str = "/sys/devices/system/cpu";

pub(crate) struct GapSchedule {
    state: u64,
}

impl GapSchedule {
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub(crate) fn next(&mut self) -> Duration {
        // xorshift64* is fixed here so every strategy receives the same
        // versioned weighted schedule.
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        let bucket = self.state.wrapping_mul(0x2545_f491_4f6c_dd1d) % 100;
        let microseconds = match bucket {
            0..=29 => 0,
            30..=44 => 1,
            45..=56 => 5,
            57..=66 => 10,
            67..=76 => 25,
            77..=84 => 50,
            85..=91 => 100,
            92..=96 => 250,
            _ => 1_000,
        };
        Duration::from_micros(microseconds)
    }
}

pub(crate) fn correct_latency(raw_cycles: i64, waiter_minus_producer_cycles: i64) -> Option<u64> {
    let corrected = i128::from(raw_cycles) - i128::from(waiter_minus_producer_cycles);
    u64::try_from(corrected).ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WaiterStartup {
    Aborted,
    Observed(u64),
}

pub(crate) fn capture_generation_before_start(
    generation: &AtomicU64,
    ready: &AtomicUsize,
    go: &AtomicBool,
    stop: &AtomicBool,
) -> WaiterStartup {
    let observed = generation.load(Ordering::Acquire);
    ready.fetch_add(1, Ordering::Release);
    while !go.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }
    if stop.load(Ordering::Acquire) {
        WaiterStartup::Aborted
    } else {
        WaiterStartup::Observed(observed)
    }
}

pub(crate) fn resolve_cpu_sysfs_root(
    custom_root: Option<PathBuf>,
    allow_custom_root: bool,
) -> Result<PathBuf, &'static str> {
    match (custom_root, allow_custom_root) {
        (Some(root), true) => Ok(root),
        (Some(_), false) => Err("official mode does not allow SNOOZER_SYSFS_ROOT"),
        (None, _) => Ok(PathBuf::from(CPU_SYSFS_ROOT)),
    }
}

pub(crate) fn latency_rank_key(
    p50_cycles: u64,
    p99_cycles: u64,
    p999_cycles: u64,
) -> (u64, u64, u64) {
    (p99_cycles, p999_cycles, p50_cycles)
}

pub(crate) fn median_latency_json_fields(
    p50_cycles: u64,
    p99_cycles: u64,
    p999_cycles: u64,
    p50_ns: u64,
    p99_ns: u64,
    p999_ns: u64,
) -> String {
    format!(
        "\"median_p50_cycles\":{p50_cycles},\"median_p99_cycles\":{p99_cycles},\"median_p999_cycles\":{p999_cycles},\"median_p50_ns\":{p50_ns},\"median_p99_ns\":{p99_ns},\"median_p999_ns\":{p999_ns}"
    )
}

pub(crate) fn percentile_sorted(values: &[u64], quantile: f64) -> u64 {
    let rank = (quantile * values.len() as f64).ceil() as usize;
    values[rank.saturating_sub(1).min(values.len() - 1)]
}

pub(crate) fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ignored = write!(escaped, "\\u{:04x}", character as u32);
            }
            character => escaped.push(character),
        }
    }
    escaped
}

pub(crate) fn parse_cpu_list(raw: &str) -> Result<BTreeSet<usize>, String> {
    let mut result = BTreeSet::new();
    for component in raw.split(',') {
        if let Some((start, end)) = component.split_once('-') {
            let start = start
                .parse::<usize>()
                .map_err(|error| format!("invalid CPU range start {component:?}: {error}"))?;
            let end = end
                .parse::<usize>()
                .map_err(|error| format!("invalid CPU range end {component:?}: {error}"))?;
            if start > end {
                return Err(format!("invalid CPU range {component:?}"));
            }
            result.extend(start..=end);
        } else {
            result.insert(
                component
                    .parse()
                    .map_err(|error| format!("invalid CPU number {component:?}: {error}"))?,
            );
        }
    }
    Ok(result)
}
