# snoozer

Snoozer explores the shortest practical path from a published state change to a running
consumer thread. It exposes both the raw hardware-wake boundary and stronger filtered operations,
so callers can choose whether an extra wake matters.

The hardware strategy supports AMD `MONITORX/MWAITX` and Intel `UMONITOR/UMWAIT` on Linux
x86-64. Before either instruction path can be constructed, `HardwareWait::preflight()` runs the
native backend's bounded operational probe and caches its result for the process. Passing this
startup gate establishes only that the controlled publication/wait trials completed; it neither
proves the exact reason each wait returned nor makes a latency, power, or universal-performance
claim.

Intel `UMWAIT` uses the architectural C0.1 hint. The name is an instruction hint, not Linux's CPU
idle-state naming: Intel C0.1 is distinct from CPU C1, C2, C3, and deeper package or core states.

> [!WARNING]
> Official benchmarks enable only POLL and exact CPU C1 on the assigned CPUs. C1E and every other
> CPU idle state, including CPU C2, CPU C3, and deeper states, are disabled because their exit
> latency conflicts with the minimum-wake-latency objective. Intel UMWAIT's C0.1 hint is separate
> from that sysfs policy. Results therefore do not represent the machine's default power-saving
> configuration.

## Choose an interface

Use a direct atomic wait when the state change itself can wake the consumer:

```rust
use snoozer::{SpinThenYield, WaitResult, WaitStrategy as _};
use std::sync::atomic::{AtomicU64, Ordering};

let generation = AtomicU64::new(1);
let observed = 0;
let strategy = SpinThenYield::new(0);

// One raw strategy attempt may finish while the value is still equal.
let next = match strategy.wait_if_equal(&generation, observed) {
    WaitResult::Changed(next) => next,
    WaitResult::Unclassified => generation.load(Ordering::Acquire),
};
assert_eq!(next, 1);

// Filters unclassified wakes and returns the newly observed value.
assert_eq!(strategy.wait_until_different(&generation, observed), 1);
```

## Change Contract

- **Responsibility:** provide explicit, low-latency userspace waiting primitives and
  park/unpark-style wrappers. `HardwareWait` selects the native AMD `MONITORX/MWAITX` or Intel
  `UMONITOR/UMWAIT` backend on Linux x86-64 only after a successful process-wide preflight.
- **Prohibitions and boundaries:** the library does not change CPU affinity, scheduler policy,
  power policy, or CPU idle settings; it does not silently replace an unsupported or failed
  hardware strategy with a scheduler-based wait. Arm support is outside the current contract.
- **Critical invariants:** arm-then-recheck closes the lost-wake window; the watched atomic
  remains alive and suitably aligned throughout a wait; filtered operations return only after an
  Acquire load observes their stated condition; raw operations may return after an unclassified
  wake; `Single*` notification handles have one producer, while `Multi*` handles preserve every
  coalesced producer publication; hardware preflight runs after the final allowed CPU domain and
  power policy are established, while its helper can still run concurrently; a `fork` child must
  `exec` before using an inherited hardware-wait cache.
- **Configuration owners:** crate metadata and build profiles live in
  [`Cargo.toml`](Cargo.toml), the compiler version in
  [`rust-toolchain.toml`](rust-toolchain.toml), test policy in
  [`.config/nextest.toml`](.config/nextest.toml), mutation policy in
  [`.cargo/mutants.toml`](.cargo/mutants.toml), and measurement parameters in the
  [benchmark source](benches/wake_latency.rs).
- **Targeted check:** run `just ci`. Hardware-specific and official benchmark checks are separate
  because ordinary CI must not depend on privileged CPU configuration or a particular processor.

Use `single_pair` when one producer owns the wake handle. This is the lowest-overhead Parker path:

```rust
use snoozer::{BusySpin, ParkResult, single_pair};

let (mut parker, mut unparker) = single_pair(BusySpin);

// Another thread publishes work, then calls:
unparker.unpark();

// Raw: returning does not prove that work is available.
let outcome = parker.park();
assert!(matches!(outcome, ParkResult::Notified));

// Filtered: returns only after consuming the notification token.
unparker.unpark();
parker.park_until_notified();
```

Use `multi_pair` when multiple producers must clone or concurrently share the wake handle. It uses
a more expensive atomic read-modify-write so one consumed token acquires every coalesced producer
publication. Both forms have exactly one consumer; `multi` describes producers, not parkers.

The exact exported constructors and strategy types are documented in the crate API. The behavior
and selection rules live in [Waiting API](docs/waiting-api.md).

For the native hardware path, preflight once before constructing the strategy:

```rust
use snoozer::{HardwareWait, WaitStrategy as _};
use std::sync::atomic::AtomicU32;

let report = HardwareWait::preflight()?;
let strategy = HardwareWait::new()?;
let state = AtomicU32::new(0);
let _outcome = strategy.wait_if_equal(&state, 0);

println!("using {:?} after {} preflight attempts", report.backend(), report.attempts());
# Ok::<(), snoozer::HardwareWaitError>(())
```

## Documentation

- [Waiting API](docs/waiting-api.md) — choose raw or filtered direct waits and Parker wrappers.
- [Architecture](docs/architecture.md) — understand the token protocol and backend boundaries.
- [Safety](docs/safety.md) — review atomic, lifetime, assembly, and platform invariants.
- [Benchmarking](docs/benchmarking.md) — reproduce latency and SMT-neighbor measurements safely.
- [Benchctl](docs/benchctl.md) — build receipts, official-run control, status, and recovery.
- [Decisions](docs/decisions/README.md) — see why the API and benchmark harness have this shape.
- [Contributing](CONTRIBUTING.md) — run repository checks and prepare changes.

## Status

This repository is experimental systems software. Benchmark results are specific to the processor,
firmware, kernel, topology, and power configuration recorded with each run. No strategy is claimed
to be universally fastest.

Licensed under either [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
