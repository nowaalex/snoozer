# snoozer

## Change Contract

- **Responsibility:** provide explicit, low-latency userspace waiting primitives and
  park/unpark-style wrappers. The first production hardware backend is AMD
  `MONITORX/MWAITX` on Linux x86-64.
- **Prohibitions and boundaries:** the library does not change CPU affinity, scheduler policy,
  power policy, or CPU idle settings; it does not silently replace an unsupported hardware
  strategy with a scheduler-based wait. Intel and Arm backends are planned, not implemented.
- **Critical invariants:** arm-then-recheck closes the lost-wake window; the watched atomic
  remains alive and suitably aligned throughout a wait; filtered operations return only after an
  Acquire load observes their stated condition; raw operations may return after an unclassified
  wake; `Single*` notification handles have one producer, while `Multi*` handles preserve every
  coalesced producer publication.
- **Configuration owners:** crate metadata and build profiles live in
  [`Cargo.toml`](Cargo.toml), the compiler version in
  [`rust-toolchain.toml`](rust-toolchain.toml), test policy in
  [`.config/nextest.toml`](.config/nextest.toml), mutation policy in
  [`.cargo/mutants.toml`](.cargo/mutants.toml), and measurement parameters in the
  [benchmark source](benches/wake_latency.rs).
- **Targeted check:** run `just ci`. Hardware-specific and official benchmark checks are separate
  because ordinary CI must not depend on privileged CPU configuration or a particular processor.

Snoozer explores the shortest practical path from a published state change to a running
consumer thread. It exposes both the raw hardware-wake boundary and stronger filtered operations,
so callers can choose whether an extra wake matters.

The project is AMD/Linux-first. Version 1 targets AMD `MONITORX/MWAITX` on Linux x86-64 and
also includes busy-spin and spin-then-yield comparison strategies. The internal boundary is
designed for later Intel `UMONITOR/UMWAIT` and Arm event-based backends, but those backends
must earn support through their own correctness and interference measurements.

> [!WARNING]
> Official benchmarks enable only POLL and exact C1 on the assigned CPUs. C1E and every other CPU
> idle state, including C2, C3, and deeper states, are disabled because their exit latency conflicts
> with the minimum-wake-latency objective. Results therefore do not represent the machine's default
> power-saving configuration.

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

## Documentation

- [Waiting API](docs/waiting-api.md) — choose raw or filtered direct waits and Parker wrappers.
- [Architecture](docs/architecture.md) — understand the token protocol and backend boundaries.
- [Safety](docs/safety.md) — review atomic, lifetime, assembly, and platform invariants.
- [Benchmarking](docs/benchmarking.md) — reproduce latency and SMT-neighbor measurements safely.
- [Decisions](docs/decisions/README.md) — see why the API and benchmark harness have this shape.
- [Contributing](CONTRIBUTING.md) — run repository checks and prepare changes.

## Status

This repository is experimental systems software. Benchmark results are specific to the processor,
firmware, kernel, topology, and power configuration recorded with each run. No strategy is claimed
to be universally fastest.

Licensed under either [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
