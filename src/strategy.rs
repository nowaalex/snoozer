#![deny(unsafe_code)]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
use self::hardware_wait_unsupported as hardware_wait;
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
use std::convert::Infallible;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::PreflightFailure;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::arch;
use crate::{
    HardwareBackend, HardwareWaitError, Strategy, UnsupportedReason, UnsupportedStrategy,
    WaitableAtomic, capabilities,
};

// EAX[7:4] is the MWAITX C-state field. Setting that field to 0xf disables
// C-state entry, leaving EAX[3:0] zero. This matches Linux's
// MWAITX_DISABLE_CSTATES:
// https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/include/asm/mwait.h
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const NO_C_STATE_HINT: u32 = 0xf0;
#[cfg(feature = "benchmark-only")]
const C1_STATE_HINT: u32 = 0;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const MWAITX_SAFETY_TIMEOUT: Duration = Duration::from_millis(1);
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const UMWAIT_SAFETY_TIMEOUT: Duration = Duration::from_millis(1);
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const NANOS_PER_SECOND: u128 = 1_000_000_000;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const PREFLIGHT_BASELINE_ATTEMPTS: u32 = 5;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const PREFLIGHT_STORE_ATTEMPTS: u32 = 16;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const PREFLIGHT_REQUIRED_STORE_OBSERVATIONS: u32 = 2;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const PREFLIGHT_MIN_BLOCK: Duration = Duration::from_micros(1);
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const PREFLIGHT_HELPER_DEADLINE: Duration = Duration::from_millis(20);

/// Result of one unbounded raw wait attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitResult<T> {
    /// An Acquire load observed a value other than the expected value.
    Changed(T),
    /// The strategy stopped waiting without observing a different value.
    Unclassified,
}

/// Result of one bounded raw wait attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitTimeoutResult<T> {
    /// An Acquire load observed a value other than the expected value.
    Changed(T),
    /// The strategy stopped waiting before the public timeout, without an
    /// observed value change.
    Unclassified,
    /// The public timeout expired while the value remained equal.
    TimedOut,
}

/// Result of a bounded filtered wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitUntilTimeoutResult<T> {
    /// An Acquire load observed a value other than the expected value.
    Changed(T),
    /// The public timeout expired while the value remained equal.
    TimedOut,
}

#[derive(Clone, Copy)]
struct Deadline {
    started: Instant,
    timeout: Duration,
}

impl Deadline {
    fn new(timeout: Duration) -> Self {
        Self {
            started: Instant::now(),
            timeout,
        }
    }

    fn remaining(self) -> Option<Duration> {
        let elapsed = self.started.elapsed();
        if elapsed >= self.timeout {
            None
        } else {
            Some(self.timeout - elapsed)
        }
    }
}

mod sealed {
    pub trait Sealed {}
}

trait StrategyImpl: Send + Sync {
    fn strategy(&self) -> Strategy;

    #[inline]
    fn wait_raw_untimed<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
    ) -> WaitTimeoutResult<A::Value> {
        self.wait_raw(atomic, expected, None)
    }

    fn wait_raw<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
        deadline: Option<Deadline>,
    ) -> WaitTimeoutResult<A::Value>;
}

/// A sealed, statically dispatched wait strategy.
///
/// The trait is deliberately not object-safe. Calls are monomorphized so no
/// virtual dispatch is added to a wait hot path.
pub trait WaitStrategy: sealed::Sealed + Send + Sync {
    /// Identifies this built-in strategy.
    ///
    /// ```
    /// use snoozer::{BusySpin, Strategy, WaitStrategy as _};
    ///
    /// assert_eq!(BusySpin.strategy(), Strategy::BusySpin);
    /// ```
    #[must_use]
    fn strategy(&self) -> Strategy;

    /// Waits only if the atomic equals `expected` and performs at most one
    /// strategy-specific blocking or yielding operation.
    ///
    /// [`WaitResult::Unclassified`] does not prove that work is available and
    /// does not synchronize with a producer. The caller must recheck its
    /// published state with an Acquire operation.
    ///
    /// ```
    /// use snoozer::{BusySpin, WaitResult, WaitStrategy as _};
    /// use std::sync::atomic::AtomicU32;
    ///
    /// let state = AtomicU32::new(1);
    /// assert_eq!(BusySpin.wait_if_equal(&state, 0), WaitResult::Changed(1));
    /// ```
    #[must_use]
    fn wait_if_equal<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
    ) -> WaitResult<A::Value>;

    /// Absorbs unclassified wakes until an Acquire load observes a value
    /// different from `expected`.
    ///
    /// ```
    /// use snoozer::{BusySpin, WaitStrategy as _};
    /// use std::sync::atomic::AtomicU32;
    ///
    /// assert_eq!(BusySpin.wait_until_different(&AtomicU32::new(1), 0), 1);
    /// ```
    #[must_use]
    fn wait_until_different<A: WaitableAtomic>(&self, atomic: &A, expected: A::Value) -> A::Value;

    /// Performs one wait attempt bounded by `timeout`.
    ///
    /// ```
    /// use snoozer::{BusySpin, WaitStrategy as _, WaitTimeoutResult};
    /// use std::sync::atomic::AtomicU32;
    /// use std::time::Duration;
    ///
    /// assert_eq!(
    ///     BusySpin.wait_if_equal_timeout(&AtomicU32::new(1), 0, Duration::ZERO),
    ///     WaitTimeoutResult::Changed(1),
    /// );
    /// ```
    #[must_use]
    fn wait_if_equal_timeout<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
        timeout: Duration,
    ) -> WaitTimeoutResult<A::Value>;

    /// Absorbs unclassified wakes until the value changes or `timeout`
    /// expires.
    ///
    /// ```
    /// use snoozer::{BusySpin, WaitStrategy as _, WaitUntilTimeoutResult};
    /// use std::sync::atomic::AtomicU32;
    /// use std::time::Duration;
    ///
    /// assert_eq!(
    ///     BusySpin.wait_until_different_timeout(&AtomicU32::new(1), 0, Duration::ZERO),
    ///     WaitUntilTimeoutResult::Changed(1),
    /// );
    /// ```
    #[must_use]
    fn wait_until_different_timeout<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
        timeout: Duration,
    ) -> WaitUntilTimeoutResult<A::Value>;
}

