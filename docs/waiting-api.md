# Waiting API

This page helps library users choose an operation. It defines what a return means; implementation
details belong in [Architecture](architecture.md), and the conditions that make the operations
safe belong in [Safety](safety.md).

## The two return contracts

Snoozer deliberately exposes two levels:

| Contract | Direct atomic operation | Parker operation      | Meaning of return                                                                                                             |
| -------- | ----------------------- | --------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| Raw      | `wait_if_equal`         | `park`                | One strategy-specific attempt finished. `Unclassified` means it did not confirm the condition; recheck application state.    |
| Filtered | `wait_until_different`  | `park_until_notified` | The required state was observed with Acquire ordering.                                                                        |

The sealed `WaitableAtomic` trait is implemented for `AtomicU32` and `AtomicU64`.
Operations are methods on a concrete `WaitStrategy`:

```rust
strategy.wait_if_equal(&atomic, expected)
strategy.wait_until_different(&atomic, expected)
strategy.wait_if_equal_timeout(&atomic, expected, timeout)
strategy.wait_until_different_timeout(&atomic, expected, timeout)
```

An **unclassified result** means that a strategy-specific attempt ended without an Acquire
operation confirming the requested condition. For a yielding strategy, the scheduler may simply
have returned control while the value was still equal. For a hardware-assisted strategy, a
monitored store, the safety timer, an interrupt, a context switch, a hypervisor, another write in
the monitored granule, or another platform event may have ended the wait. There is no portable,
reliable reason that the caller can treat as proof of readiness.

The raw contract is useful when the caller already has a cheap condition check, or when an
occasional extra pass through its event loop is harmless. The filtered contract is easier to use
when returning early would only make the caller repeat the same check.

## Direct atomic waits

Direct waits observe a caller-owned atomic. A producer's ordinary state update can therefore be
the wake-producing store; no separate notification write is needed.

`wait_if_equal(atomic, expected)` returns `Changed` only after an Acquire load observes a value
other than `expected`. What it does while the value remains equal depends on the selected strategy:

- `BusySpin` keeps polling with a processor spin hint, so an unbounded call normally returns only
  after observing a change;
- `SpinThenYield` polls for its configured prefix, yields at most once, then classifies its Acquire
  recheck;
- hardware and hybrid hardware strategies use the selected native address-monitoring protocol
  after any spin prefix.

An address-monitoring attempt follows this sequence:

1. Load the atomic with Acquire ordering. Return immediately if it is not `expected`.
2. Arm the architecture's address monitor.
3. Load again with Acquire ordering. Return if the value changed while the monitor was being
   armed.
4. Perform at most one hardware wait and return when that wait ends.

If the final Acquire recheck still observes `expected`, the raw operation returns `Unclassified`.
That result does **not** prove that the atomic changed and does not, on its own, establish
synchronization with a producer. Load the published condition with Acquire ordering before
consuming associated data. Callers must handle `Unclassified` even when a selected strategy's
current implementation normally waits until it observes a change.

`wait_until_different(atomic, expected)` repeats the raw operation and the Acquire load until
it observes a different value, then returns that value. Its timed form returns either the changed
value or a timeout outcome; internal hardware safety-timer expirations are not public timeouts.

The raw results are `WaitResult::{Changed, Unclassified}` and
`WaitTimeoutResult::{Changed, Unclassified, TimedOut}`. The filtered timed result is
`WaitUntilTimeoutResult::{Changed, TimedOut}`.

### ABA behavior

A direct wait observes state, not history. Suppose the expected value is `10`, and a producer
changes it from `10` to `11` and back to `10` before the waiter reloads it. The filtered
operation is allowed to keep waiting because the current value again equals the expectation.

Use a generation counter when every transition matters. Define overflow behavior as part of the
application protocol; the waiting primitive cannot infer it.

## Single- and multi-producer Parker pairs

Both Parker pairs wrap the direct-wait protocol around an internal, coalescing notification token.
The prefix selects producer ownership and the cost of publishing that token:

| Constructor | Consumer | Producer contract | Publication operation | Choose it when |
| ----------- | -------- | ----------------- | --------------------- | -------------- |
| `single_pair` | `SingleParker` | One non-clonable `SingleUnparker`; `unpark` needs exclusive access | Release store | One producer owns the wake handle and minimum producer overhead matters |
| `multi_pair` | `MultiParker` | Clonable, shareable `MultiUnparker` | Release read-modify-write | Multiple producers can notify concurrently or the handle must be cloned |

Both Parker types are single-consumer. They can move between threads, but cannot be waited on
concurrently. The `multi` prefix means multiple producers, never multiple consumers.

The single-producer restriction lets `SingleUnparker::unpark` publish with an ordinary Release
store. `MultiUnparker::unpark` uses a Release read-modify-write even when the token is already set.
Consecutive producer RMWs form overlapping release sequences, so the consumer's Acquire token
consumption acquires every publication represented by the coalesced token.

