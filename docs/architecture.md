# Architecture

This page is for maintainers and backend authors. It describes which layer owns each contract and
how a new architecture can be added without changing the public waiting model.

## Layers and ownership

Snoozer separates four concerns:

1. Caller-owned state is represented by `AtomicU32` or `AtomicU64` through the sealed
   `WaitableAtomic` contract.
2. `WaitStrategy` owns the raw and filtered state-wait loops.
3. `SingleParker`/`SingleUnparker` and `MultiParker`/`MultiUnparker` adapt those loops to a
   private, coalescing token with an explicit producer-ownership contract.
4. An architecture backend owns capability detection and the smallest possible
   arm/recheck/hardware-wait boundary.

The token protocol must not contain AMD-, Intel-, or Arm-specific branches. Likewise, an
architecture backend does not interpret queue readiness or notification counts.

Static dispatch is intentional. A concrete strategy can be inlined through the public operation,
and no trait object, allocation, system call, CPUID query, or wall-clock read is required in the
notification hot path.

## Lost-wake protocol

An address-monitoring backend must follow this order:

1. Acquire-load the watched atomic and return if it no longer equals the expected value.
2. Arm monitoring for the atomic's address.
3. Acquire-load the same atomic again.
4. Skip sleeping if the second load differs.
5. Otherwise execute one bounded hardware wait.

The second load is essential. A producer may store after the first load but before monitoring is
armed. Without the second load, that store could be missed and the consumer could sleep until an
unrelated interrupt or safety timeout.

The raw operation ends after step 1, step 4, or one execution of step 5. The filtered operation
calls the raw operation repeatedly until its Acquire load sees a different value. See
[Waiting API](waiting-api.md) for the public meaning of those returns.

## Parker adaptation

Both public Parker pairs use one private consumer core and an isolated atomic token with two
logical states: empty and notified. A Relaxed load avoids an unnecessary locked operation while
the token is empty; an Acquire compare-exchange consumes a token that is present.

The producer operation is the deliberate difference:

- `SingleUnparker` has exclusive ownership and uses a Release store;
- every `MultiUnparker` clone uses a Release read-modify-write, preserving overlapping release
  sequences across concurrent producers.

This keeps the one-producer path free of a producer-side locked RMW without weakening the
multi-producer publication contract.

This gives a one-token mailbox:

- notification before parking is retained;
- notification during arming is covered by the recheck;
- notifications during hardware waiting write the monitored line;
- multiple notifications before consumption coalesce.

The internal token and direct waits deliberately share the same strategy implementation. A
Parker backend that duplicated the hardware loop would create two places for lost-wake and safety
bugs.

## Hardware backend selection and preflight

`HardwareWait` is the single production hardware strategy. On Linux x86-64 it selects the native
backend from `HardwareBackend::{AmdMwaitx, IntelUmwait}`. Selection is explicit in diagnostics and
never substitutes busy spinning, yielding, parking, or another vendor's instructions.

`HardwareWait::preflight()` is a mandatory process-wide startup gate. It checks static capability,
runs a bounded baseline and monitored-store operational probe through the selected backend, and
caches the complete result. Concurrent and later callers observe that same pass or failure. A
successful report records the selected backend, attempt count, observed store-wake trials, and
baseline duration.
Construction through `HardwareWait::new()` fails with `PreflightRequired` until that gate has run,
and returns the cached failure rather than retrying or falling back. This contract is recorded in
[ADR 0005](decisions/0005-mandatory-hardware-preflight.md).

Preflight is bounded operational evidence, not a reason oracle or benchmark. Completion of a
store-wake trial does not prove which microarchitectural event ended the wait. Preflight also does
not prove a latency distribution, an idle-state transition, or acceptable SMT-neighbor
interference.

The caller must establish its final allowed CPU domain and power policy before preflight, while
leaving at least two logical CPUs available so the probe's waiter and producer can run
concurrently. Workers may later pin themselves inside that same domain, but the process must not
widen or replace it and must keep TSC access stable. A `fork` child inherits the cache without rerunning
the threads that produced it, so the child must `exec` before using `HardwareWait`. CPU hotplug,
microcode changes, and virtual-machine migration after preflight invalidate its operational and
latency evidence. If only store-wake effectiveness changes, bounded hardware deadlines and Acquire
rechecks continue to own functional progress and state observation. Revoking the selected ISA or
user-space TSC access is outside the execution contract and can fault the process.

## AMD Linux x86-64 backend

The AMD backend uses `MONITORX/MWAITX`.

- Construction checks the architectural capability before any instruction is reachable.
- `MONITORX` arms address monitoring.
- The common protocol performs the required recheck.
- `MWAITX` uses `EAX = 0xF0`, placing `0xF` in `EAX[7:4]` to request the optimized C0
  no-C-state path. Linux names the same encoding
  [`MWAITX_DISABLE_CSTATES`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/arch/x86/include/asm/mwait.h).
- The hardware timer is enabled with a bounded, nonzero interval so unrelated missed progress
  cannot leave the thread asleep forever.
- The fixed safety-timer cycle budget is computed during construction. An untimed public wait
  reuses it without wall-clock reads or duration division in the hot path; a timed public wait
  still computes a shorter remaining budget when required.
- Timer expiry and any other return from `MWAITX` are unclassified until Rust reloads the
  atomic.

The remaining instruction operands and timer calibration are implementation-owned by the AMD
backend, not by this document. The diagnostic `EAX = 0` CPU C1 variant exists only in the
benchmark boundary and is not a production strategy.

## Intel Linux x86-64 backend

The Intel backend uses `UMONITOR/UMWAIT` and preserves the same arm/recheck/wait contract.

- Static detection verifies Intel vendor identity, `WAITPKG`, invariant TSC, and `RDTSCP` before
  either instruction is reachable.
- Preflight rejects a backend whose wait does not block sufficiently to classify the probe or
  whose monitored store wake is not observed.
- Production and benchmark paths request Intel C0.1 only. C0.1 and C0.2 are `UMWAIT` instruction
  hints; neither name means Linux CPU idle state C1, C2, or C3.
- The bounded TSC deadline remains a safety wake. A return is unclassified until Rust reloads the
  atomic.

Support means that the selected instruction path passed its startup contract. It does not claim
hardware-verified latency or interference results on processors that have not been measured.

## Planned Arm boundary

Arm is outside the current implementation. Any later backend would need to define:

- how the exclusive reservation is established and rechecked;
- the monitored reservation granule and resulting false-wake behavior;
- ordering around producer stores and consumer loads;
- progress under interrupts, migration, virtualization, and timeout;
- whether an explicit event instruction is required by its selected protocol.

Arm support is not a textual substitution for the x86 instructions. It needs architecture-specific
proofs and measurements while preserving the same public raw and filtered contracts.

## Adding a backend

A backend proposal is complete only when it provides:

- runtime capability detection and a typed unsupported reason;
- an arm/recheck/wait implementation with minimal documented `unsafe` blocks;
- deterministic protocol tests through the fake backend;
- bounded smoke tests on the intended hardware;
- latency and neighbor-interference measurements;
- updates to [Safety](safety.md) for new invariants and to
  [Benchmarking](benchmarking.md) for platform-specific preflight.

It must not alter the token protocol or silently route unsupported callers to another strategy.