macro_rules! impl_wait_strategy {
    ($strategy:ty) => {
        impl sealed::Sealed for $strategy {}

        impl WaitStrategy for $strategy {
            #[inline]
            fn strategy(&self) -> Strategy {
                StrategyImpl::strategy(self)
            }

            #[inline]
            fn wait_if_equal<A: WaitableAtomic>(
                &self,
                atomic: &A,
                expected: A::Value,
            ) -> WaitResult<A::Value> {
                match self.wait_raw_untimed(atomic, expected) {
                    WaitTimeoutResult::Changed(value) => WaitResult::Changed(value),
                    WaitTimeoutResult::Unclassified | WaitTimeoutResult::TimedOut => {
                        WaitResult::Unclassified
                    }
                }
            }

            #[inline]
            fn wait_until_different<A: WaitableAtomic>(
                &self,
                atomic: &A,
                expected: A::Value,
            ) -> A::Value {
                loop {
                    if let WaitTimeoutResult::Changed(value) =
                        self.wait_raw_untimed(atomic, expected)
                    {
                        return value;
                    }
                }
            }

            #[inline]
            fn wait_if_equal_timeout<A: WaitableAtomic>(
                &self,
                atomic: &A,
                expected: A::Value,
                timeout: Duration,
            ) -> WaitTimeoutResult<A::Value> {
                self.wait_raw(atomic, expected, Some(Deadline::new(timeout)))
            }

            #[inline]
            fn wait_until_different_timeout<A: WaitableAtomic>(
                &self,
                atomic: &A,
                expected: A::Value,
                timeout: Duration,
            ) -> WaitUntilTimeoutResult<A::Value> {
                let deadline = Deadline::new(timeout);
                loop {
                    match self.wait_raw(atomic, expected, Some(deadline)) {
                        WaitTimeoutResult::Changed(value) => {
                            return WaitUntilTimeoutResult::Changed(value);
                        }
                        WaitTimeoutResult::TimedOut => {
                            return WaitUntilTimeoutResult::TimedOut;
                        }
                        WaitTimeoutResult::Unclassified => {}
                    }
                }
            }
        }
    };
}

/// Continuously polls the observed atomic with a processor spin hint.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BusySpin;

impl StrategyImpl for BusySpin {
    fn strategy(&self) -> Strategy {
        Strategy::BusySpin
    }

    #[inline]
    fn wait_raw<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
        deadline: Option<Deadline>,
    ) -> WaitTimeoutResult<A::Value> {
        loop {
            let observed = atomic.__load_acquire();
            if observed != expected {
                return WaitTimeoutResult::Changed(observed);
            }
            if deadline.is_some_and(|value| value.remaining().is_none()) {
                return WaitTimeoutResult::TimedOut;
            }
            std::hint::spin_loop();
        }
    }
}

impl_wait_strategy!(BusySpin);

/// Polls for a fixed number of iterations, then yields the current time slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpinThenYield {
    spin_iterations: usize,
}

impl SpinThenYield {
    /// Creates a strategy with the requested spin prefix.
    ///
    /// ```
    /// use snoozer::SpinThenYield;
    ///
    /// let strategy = SpinThenYield::new(32);
    /// assert_eq!(strategy.spin_iterations(), 32);
    /// ```
    #[must_use]
    pub const fn new(spin_iterations: usize) -> Self {
        Self { spin_iterations }
    }

    /// Returns the configured spin prefix length.
    ///
    /// ```
    /// use snoozer::SpinThenYield;
    ///
    /// assert_eq!(SpinThenYield::new(32).spin_iterations(), 32);
    /// ```
    #[must_use]
    pub const fn spin_iterations(self) -> usize {
        self.spin_iterations
    }
}

impl StrategyImpl for SpinThenYield {
    fn strategy(&self) -> Strategy {
        Strategy::SpinThenYield
    }

