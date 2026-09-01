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

## AMD Linux x86-64 backend

The first hardware backend targets AMD `MONITORX/MWAITX` on Linux x86-64.

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
backend, not by this document. The diagnostic `EAX = 0` C1 variant exists only in the benchmark
boundary and is not a production strategy.

## Planned Intel boundary

A later Intel backend may implement the same arm/recheck/wait contract with
`UMONITOR/UMWAIT`. It must:

- verify `WAITPKG` support before executing either instruction;
- account for operating-system controls on user waits and their timeout;
- detect or report a configuration in which address monitoring is advertised but ineffective;
- measure both wake latency and sibling interference before becoming supported.

No current Intel code path is implied by this boundary, and unsupported construction must remain
an explicit typed error.

## Planned Arm boundary

A later Arm backend will evaluate event-based waiting such as an exclusive load followed by
`WFE`, and `WFET` where available. It must define:

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
