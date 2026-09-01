#![deny(unsafe_code)]

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
use std::convert::Infallible;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::arch;
use crate::{Strategy, UnsupportedReason, UnsupportedStrategy, WaitableAtomic, capabilities};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use self::mwaitx_raw_hardware as mwaitx_dispatch;
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
use self::mwaitx_raw_unsupported as mwaitx_dispatch;

const NO_C_STATE_HINT: u32 = 0x0f;
#[cfg(feature = "benchmark-only")]
const C1_STATE_HINT: u32 = 0;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const MWAITX_SAFETY_TIMEOUT: Duration = Duration::from_millis(1);
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const NANOS_PER_SECOND: u128 = 1_000_000_000;

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
    #[must_use]
    fn strategy(&self) -> Strategy;

    /// Waits only if the atomic equals `expected` and performs at most one
    /// strategy-specific blocking or yielding operation.
    ///
    /// [`WaitResult::Unclassified`] does not prove that work is available and
    /// does not synchronize with a producer. The caller must recheck its
    /// published state with an Acquire operation.
    #[must_use]
    fn wait_if_equal<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
    ) -> WaitResult<A::Value>;

    /// Absorbs unclassified wakes until an Acquire load observes a value
    /// different from `expected`.
    #[must_use]
    fn wait_until_different<A: WaitableAtomic>(&self, atomic: &A, expected: A::Value) -> A::Value;

    /// Performs one wait attempt bounded by `timeout`.
    #[must_use]
    fn wait_if_equal_timeout<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
        timeout: Duration,
    ) -> WaitTimeoutResult<A::Value>;

    /// Absorbs unclassified wakes until the value changes or `timeout`
    /// expires.
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
                match self.wait_raw(atomic, expected, None) {
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
                    if let WaitTimeoutResult::Changed(value) = self.wait_raw(atomic, expected, None)
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
    #[must_use]
    pub const fn new(spin_iterations: usize) -> Self {
        Self { spin_iterations }
    }

    /// Returns the configured spin prefix length.
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
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AmdConfig {
    never: Infallible,
}

/// AMD `MONITORX`/`MWAITX` using a hint that does not enter a CPU C-state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AmdMwaitx {
    config: AmdConfig,
}

impl AmdMwaitx {
    /// Constructs the strategy after all cached capability and timer guards
    /// succeed.
    pub fn new() -> Result<Self, UnsupportedStrategy> {
        amd_config(Strategy::AmdMwaitx).map(|config| Self { config })
    }
}

impl StrategyImpl for AmdMwaitx {
    fn strategy(&self) -> Strategy {
        Strategy::AmdMwaitx
    }

    #[inline]
    fn wait_raw<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
        deadline: Option<Deadline>,
    ) -> WaitTimeoutResult<A::Value> {
        mwaitx_dispatch(&self.config, NO_C_STATE_HINT, atomic, expected, deadline)
    }
}

impl_wait_strategy!(AmdMwaitx);

/// Polls briefly, then uses AMD `MONITORX`/`MWAITX` without entering a C-state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpinThenAmdMwaitx {
    spin_iterations: usize,
    amd: AmdMwaitx,
}

impl SpinThenAmdMwaitx {
    /// Constructs the strategy after all cached capability and timer guards
    /// succeed.
    pub fn new(spin_iterations: usize) -> Result<Self, UnsupportedStrategy> {
        AmdMwaitx::new().map(|amd| Self {
            spin_iterations,
            amd,
        })
    }

    /// Returns the configured spin prefix length.
    #[must_use]
    pub const fn spin_iterations(self) -> usize {
        self.spin_iterations
    }
}

impl StrategyImpl for SpinThenAmdMwaitx {
    fn strategy(&self) -> Strategy {
        Strategy::SpinThenAmdMwaitx
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
        mwaitx_dispatch(
            &self.amd.config,
            NO_C_STATE_HINT,
            atomic,
            expected,
            deadline,
        )
    }
}

impl_wait_strategy!(SpinThenAmdMwaitx);

#[cfg(feature = "benchmark-only")]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AmdMwaitxC1 {
    config: AmdConfig,
}

#[cfg(feature = "benchmark-only")]
impl AmdMwaitxC1 {
    pub fn new() -> Result<Self, UnsupportedStrategy> {
        amd_config(Strategy::AmdMwaitx).map(|config| Self { config })
    }
}