    #[inline]
    fn wait_raw<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
        deadline: Option<Deadline>,
    ) -> WaitTimeoutResult<A::Value> {
        if let Some(result) = spin_prefix(atomic, expected, self.spin_iterations, deadline) {
            return result;
        }

        thread::yield_now();
        classify_after_wait(atomic, expected, deadline)
    }
}

impl_wait_strategy!(SpinThenYield);

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AmdConfig {
    timer_hz: u64,
    safety_timeout_cycles: u32,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl AmdConfig {
    fn new(timer_hz: u64) -> Self {
        Self {
            timer_hz,
            safety_timeout_cycles: duration_to_cycles(MWAITX_SAFETY_TIMEOUT, timer_hz),
        }
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AmdConfig {
    never: Infallible,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IntelConfig {
    timer_hz: u64,
    safety_timeout_cycles: u64,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl IntelConfig {
    fn new(timer_hz: u64) -> Self {
        Self {
            timer_hz,
            safety_timeout_cycles: duration_to_cycles_u64(UMWAIT_SAFETY_TIMEOUT, timer_hz),
        }
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IntelConfig {
    never: Infallible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code, reason = "uninhabited unsupported-target strategy state")
)]
enum HardwareConfig {
    Amd(AmdConfig),
    Intel(IntelConfig),
}

impl HardwareConfig {
    const fn backend(self) -> HardwareBackend {
        match self {
            Self::Amd(_) => HardwareBackend::AmdMwaitx,
            Self::Intel(_) => HardwareBackend::IntelUmwait,
        }
    }
}

/// Evidence collected by the one-time hardware wait preflight.
///
/// The store trials establish bounded operational evidence, not an
/// architectural receipt for the precise cause of a wake. Both instruction
/// sets permit unrelated and unclassified returns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreflightReport {
    backend: HardwareBackend,
    attempts: u32,
    verified_store_wakes: u32,
    baseline_wait: Duration,
}

impl PreflightReport {
    /// Returns the backend selected for this process.
    ///
    /// ```no_run
    /// # use snoozer::HardwareWait;
    /// let report = HardwareWait::preflight()?;
    /// let backend = report.backend();
    /// # Ok::<(), snoozer::HardwareWaitError>(())
    /// ```
    #[must_use]
    pub const fn backend(self) -> HardwareBackend {
        self.backend
    }

    /// Returns the number of bounded store-wake trials performed.
    ///
    /// ```no_run
    /// # use snoozer::HardwareWait;
    /// let report = HardwareWait::preflight()?;
    /// assert!(report.attempts() > 0);
    /// # Ok::<(), snoozer::HardwareWaitError>(())
    /// ```
    #[must_use]
    pub const fn attempts(self) -> u32 {
        self.attempts
    }

    /// Returns trials that observed the published value before the baseline deadline.
    ///
    /// ```no_run
    /// # use snoozer::HardwareWait;
    /// let report = HardwareWait::preflight()?;
    /// let wakes = report.verified_store_wakes();
    /// # Ok::<(), snoozer::HardwareWaitError>(())
    /// ```
    #[must_use]
    pub const fn verified_store_wakes(self) -> u32 {
        self.verified_store_wakes
    }

    /// Returns the median no-store wait measured by preflight.
    ///
    /// ```no_run
    /// # use snoozer::HardwareWait;
    /// let report = HardwareWait::preflight()?;
    /// let baseline = report.baseline_wait();
    /// # Ok::<(), snoozer::HardwareWaitError>(())
    /// ```
    #[must_use]
    pub const fn baseline_wait(self) -> Duration {
        self.baseline_wait
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PreparedHardwareWait {
    config: HardwareConfig,
    report: PreflightReport,
}

static HARDWARE_PREFLIGHT: OnceLock<Result<PreparedHardwareWait, HardwareWaitError>> =
    OnceLock::new();

fn initialize_once<T: Copy>(
    state: &OnceLock<Result<T, HardwareWaitError>>,
    initialize: impl FnOnce() -> Result<T, HardwareWaitError>,
) -> Result<T, HardwareWaitError> {
    *state.get_or_init(|| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(initialize))
            .unwrap_or(Err(HardwareWaitError::PreflightPanicked))
    })
}

/// Cross-vendor hardware-address wait selected by one-time preflight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HardwareWait {
    config: HardwareConfig,
}

impl HardwareWait {
    /// Performs capability detection, timer calibration, and a bounded
    /// functional probe exactly once for this process.
    ///
    /// Call this after establishing the final allowed CPU domain and power
    /// policy, while its helper can still run concurrently on another logical
    /// CPU. Repeated or concurrent calls return the cached result.
    ///
    /// ```no_run
    /// use snoozer::HardwareWait;
    ///
    /// let report = HardwareWait::preflight()?;
    /// # Ok::<(), snoozer::HardwareWaitError>(())
    /// ```
    pub fn preflight() -> Result<PreflightReport, HardwareWaitError> {
        initialize_once(&HARDWARE_PREFLIGHT, run_hardware_preflight).map(|prepared| prepared.report)
    }

    /// Cheaply constructs a strategy from the cached successful preflight.
    ///
    /// This method performs no CPUID, calibration, thread creation, or probe.
    ///
    /// ```no_run
    /// use snoozer::HardwareWait;
    ///
    /// HardwareWait::preflight()?;
    /// let strategy = HardwareWait::new()?;
    /// # Ok::<(), snoozer::HardwareWaitError>(())
    /// ```
    pub fn new() -> Result<Self, HardwareWaitError> {
        match HARDWARE_PREFLIGHT.get() {
            None => Err(HardwareWaitError::PreflightRequired),
            Some(Ok(prepared)) => Ok(Self {
                config: prepared.config,
            }),
            Some(Err(error)) => Err(*error),
        }
    }

    /// Returns the backend selected by preflight.
    ///
    /// ```no_run
    /// # use snoozer::HardwareWait;
    /// HardwareWait::preflight()?;
    /// let backend = HardwareWait::new()?.backend();
    /// # Ok::<(), snoozer::HardwareWaitError>(())
    /// ```
    #[must_use]
    pub const fn backend(self) -> HardwareBackend {
        self.config.backend()
    }
}

impl StrategyImpl for HardwareWait {
    fn strategy(&self) -> Strategy {
        Strategy::HardwareWait
    }

    #[inline]
    fn wait_raw_untimed<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
    ) -> WaitTimeoutResult<A::Value> {
        hardware_wait_untimed(self.config, atomic, expected)
    }

    #[inline]
    fn wait_raw<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
        deadline: Option<Deadline>,
    ) -> WaitTimeoutResult<A::Value> {
        hardware_wait(self.config, atomic, expected, deadline)
    }
}

impl_wait_strategy!(HardwareWait);

/// Polls briefly, then uses the preflight-selected hardware-address wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpinThenHardwareWait {
    spin_iterations: usize,
    hardware: HardwareWait,
}