The remaining behavior is shared:

- an available token makes a later park return without sleeping;
- at most one token is stored, so repeated notifications coalesce;
- consuming a token uses Acquire ordering;
- the consumer checks for a token before and after waiting.

`park` is the raw operation. It consumes a token if one is already available; otherwise it
delegates one `wait_if_equal` attempt on the internal token to the selected strategy, then tries
once more to consume the token. A strategy that can finish its attempt while the token is still
empty lets `park` return `Unclassified`; `BusySpin` instead keeps polling until it can observe the
token change.

`park_until_notified` is filtered. It absorbs unclassified wakes until it consumes a token.
Timed variants preserve the same raw/filtered distinction and report public timeout separately
from notification.

The raw results are `ParkResult::{Notified, Unclassified}` and
`ParkTimeoutResult::{Notified, Unclassified, TimedOut}`. The filtered timed result is
`NotificationTimeoutResult::{Notified, TimedOut}`.

A notification token is not a queue item and is not evidence that application work still exists.
Even after a filtered park, use the application's condition loop. For example, with the consumer
returned by `single_pair`:

```rust
loop {
    while let Some(job) = queue.try_pop() {
        process(job);
    }

    parker.park_until_notified();
}
```

This loop also handles another application worker taking the work after the producer issued the
notification. It does not permit concurrent use of the single-consumer Parker handle.

## Strategies

The API uses statically dispatched strategies so the selected path can be inlined without a trait
object in the hot loop:

- busy spin for the shortest expected gaps;
- spin then yield when scheduler cooperation is acceptable;
- `HardwareWait` for the preflighted native AMD `MONITORX/MWAITX` or Intel
  `UMONITOR/UMWAIT` path;
- `SpinThenHardwareWait` for short gaps followed by that same native hardware wait.

Call `HardwareWait::preflight()` during process startup before constructing either hardware
strategy. It runs a bounded baseline and monitored-store probe once for the process and returns a
`PreflightReport` containing the selected `HardwareBackend`, attempt count, observed store-wake
trial count, and baseline duration. `HardwareWait::new` fails with `PreflightRequired` until that gate has run;
`SpinThenHardwareWait::new` observes the same cached result. A cached unsupported, panicked, or
failed result is returned as `HardwareWaitError`, not retried and not replaced by another
mechanism.

Run preflight after the process has established its final allowed CPU domain and power policy, but
while at least two logical CPUs remain available to run the probe concurrently. Worker threads may
later pin themselves within that domain. Do not widen or replace the domain, and keep TSC access
stable for the lifetime of the cached result. A `fork` child must `exec`
before it constructs or uses a hardware strategy; it must not rely on the parent's inherited
cache. CPU hotplug, microcode changes, and virtual-machine migration invalidate the report's
operational and latency meaning. Bounded deadlines and Acquire rechecks preserve functional state
checks only while the selected ISA and user-space TSC access remain available.

On AMD the selected backend is `HardwareBackend::AmdMwaitx`. On Intel it is
`HardwareBackend::IntelUmwait`, and both production and benchmark paths request UMWAIT C0.1. Intel
C0.1 and C0.2 are instruction hints, not Linux CPU idle states C1, C2, or C3. `BusySpin` remains a
unit strategy, and `SpinThenYield::new` owns its explicit spin count. There is intentionally no
automatic or silent fallback: changing the wait mechanism would invalidate performance
expectations.

Use `capabilities()` to report CPUID-only platform facts without calibration, probe threads, or a
hardware wait. Static support does not replace the runtime preflight report. `HardwareWaitError`
distinguishes a missing gate, caught initializer panic, `UnsupportedStrategy`, and a
backend-specific `PreflightFailure`; do not branch on display text. Public capability and strategy
enums are non-exhaustive, so match them with a wildcard arm and avoid exhaustive snapshot
destructuring.

## Choosing quickly

- Choose raw direct waiting when an application atomic already represents readiness and an extra
  event-loop iteration is cheap.
- Choose filtered direct waiting when the call should not return while the value remains equal.
- Choose `single_pair` when one producer owns the notification handle; it is the primary
  low-overhead Parker API.
- Choose `multi_pair` only when producers must clone or concurrently share the notification
  handle; its Release RMW buys the stronger multi-producer publication guarantee.
- On either pair, choose raw `park` when the caller always checks its own readiness state.
- On either pair, choose filtered `park_until_notified` when the caller wants to absorb hardware
  wakes that did not deliver a token.
- Choose a counting primitive rather than either Parker operation when every notification must be
  preserved.

Measure the candidate strategies on the deployment CPU. See [Benchmarking](benchmarking.md) for
the comparison contract.