#[cfg(feature = "benchmark-only")]
impl StrategyImpl for AmdMwaitxC1 {
    fn strategy(&self) -> Strategy {
        Strategy::AmdMwaitx
    }

    #[inline]
    fn wait_raw<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
        deadline: Option<Deadline>,
    ) -> WaitTimeoutResult<A::Value> {
        mwaitx_dispatch(&self.config, C1_STATE_HINT, atomic, expected, deadline)
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

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn mwaitx_raw_hardware<A: WaitableAtomic>(
    config: &AmdConfig,
    c_state_hint: u32,
    atomic: &A,
    expected: A::Value,
    deadline: Option<Deadline>,
) -> WaitTimeoutResult<A::Value> {
    mwaitx_protocol(
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
fn mwaitx_protocol<A, M, W>(
    config: &AmdConfig,
    c_state_hint: u32,
    atomic: &A,
    expected: A::Value,
    deadline: Option<Deadline>,
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
    if deadline.is_some_and(|value| value.remaining().is_none()) {
        return WaitTimeoutResult::TimedOut;
    }

    arm(atomic.__monitored_address());

    let observed = atomic.__load_acquire();
    if observed != expected {
        return WaitTimeoutResult::Changed(observed);
    }
    let remaining = match deadline {
        Some(value) => match value.remaining() {
            Some(remaining) => remaining.min(MWAITX_SAFETY_TIMEOUT),
            None => return WaitTimeoutResult::TimedOut,
        },
        None => MWAITX_SAFETY_TIMEOUT,
    };

    wait(duration_to_cycles(remaining, config.timer_hz), c_state_hint);
    classify_after_wait(atomic, expected, deadline)
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn mwaitx_raw_unsupported<A: WaitableAtomic>(
    config: &AmdConfig,
    _c_state_hint: u32,
    _atomic: &A,
    _expected: A::Value,
    _deadline: Option<Deadline>,
) -> WaitTimeoutResult<A::Value> {
    match config.never {}
}

fn amd_config(strategy: Strategy) -> Result<AmdConfig, UnsupportedStrategy> {
    let detected = capabilities();
    if let Some(reason) = amd_unavailable_reason(detected) {
        return Err(UnsupportedStrategy { strategy, reason });
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        if let Some(timer_hz) = detected.mwaitx_timer_hz {
            return Ok(AmdConfig { timer_hz });
        }
    }

    // No supported-target path can reach this branch because the checks above
    // reject a missing calibration. Keeping the return typed avoids a panic if
    // a future target changes those checks incompletely.
    Err(UnsupportedStrategy {
        strategy,
        reason: UnsupportedReason::UnstableTimerCalibration,
    })
}

fn amd_unavailable_reason(detected: &crate::Capabilities) -> Option<UnsupportedReason> {
    if !detected.supported_target {
        Some(UnsupportedReason::UnsupportedTarget)
    } else if !detected.amd_vendor {
        Some(UnsupportedReason::NotAmd)
    } else if !detected.monitorx_mwaitx {
        Some(UnsupportedReason::MissingMonitorxMwaitx)
    } else if !detected.invariant_tsc {
        Some(UnsupportedReason::MissingInvariantTsc)
    } else if !detected.rdtscp {
        Some(UnsupportedReason::MissingRdtscp)
    } else if detected.mwaitx_timer_hz.is_none() {
        Some(UnsupportedReason::UnstableTimerCalibration)
    } else {
        None
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn duration_to_cycles(duration: Duration, timer_hz: u64) -> u32 {
    let cycles = duration.as_nanos().saturating_mul(u128::from(timer_hz)) / NANOS_PER_SECOND;
    u32::try_from(cycles.clamp(1, u128::from(u32::MAX))).unwrap_or(u32::MAX)
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum TestGatePoint {
    AfterArmBeforeRecheck,
    AfterRecheckBeforeWait,
    DuringWait,
}

#[cfg(test)]
pub(crate) struct TestGateStrategy {
    point: TestGatePoint,
    reached: std::sync::Arc<std::sync::Barrier>,
    released: std::sync::Arc<std::sync::Barrier>,
}

#[cfg(test)]
impl TestGateStrategy {
    pub(crate) fn new(
        point: TestGatePoint,
        reached: std::sync::Arc<std::sync::Barrier>,
        released: std::sync::Arc<std::sync::Barrier>,
    ) -> Self {
        Self {
            point,
            reached,
            released,
        }
    }

    fn gate(&self) {
        self.reached.wait();
        self.released.wait();
    }
}

#[cfg(test)]
impl StrategyImpl for TestGateStrategy {
    fn strategy(&self) -> Strategy {
        Strategy::BusySpin
    }

    fn wait_raw<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
        _deadline: Option<Deadline>,
    ) -> WaitTimeoutResult<A::Value> {
        let initially_observed = atomic.__load_acquire();
        if initially_observed != expected {
            return WaitTimeoutResult::Changed(initially_observed);
        }

        if matches!(self.point, TestGatePoint::AfterArmBeforeRecheck) {
            self.gate();
        }

        let rechecked = atomic.__load_acquire();
        if rechecked != expected {
            return WaitTimeoutResult::Changed(rechecked);
        }

        if matches!(
            self.point,
            TestGatePoint::AfterRecheckBeforeWait | TestGatePoint::DuringWait
        ) {
            self.gate();
        }

        classify_after_wait(atomic, expected, None)
    }
}

#[cfg(test)]
impl_wait_strategy!(TestGateStrategy);

#[cfg(test)]
pub(crate) struct TestBudgetStrategy {
    observed_budgets: std::sync::Arc<std::sync::Mutex<Vec<Duration>>>,
}

#[cfg(test)]
impl TestBudgetStrategy {
    pub(crate) fn new() -> (Self, std::sync::Arc<std::sync::Mutex<Vec<Duration>>>) {
        let observed_budgets = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Self {
                observed_budgets: std::sync::Arc::clone(&observed_budgets),
            },
            observed_budgets,
        )
    }
}

#[cfg(test)]
impl StrategyImpl for TestBudgetStrategy {
    fn strategy(&self) -> Strategy {
        Strategy::BusySpin
    }

    fn wait_raw<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
        deadline: Option<Deadline>,
    ) -> WaitTimeoutResult<A::Value> {
        let budget = deadline.map(|value| value.timeout).unwrap_or_default();
        match self.observed_budgets.lock() {
            Ok(mut observed) => observed.push(budget),
            Err(poisoned) => poisoned.into_inner().push(budget),
        }
        let observed = atomic.__load_acquire();
        if observed == expected {
            WaitTimeoutResult::Unclassified
        } else {
            WaitTimeoutResult::Changed(observed)
        }
    }
}

#[cfg(test)]
impl_wait_strategy!(TestBudgetStrategy);

#[cfg(test)]
mod tests {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

    use super::*;

    #[test]
    fn deadlines_report_a_bounded_remaining_interval_and_expire() {
        let timeout = Duration::from_secs(60);
        let deadline = Deadline::new(timeout);
        let remaining = deadline
            .remaining()
            .expect("fresh deadline must remain live");
        assert!(remaining <= timeout);
        assert!(!remaining.is_zero());

        let expired = Deadline {
            started: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .expect("one second must be representable"),
            timeout: Duration::from_millis(1),
        };
        assert_eq!(expired.remaining(), None);
    }

    #[test]
    fn public_strategy_identity_and_spin_configuration_are_exact() {
        let yielding = SpinThenYield::new(17);
        assert_eq!(yielding.spin_iterations(), 17);
        assert_eq!(WaitStrategy::strategy(&BusySpin), Strategy::BusySpin);
        assert_eq!(WaitStrategy::strategy(&yielding), Strategy::SpinThenYield);

        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            let amd = AmdMwaitx {
                config: AmdConfig { timer_hz: 1 },
            };
            let hybrid = SpinThenAmdMwaitx {
                spin_iterations: 19,
                amd,
            };
            assert_eq!(hybrid.spin_iterations(), 19);
            assert_eq!(StrategyImpl::strategy(&amd), Strategy::AmdMwaitx);
            assert_eq!(StrategyImpl::strategy(&hybrid), Strategy::SpinThenAmdMwaitx);

            let already_changed = AtomicU32::new(2);
            assert_eq!(
                WaitStrategy::wait_if_equal(&amd, &already_changed, 1),
                WaitResult::Changed(2)
            );
            assert_eq!(
                WaitStrategy::wait_if_equal(&hybrid, &already_changed, 1),
                WaitResult::Changed(2)
            );

            #[cfg(feature = "benchmark-only")]
            {
                let diagnostic = AmdMwaitxC1 { config: amd.config };
                assert_eq!(StrategyImpl::strategy(&diagnostic), Strategy::AmdMwaitx);
                assert_eq!(
                    WaitStrategy::wait_if_equal(&diagnostic, &already_changed, 1),
                    WaitResult::Changed(2)
                );
            }
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn production_mwaitx_protocol_arms_rechecks_waits_and_classifies() {
        let config = AmdConfig {
            timer_hz: 1_000_000_000,
        };

        let changed_during_arm = AtomicU32::new(0);
        let waited_after_arm_change = AtomicBool::new(false);
        assert_eq!(
            mwaitx_protocol(
                &config,
                NO_C_STATE_HINT,
                &changed_during_arm,
                0,
                None,
                |_| changed_during_arm.store(1, Ordering::Release),
                |_, _| waited_after_arm_change.store(true, Ordering::Relaxed),
            ),
            WaitTimeoutResult::Changed(1)
        );
        assert!(!waited_after_arm_change.load(Ordering::Relaxed));

        let changed_during_wait = AtomicU32::new(0);
        let observed_cycles = AtomicU32::new(0);
        let observed_hint = AtomicU32::new(0);
        assert_eq!(
            mwaitx_protocol(
                &config,
                NO_C_STATE_HINT,
                &changed_during_wait,
                0,
                None,
                |_| {},
                |cycles, hint| {
                    observed_cycles.store(cycles, Ordering::Relaxed);
                    observed_hint.store(hint, Ordering::Relaxed);
                    changed_during_wait.store(1, Ordering::Release);
                },
            ),
            WaitTimeoutResult::Changed(1)
        );
        assert_eq!(observed_cycles.load(Ordering::Relaxed), 1_000_000);
        assert_eq!(observed_hint.load(Ordering::Relaxed), NO_C_STATE_HINT);

        let unchanged = AtomicU32::new(0);
        assert_eq!(
            mwaitx_protocol(
                &config,
                NO_C_STATE_HINT,
                &unchanged,
                0,
                None,
                |_| {},
                |_, _| {},
            ),
            WaitTimeoutResult::Unclassified
        );

        let armed_after_timeout = AtomicBool::new(false);
        assert_eq!(
            mwaitx_protocol(
                &config,
                NO_C_STATE_HINT,
                &unchanged,
                0,
                Some(Deadline::new(Duration::ZERO)),
                |_| armed_after_timeout.store(true, Ordering::Relaxed),
                |_, _| {},
            ),
            WaitTimeoutResult::TimedOut
        );
        assert!(!armed_after_timeout.load(Ordering::Relaxed));
    }

    #[test]
    fn pure_wait_classification_helpers_preserve_the_contract() {
        let changed = AtomicU32::new(7);
        assert_eq!(
            spin_prefix(&changed, 6, 0, None),
            Some(WaitTimeoutResult::Changed(7))
        );
        assert_eq!(
            classify_after_wait(&changed, 6, None),
            WaitTimeoutResult::Changed(7)
        );

        let equal = AtomicU32::new(7);
        assert_eq!(spin_prefix(&equal, 7, 0, None), None);
        assert_eq!(
            classify_after_wait(&equal, 7, None),
            WaitTimeoutResult::Unclassified
        );
        assert_eq!(
            spin_prefix(&equal, 7, 0, Some(Deadline::new(Duration::ZERO))),
            Some(WaitTimeoutResult::TimedOut)
        );
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn mwaitx_duration_conversion_is_bounded_and_scaled() {
        assert_eq!(duration_to_cycles(Duration::ZERO, 1_000_000_000), 1);
        assert_eq!(
            duration_to_cycles(Duration::from_nanos(37), 1_000_000_000),
            37
        );
        assert_eq!(
            duration_to_cycles(Duration::from_secs(u64::MAX), u64::MAX),
            u32::MAX
        );
    }

    #[test]
    fn raw_yield_exposes_an_unclassified_wake() {
        let value = AtomicU32::new(7);
        assert_eq!(
            SpinThenYield::new(0).wait_if_equal(&value, 7),
            WaitResult::Unclassified
        );
        assert_eq!(
            SpinThenYield::new(1).wait_if_equal(&value, 7),
            WaitResult::Unclassified
        );
    }

    #[test]
    fn both_atomic_widths_are_supported() {
        let small = AtomicU32::new(2);
        let large = AtomicU64::new(3);
        assert_eq!(
            SpinThenYield::new(0).wait_if_equal(&small, 1),
            WaitResult::Changed(2)
        );
        assert_eq!(
            SpinThenYield::new(0).wait_if_equal(&large, 1),
            WaitResult::Changed(3)
        );
    }

    #[test]
    fn filtered_wait_absorbs_unclassified_wakes() {
        let value = Arc::new(AtomicU32::new(0));
        let start = Arc::new(Barrier::new(2));
        let consumer_value = Arc::clone(&value);
        let consumer_start = Arc::clone(&start);
        let consumer = std::thread::spawn(move || {
            consumer_start.wait();
            SpinThenYield::new(0).wait_until_different(&*consumer_value, 0)
        });

        start.wait();
        value.store(1, Ordering::Release);
        match consumer.join() {
            Ok(value) => assert_eq!(value, 1),
            Err(_) => panic!("consumer thread panicked"),
        }
    }

    #[test]
    fn zero_timeout_still_reports_an_already_changed_value() {
        let value = AtomicU32::new(9);
        assert_eq!(
            BusySpin.wait_until_different_timeout(&value, 8, Duration::ZERO),
            WaitUntilTimeoutResult::Changed(9)
        );
        assert_eq!(
            BusySpin.wait_until_different_timeout(&value, 9, Duration::ZERO),
            WaitUntilTimeoutResult::TimedOut
        );
    }

    #[test]
    fn forced_capability_failures_are_typed_without_executing_assembly() {
        let supported = crate::Capabilities {
            supported_target: true,
            amd_vendor: true,
            monitorx_mwaitx: true,
            invariant_tsc: true,
            rdtscp: true,
            mwaitx_timer_hz: Some(1),
        };
        assert_eq!(amd_unavailable_reason(&supported), None);

        let cases = [
            (
                crate::Capabilities {
                    supported_target: false,
                    ..supported
                },
                UnsupportedReason::UnsupportedTarget,
            ),
            (
                crate::Capabilities {
                    amd_vendor: false,
                    ..supported
                },
                UnsupportedReason::NotAmd,
            ),
            (
                crate::Capabilities {
                    monitorx_mwaitx: false,
                    ..supported
                },
                UnsupportedReason::MissingMonitorxMwaitx,
            ),
            (
                crate::Capabilities {
                    invariant_tsc: false,
                    ..supported
                },
                UnsupportedReason::MissingInvariantTsc,
            ),
            (
                crate::Capabilities {
                    rdtscp: false,
                    ..supported
                },
                UnsupportedReason::MissingRdtscp,
            ),
            (
                crate::Capabilities {
                    mwaitx_timer_hz: None,
                    ..supported
                },
                UnsupportedReason::UnstableTimerCalibration,
            ),
        ];

        for (capabilities, expected) in cases {
            assert_eq!(amd_unavailable_reason(&capabilities), Some(expected));
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn monitored_store_after_recheck_prevents_the_long_safety_wait() {
        const TEST_SAFETY_TIMEOUT: Duration = Duration::from_millis(200);
        const DISCRIMINATING_BOUND: Duration = Duration::from_millis(100);

        #[repr(align(128))]
        struct IsolatedState(AtomicU32);

        let Ok(strategy) = AmdMwaitx::new() else {
            return;
        };
        let state = Arc::new(IsolatedState(AtomicU32::new(0)));
        let producer_state = Arc::clone(&state);
        let producer_may_store = Arc::new(AtomicBool::new(false));
        let producer_has_stored = Arc::new(AtomicBool::new(false));
        let producer_gate = Arc::clone(&producer_may_store);
        let producer_done = Arc::clone(&producer_has_stored);
        let producer = std::thread::spawn(move || {
            while !producer_gate.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            producer_state.0.store(1, Ordering::Release);
            producer_done.store(true, Ordering::Release);
        });

        arch::monitorx(state.0.__monitored_address());
        assert_eq!(state.0.load(Ordering::Acquire), 0);
        producer_may_store.store(true, Ordering::Release);
        while !producer_has_stored.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }

        let started = Instant::now();
        arch::mwaitx(
            duration_to_cycles(TEST_SAFETY_TIMEOUT, strategy.config.timer_hz),
            NO_C_STATE_HINT,
        );
        let elapsed = started.elapsed();

        assert_eq!(state.0.load(Ordering::Acquire), 1);
        assert!(
            elapsed < DISCRIMINATING_BOUND,
            "monitored store did not wake MWAITX before its safety timer: {elapsed:?}"
        );
        assert!(producer.join().is_ok());
    }
}