impl SpinThenHardwareWait {
    /// Cheaply constructs a hybrid strategy from cached successful preflight.
    ///
    /// ```no_run
    /// use snoozer::{HardwareWait, SpinThenHardwareWait};
    ///
    /// HardwareWait::preflight()?;
    /// let strategy = SpinThenHardwareWait::new(32)?;
    /// # Ok::<(), snoozer::HardwareWaitError>(())
    /// ```
    pub fn new(spin_iterations: usize) -> Result<Self, HardwareWaitError> {
        HardwareWait::new().map(|hardware| Self {
            spin_iterations,
            hardware,
        })
    }

    /// Returns the configured spin prefix length.
    ///
    /// ```no_run
    /// use snoozer::{HardwareWait, SpinThenHardwareWait};
    ///
    /// HardwareWait::preflight()?;
    /// assert_eq!(SpinThenHardwareWait::new(32)?.spin_iterations(), 32);
    /// # Ok::<(), snoozer::HardwareWaitError>(())
    /// ```
    #[must_use]
    pub const fn spin_iterations(self) -> usize {
        self.spin_iterations
    }
}

impl StrategyImpl for SpinThenHardwareWait {
    fn strategy(&self) -> Strategy {
        Strategy::SpinThenHardwareWait
    }

    #[inline]
    fn wait_raw_untimed<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
    ) -> WaitTimeoutResult<A::Value> {
        if let Some(result) = spin_prefix(atomic, expected, self.spin_iterations, None) {
            return result;
        }
        self.hardware.wait_raw_untimed(atomic, expected)
    }

    #[inline]
    fn wait_raw<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
        deadline: Option<Deadline>,
    ) -> WaitTimeoutResult<A::Value> {
        if let Some(result) = spin_prefix(atomic, expected, self.spin_iterations, deadline) {
            return result;
        }
        self.hardware.wait_raw(atomic, expected, deadline)
    }
}

impl_wait_strategy!(SpinThenHardwareWait);

#[cfg(feature = "benchmark-only")]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AmdMwaitxC1 {
    config: AmdConfig,
}

#[cfg(feature = "benchmark-only")]
impl AmdMwaitxC1 {
    pub fn new() -> Result<Self, HardwareWaitError> {
        match HardwareWait::new()?.config {
            HardwareConfig::Amd(config) => Ok(Self { config }),
            HardwareConfig::Intel(_) => Err(HardwareWaitError::Unsupported(UnsupportedStrategy {
                strategy: Strategy::HardwareWait,
                reason: UnsupportedReason::UnsupportedCpuVendor,
            })),
        }
    }
}

#[cfg(feature = "benchmark-only")]
impl StrategyImpl for AmdMwaitxC1 {
    fn strategy(&self) -> Strategy {
        Strategy::HardwareWait
    }

    #[inline]
    fn wait_raw_untimed<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
    ) -> WaitTimeoutResult<A::Value> {
        mwaitx_raw_hardware(&self.config, C1_STATE_HINT, atomic, expected)
    }

    #[inline]
    fn wait_raw<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
        deadline: Option<Deadline>,
    ) -> WaitTimeoutResult<A::Value> {
        match deadline {
            Some(deadline) => {
                mwaitx_raw_timeout_hardware(&self.config, C1_STATE_HINT, atomic, expected, deadline)
            }
            None => self.wait_raw_untimed(atomic, expected),
        }
    }
}

#[cfg(feature = "benchmark-only")]
impl_wait_strategy!(AmdMwaitxC1);

