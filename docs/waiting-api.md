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
- AMD and hybrid AMD strategies use the address-monitoring protocol below after any spin prefix.

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

## Parker and Unparker

`Parker`/`Unparker` wrap the same direct-wait protocol around an internal notification
token:

- `unpark` sets one token with a Release read-modify-write;
- an available token makes a later park return without sleeping;
- at most one token is stored, so repeated notifications coalesce;
- consuming a token uses Acquire ordering;
- `Unparker` is cloneable and can be shared by producers;
- a `Parker` can move between threads but cannot be waited on concurrently.

Each Release notification heads a release sequence that includes later notification
read-modify-writes. Those sequences overlap, so the consumer's Acquire token consumption acquires
the publications of every producer represented by that token, even though the notifications
themselves collapse to one.

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
Even after a filtered park, use the application's condition loop:

```rust
loop {
    while let Some(job) = queue.try_pop() {
        process(job);
    }

    parker.park_until_notified();
}
```

This loop also handles another consumer taking the work after the producer issued the
notification.

## Strategies

The API uses statically dispatched strategies so the selected path can be inlined without a trait
object in the hot loop:

- busy spin for the shortest expected gaps;
- spin then yield when scheduler cooperation is acceptable;
- AMD `MONITORX/MWAITX` for a hardware-assisted wait;
- spin then AMD `MONITORX/MWAITX` for short gaps followed by hardware waiting.

Construction performs capability checks before an architecture-specific instruction can execute.
`AmdMwaitx::new` and `SpinThenAmdMwaitx::new` return a typed
`UnsupportedStrategy` when their requirements are absent. `BusySpin` is a unit strategy,
and `SpinThenYield::new` owns its explicit spin count. There is intentionally no automatic or
silent fallback: changing the wait mechanism would invalidate performance expectations.

Use `capabilities()` to report detected platform support without constructing a hardware
strategy. `UnsupportedStrategy::strategy` identifies the requested strategy and
`UnsupportedStrategy::reason` identifies the failed requirement; do not branch on display
text. `Capabilities`, `Strategy`, and `UnsupportedReason` are non-exhaustive because planned
architecture backends may add fields, strategies, and failure reasons. Match the enums with a
wildcard arm and inspect capability fields without constructing or exhaustively destructuring the
snapshot.

## Choosing quickly

- Choose raw direct waiting when an application atomic already represents readiness and an extra
  event-loop iteration is cheap.
- Choose filtered direct waiting when the call should not return while the value remains equal.
- Choose raw Parker when a coalescing token is convenient and the caller always checks its own
  readiness state.
- Choose filtered Parker when the caller wants to absorb hardware wakes that did not deliver a
  token.
- Choose a counting primitive rather than either Parker operation when every notification must be
  preserved.

Measure the candidate strategies on the deployment CPU. See [Benchmarking](benchmarking.md) for
the comparison contract.
