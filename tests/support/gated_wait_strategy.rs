use super::*;

#[derive(Clone, Copy)]
pub(crate) enum TestGatePoint {
    AfterArmBeforeRecheck,
    AfterRecheckBeforeWait,
    DuringWait,
}

pub(crate) struct TestGateStrategy {
    point: TestGatePoint,
    reached: std::sync::Arc<std::sync::Barrier>,
    released: std::sync::Arc<std::sync::Barrier>,
}

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

impl_wait_strategy!(TestGateStrategy);

pub(crate) struct TestTimeoutGateStrategy {
    reached: std::sync::Arc<std::sync::Barrier>,
    released: std::sync::Arc<std::sync::Barrier>,
}

impl TestTimeoutGateStrategy {
    pub(crate) fn new(
        reached: std::sync::Arc<std::sync::Barrier>,
        released: std::sync::Arc<std::sync::Barrier>,
    ) -> Self {
        Self { reached, released }
    }
}

impl StrategyImpl for TestTimeoutGateStrategy {
    fn strategy(&self) -> Strategy {
        Strategy::BusySpin
    }

    fn wait_raw<A: WaitableAtomic>(
        &self,
        atomic: &A,
        expected: A::Value,
        _deadline: Option<Deadline>,
    ) -> WaitTimeoutResult<A::Value> {
        let observed = atomic.__load_acquire();
        if observed != expected {
            return WaitTimeoutResult::Changed(observed);
        }

        self.reached.wait();
        self.released.wait();
        WaitTimeoutResult::TimedOut
    }
}

impl_wait_strategy!(TestTimeoutGateStrategy);