fn spin_prefix<A: WaitableAtomic>(
    atomic: &A,
    expected: A::Value,
    spin_iterations: usize,
    deadline: Option<Deadline>,
) -> Option<WaitTimeoutResult<A::Value>> {
    let initially_observed = atomic.__load_acquire();
    if initially_observed != expected {
        return Some(WaitTimeoutResult::Changed(initially_observed));
    }
    if deadline.is_some_and(|value| value.remaining().is_none()) {
        return Some(WaitTimeoutResult::TimedOut);
    }

    for _ in 0..spin_iterations {
        std::hint::spin_loop();
        let observed = atomic.__load_acquire();
        if observed != expected {
            return Some(WaitTimeoutResult::Changed(observed));
        }
        if deadline.is_some_and(|value| value.remaining().is_none()) {
            return Some(WaitTimeoutResult::TimedOut);
        }
    }
    None
}

fn classify_after_wait<A: WaitableAtomic>(
    atomic: &A,
    expected: A::Value,
    deadline: Option<Deadline>,
) -> WaitTimeoutResult<A::Value> {
    let observed = atomic.__load_acquire();
    if observed != expected {
        WaitTimeoutResult::Changed(observed)
    } else if deadline.is_some_and(|value| value.remaining().is_none()) {
        WaitTimeoutResult::TimedOut
    } else {
        WaitTimeoutResult::Unclassified
    }
}

#[inline]
fn hardware_wait_untimed<A: WaitableAtomic>(
    config: HardwareConfig,
    atomic: &A,
    expected: A::Value,
) -> WaitTimeoutResult<A::Value> {
    hardware_wait(config, atomic, expected, None)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[inline]
fn hardware_wait<A: WaitableAtomic>(
    config: HardwareConfig,
    atomic: &A,
    expected: A::Value,
    deadline: Option<Deadline>,
) -> WaitTimeoutResult<A::Value> {
    match (config, deadline) {
        (HardwareConfig::Amd(config), Some(deadline)) => {
            mwaitx_raw_timeout_hardware(&config, NO_C_STATE_HINT, atomic, expected, deadline)
        }
        (HardwareConfig::Amd(config), None) => {
            mwaitx_raw_hardware(&config, NO_C_STATE_HINT, atomic, expected)
        }
        (HardwareConfig::Intel(config), Some(deadline)) => {
            umwait_raw_timeout_hardware(&config, atomic, expected, deadline)
        }
        (HardwareConfig::Intel(config), None) => umwait_raw_hardware(&config, atomic, expected),
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn hardware_wait_unsupported<A: WaitableAtomic>(
    config: HardwareConfig,
    _atomic: &A,
    _expected: A::Value,
    _deadline: Option<Deadline>,
) -> WaitTimeoutResult<A::Value> {
    match config {
        HardwareConfig::Amd(config) => match config.never {},
        HardwareConfig::Intel(config) => match config.never {},
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn mwaitx_raw_hardware<A: WaitableAtomic>(
    config: &AmdConfig,
    c_state_hint: u32,
    atomic: &A,
    expected: A::Value,
) -> WaitTimeoutResult<A::Value> {
    mwaitx_untimed_protocol(
        config,
        c_state_hint,
        atomic,
        expected,
        arch::monitorx,
        arch::mwaitx,
    )
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn mwaitx_raw_timeout_hardware<A: WaitableAtomic>(
    config: &AmdConfig,
    c_state_hint: u32,
    atomic: &A,
    expected: A::Value,
    deadline: Deadline,
) -> WaitTimeoutResult<A::Value> {
    mwaitx_timed_protocol(
        config,
        c_state_hint,
        atomic,
        expected,
        deadline,
        arch::monitorx,
        arch::mwaitx,
    )
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn umwait_raw_hardware<A: WaitableAtomic>(
    config: &IntelConfig,
    atomic: &A,
    expected: A::Value,
) -> WaitTimeoutResult<A::Value> {
    umwait_untimed_protocol(
        config,
        atomic,
        expected,
        arch::umonitor,
        umwait_deadline,
        arch::umwait_c01,
    )
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn umwait_raw_timeout_hardware<A: WaitableAtomic>(
    config: &IntelConfig,
    atomic: &A,
    expected: A::Value,
    deadline: Deadline,
) -> WaitTimeoutResult<A::Value> {
    umwait_timed_protocol(
        config,
        atomic,
        expected,
        deadline,
        arch::umonitor,
        umwait_deadline,
        arch::umwait_c01,
    )
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn mwaitx_untimed_protocol<A, M, W>(
    config: &AmdConfig,
    c_state_hint: u32,
    atomic: &A,
    expected: A::Value,
    arm: M,
    wait: W,
) -> WaitTimeoutResult<A::Value>
where
    A: WaitableAtomic,
    M: FnOnce(*const ()),
    W: FnOnce(u32, u32),
{
    let observed = atomic.__load_acquire();
    if observed != expected {
        return WaitTimeoutResult::Changed(observed);
    }

    arm(atomic.__monitored_address());

    let observed = atomic.__load_acquire();
    if observed != expected {
        return WaitTimeoutResult::Changed(observed);
    }

    wait(config.safety_timeout_cycles, c_state_hint);
    classify_after_wait(atomic, expected, None)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn mwaitx_timed_protocol<A, M, W>(
    config: &AmdConfig,
    c_state_hint: u32,
    atomic: &A,
    expected: A::Value,
    deadline: Deadline,
    arm: M,
    wait: W,
) -> WaitTimeoutResult<A::Value>
where
    A: WaitableAtomic,
    M: FnOnce(*const ()),
    W: FnOnce(u32, u32),
{
    let observed = atomic.__load_acquire();
    if observed != expected {
        return WaitTimeoutResult::Changed(observed);
    }
    if deadline.remaining().is_none() {
        return WaitTimeoutResult::TimedOut;
    }

    arm(atomic.__monitored_address());

    let observed = atomic.__load_acquire();
    if observed != expected {
        return WaitTimeoutResult::Changed(observed);
    }
    let Some(remaining) = deadline.remaining() else {
        return WaitTimeoutResult::TimedOut;
    };

    wait(mwaitx_timeout_cycles(config, remaining), c_state_hint);
    classify_after_wait(atomic, expected, Some(deadline))
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn mwaitx_timeout_cycles(config: &AmdConfig, remaining: Duration) -> u32 {
    if remaining < MWAITX_SAFETY_TIMEOUT {
        duration_to_cycles(remaining, config.timer_hz)
    } else {
        config.safety_timeout_cycles
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn umwait_untimed_protocol<A, M, D, W>(
    config: &IntelConfig,
    atomic: &A,
    expected: A::Value,
    arm: M,
    absolute_deadline: D,
    wait: W,
) -> WaitTimeoutResult<A::Value>
where
    A: WaitableAtomic,
    M: FnOnce(*const ()),
    D: FnOnce(u64) -> u64,
    W: FnOnce(u64),
{
    let observed = atomic.__load_acquire();
    if observed != expected {
        return WaitTimeoutResult::Changed(observed);
    }

    arm(atomic.__monitored_address());

    let observed = atomic.__load_acquire();
    if observed != expected {
        return WaitTimeoutResult::Changed(observed);
    }

    wait(absolute_deadline(config.safety_timeout_cycles));
    classify_after_wait(atomic, expected, None)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn umwait_timed_protocol<A, M, D, W>(
    config: &IntelConfig,
    atomic: &A,
    expected: A::Value,
    deadline: Deadline,
    arm: M,
    absolute_deadline: D,
    wait: W,
) -> WaitTimeoutResult<A::Value>
where
    A: WaitableAtomic,
    M: FnOnce(*const ()),
    D: FnOnce(u64) -> u64,
    W: FnOnce(u64),
{
    let observed = atomic.__load_acquire();
    if observed != expected {
        return WaitTimeoutResult::Changed(observed);
    }
    if deadline.remaining().is_none() {
        return WaitTimeoutResult::TimedOut;
    }

    arm(atomic.__monitored_address());

    let observed = atomic.__load_acquire();
    if observed != expected {
        return WaitTimeoutResult::Changed(observed);
    }
    let Some(remaining) = deadline.remaining() else {
        return WaitTimeoutResult::TimedOut;
    };
    let cycles = umwait_timeout_cycles(config, remaining);
    wait(absolute_deadline(cycles));
    classify_after_wait(atomic, expected, Some(deadline))
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn umwait_timeout_cycles(config: &IntelConfig, remaining: Duration) -> u64 {
    if remaining < UMWAIT_SAFETY_TIMEOUT {
        duration_to_cycles_u64(remaining, config.timer_hz)
    } else {
        config.safety_timeout_cycles
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn umwait_deadline(timeout_cycles: u64) -> u64 {
    arch::read_tsc_ordered()
        .0
        .saturating_add(timeout_cycles.max(1))
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn mwaitx_raw_hardware<A: WaitableAtomic>(
    config: &AmdConfig,
    _c_state_hint: u32,
    _atomic: &A,
    _expected: A::Value,
) -> WaitTimeoutResult<A::Value> {
    match config.never {}
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn mwaitx_raw_timeout_hardware<A: WaitableAtomic>(
    config: &AmdConfig,
    _c_state_hint: u32,
    _atomic: &A,
    _expected: A::Value,
    _deadline: Deadline,
) -> WaitTimeoutResult<A::Value> {
    match config.never {}
}

fn unsupported(reason: UnsupportedReason) -> HardwareWaitError {
    HardwareWaitError::Unsupported(UnsupportedStrategy {
        strategy: Strategy::HardwareWait,
        reason,
    })
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn prepare_hardware_config<U, A, I>(
    detected: &crate::Capabilities,
    userspace_tsc_is_enabled: U,
    calibrate_amd_timer_hz: A,
    calibrate_intel_timer_hz: I,
) -> Result<HardwareConfig, HardwareWaitError>
where
    U: FnOnce() -> bool,
    A: FnOnce() -> Option<u64>,
    I: FnOnce() -> Option<u64>,
{
    let backend = select_backend(detected).map_err(unsupported)?;
    require_shared_timer_capabilities(detected)?;
    if !userspace_tsc_is_enabled() {
        return Err(unsupported(UnsupportedReason::TscAccessDisabled));
    }
    match backend {
        HardwareBackend::AmdMwaitx => {
            let timer_hz = calibrate_amd_timer_hz()
                .ok_or_else(|| unsupported(UnsupportedReason::UnstableTimerCalibration))?;
            Ok(HardwareConfig::Amd(AmdConfig::new(timer_hz)))
        }
        HardwareBackend::IntelUmwait => {
            let timer_hz = calibrate_intel_timer_hz()
                .ok_or_else(|| unsupported(UnsupportedReason::UnstableTimerCalibration))?;
            Ok(HardwareConfig::Intel(IntelConfig::new(timer_hz)))
        }
    }
}

fn select_backend(detected: &crate::Capabilities) -> Result<HardwareBackend, UnsupportedReason> {
    if !detected.supported_target {
        Err(UnsupportedReason::UnsupportedTarget)
    } else if detected.amd_vendor == detected.intel_vendor {
        Err(UnsupportedReason::UnsupportedCpuVendor)
    } else if detected.amd_vendor && !detected.monitorx_mwaitx {
        Err(UnsupportedReason::MissingMonitorxMwaitx)
    } else if detected.amd_vendor {
        Ok(HardwareBackend::AmdMwaitx)
    } else if detected.intel_vendor && !detected.waitpkg {
        Err(UnsupportedReason::MissingWaitpkg)
    } else {
        Ok(HardwareBackend::IntelUmwait)
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn require_shared_timer_capabilities(
    detected: &crate::Capabilities,
) -> Result<(), HardwareWaitError> {
    if !detected.invariant_tsc {
        Err(unsupported(UnsupportedReason::MissingInvariantTsc))
    } else if !detected.rdtscp {
        Err(unsupported(UnsupportedReason::MissingRdtscp))
    } else {
        Ok(())
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn run_hardware_preflight() -> Result<PreparedHardwareWait, HardwareWaitError> {
    prepare_and_probe_hardware_wait(
        || {
            prepare_hardware_config(
                capabilities(),
                arch::userspace_tsc_is_enabled,
                arch::calibrate_amd_timer_hz,
                arch::calibrate_intel_timer_hz,
            )
        },
        functional_preflight,
    )
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn run_hardware_preflight() -> Result<PreparedHardwareWait, HardwareWaitError> {
    let detected = capabilities();
    let _backend = select_backend(detected).map_err(unsupported)?;
    Err(unsupported(UnsupportedReason::UnsupportedCpuVendor))
}

#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), test))]
fn prepare_and_probe_hardware_wait<P, F>(
    prepare: P,
    probe: F,
) -> Result<PreparedHardwareWait, HardwareWaitError>
where
    P: FnOnce() -> Result<HardwareConfig, HardwareWaitError>,
    F: FnOnce(HardwareConfig) -> Result<PreflightReport, HardwareWaitError>,
{
    let config = prepare()?;
    let report = probe(config)?;
    Ok(PreparedHardwareWait { config, report })
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[repr(align(128))]
struct PreflightTarget(std::sync::atomic::AtomicU32);

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[repr(align(128))]
struct PreflightHandoff(std::sync::atomic::AtomicBool);

#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PreflightTrialEvidence {
    helper_overlapped: bool,
    verified_store_wake: bool,
}

#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), test))]
fn classify_preflight_trial(
    published_at: Option<Instant>,
    wait_finished: Instant,
    observed: u32,
    elapsed: Duration,
    wake_threshold: Duration,
) -> PreflightTrialEvidence {
    let helper_overlapped = published_at.is_some_and(|published| published <= wait_finished);
    PreflightTrialEvidence {
        helper_overlapped,
        verified_store_wake: helper_overlapped && observed != 0 && elapsed < wake_threshold,
    }
}

#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), test))]
fn conclude_preflight(
    backend: HardwareBackend,
    attempts: u32,
    verified_store_wakes: u32,
    helper_overlaps: u32,
    baseline_wait: Duration,
) -> Result<PreflightReport, HardwareWaitError> {
    if helper_overlaps == 0 {
        return Err(HardwareWaitError::PreflightFailed {
            backend,
            reason: PreflightFailure::Inconclusive,
        });
    }
    if verified_store_wakes < PREFLIGHT_REQUIRED_STORE_OBSERVATIONS {
        return Err(HardwareWaitError::PreflightFailed {
            backend,
            reason: PreflightFailure::StoreWakeNotObserved,
        });
    }

    Ok(PreflightReport {
        backend,
        attempts,
        verified_store_wakes,
        baseline_wait,
    })
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn functional_preflight(config: HardwareConfig) -> Result<PreflightReport, HardwareWaitError> {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    let baseline_target = PreflightTarget(std::sync::atomic::AtomicU32::new(0));
    let mut baselines = [Duration::ZERO; PREFLIGHT_BASELINE_ATTEMPTS as usize];
    for elapsed in &mut baselines {
        let started = Instant::now();
        let _ = hardware_wait_untimed(config, &baseline_target.0, 0);
        *elapsed = started.elapsed();
    }
    baselines.sort_unstable();
    let baseline_wait = baselines[baselines.len() / 2];
    if baseline_wait < PREFLIGHT_MIN_BLOCK {
        return Err(HardwareWaitError::PreflightFailed {
            backend: config.backend(),
            reason: PreflightFailure::WaitDidNotBlock,
        });
    }

    let wake_threshold = baseline_wait.saturating_mul(3) / 4;
    let producer_delay = (baseline_wait / 16)
        .max(Duration::from_micros(1))
        .min(Duration::from_micros(20));
    let mut verified_store_wakes = 0;
    let mut helper_overlaps = 0;
    let mut attempts = 0;

    for _ in 0..PREFLIGHT_STORE_ATTEMPTS {
        attempts += 1;
        let target = Arc::new(PreflightTarget(std::sync::atomic::AtomicU32::new(0)));
        let handoff = Arc::new(PreflightHandoff(std::sync::atomic::AtomicBool::new(false)));
        let producer_target = Arc::clone(&target);
        let producer_handoff = Arc::clone(&handoff);
        let (producer_result, receive_result) = std::sync::mpsc::sync_channel(1);
        drop(thread::spawn(move || {
            let deadline = Instant::now() + PREFLIGHT_HELPER_DEADLINE;
            while !producer_handoff.0.load(Ordering::Acquire) {
                if Instant::now() >= deadline {
                    let _send_failed = producer_result.send(None).is_err();
                    return;
                }
                std::hint::spin_loop();
            }
            let delay_started = Instant::now();
            while delay_started.elapsed() < producer_delay {
                std::hint::spin_loop();
            }
            producer_target.0.store(1, Ordering::Release);
            let _send_failed = producer_result.send(Some(Instant::now())).is_err();
        }));

        let started = Instant::now();
        let observed = preflight_wait_once(config, &target.0, &handoff.0);
        let wait_finished = Instant::now();
        let elapsed = wait_finished.duration_since(started);
        let published_at = receive_result
            .recv_timeout(PREFLIGHT_HELPER_DEADLINE)
            .ok()
            .flatten();
        let evidence = classify_preflight_trial(
            published_at,
            wait_finished,
            observed,
            elapsed,
            wake_threshold,
        );
        if evidence.helper_overlapped {
            helper_overlaps += 1;
        }
        if evidence.verified_store_wake {
            verified_store_wakes += 1;
            if verified_store_wakes >= PREFLIGHT_REQUIRED_STORE_OBSERVATIONS {
                break;
            }
        }
    }

    conclude_preflight(
        config.backend(),
        attempts,
        verified_store_wakes,
        helper_overlaps,
        baseline_wait,
    )
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn preflight_wait_once(
    config: HardwareConfig,
    target: &std::sync::atomic::AtomicU32,
    handoff: &std::sync::atomic::AtomicBool,
) -> u32 {
    use std::sync::atomic::Ordering;

    let observed = target.load(Ordering::Acquire);
    if observed != 0 {
        return observed;
    }

    match config {
        HardwareConfig::Amd(config) => {
            arch::monitorx(target as *const _ as *const ());
            let observed = target.load(Ordering::Acquire);
            if observed != 0 {
                return observed;
            }
            handoff.store(true, Ordering::Release);
            let observed = target.load(Ordering::Acquire);
            if observed != 0 {
                return observed;
            }
            arch::mwaitx(config.safety_timeout_cycles, NO_C_STATE_HINT);
        }
        HardwareConfig::Intel(config) => {
            arch::umonitor(target as *const _ as *const ());
            let observed = target.load(Ordering::Acquire);
            if observed != 0 {
                return observed;
            }
            handoff.store(true, Ordering::Release);
            let observed = target.load(Ordering::Acquire);
            if observed != 0 {
                return observed;
            }
            arch::umwait_c01(umwait_deadline(config.safety_timeout_cycles));
        }
    }

    target.load(Ordering::Acquire)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn duration_to_cycles(duration: Duration, timer_hz: u64) -> u32 {
    let cycles = duration.as_nanos().saturating_mul(u128::from(timer_hz)) / NANOS_PER_SECOND;
    u32::try_from(cycles.clamp(1, u128::from(u32::MAX))).unwrap_or(u32::MAX)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn duration_to_cycles_u64(duration: Duration, timer_hz: u64) -> u64 {
    let cycles = duration.as_nanos().saturating_mul(u128::from(timer_hz)) / NANOS_PER_SECOND;
    u64::try_from(cycles.clamp(1, u128::from(u64::MAX))).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "../tests/support/gated_wait_strategy.rs"]
mod gated_wait_strategy;

#[cfg(test)]
pub(crate) use gated_wait_strategy::{TestGatePoint, TestGateStrategy, TestTimeoutGateStrategy};

#[cfg(test)]
#[path = "../tests/unit/wait_strategy_contract.rs"]
mod wait_strategy_contract;

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
#[path = "../tests/unit/amd_wait_protocol.rs"]
mod amd_wait_protocol;

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
#[path = "../tests/unit/intel_wait_protocol.rs"]
mod intel_wait_protocol;

#[cfg(test)]
#[path = "../tests/unit/hardware_preflight_state.rs"]
mod hardware_preflight_state;
