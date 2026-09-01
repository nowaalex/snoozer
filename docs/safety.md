# Safety

This page is for reviewers and backend implementers. It records the assumptions that make the
safe public API sound and the rules that constrain architecture-specific assembly.

Snoozer uses `unsafe` because Rust has no safe intrinsic for the targeted user-mode wait
instructions. The public API remains safe: unsupported hardware and failed runtime preflight must
remain typed errors rather than reach an illegal instruction or select a substitute strategy.

## Watched-address lifetime

The address passed to an architecture monitor must remain valid for the entire monitor/wait
sequence. A public wait borrows the atomic for the call, which prevents safe Rust from destroying
or moving it during that sequence. A backend must not retain the pointer after returning.

Each Parker token is reference-counted with its Unparker handle or handles. Its allocation must
remain alive until no endpoint can access it.

Only naturally aligned `AtomicU32` and `AtomicU64` are supported in the first version. The
sealed atomic trait prevents downstream implementations from supplying a type with incompatible
layout, load semantics, or pointer provenance.

## Memory ordering

Address monitoring is a sleep mechanism, not a Rust memory-ordering primitive.

- A producer publishes application data before a Release update to the watched atomic or before
  calling an `unpark` method.
- The consumer must observe the relevant atomic state through an Acquire operation before reading
  the published data.
- `wait_if_equal` may return after an unclassified wake. That return alone establishes no
  synchronization with a producer.
- `wait_until_different` returns only after an Acquire load observes a different value.
- `park_until_notified` returns only after consuming the token with Acquire ordering.

The Parker variants use different publication proofs:

- `SingleUnparker` is `Send` but not `Sync` or `Clone`, and `unpark` requires exclusive access.
  Its Release store is therefore ordered after every earlier publication by that sole producer.
  The consumer's Acquire token consumption synchronizes with the latest represented store.
- `MultiUnparker` is clonable and shareable. Every `unpark` performs a Release
  read-modify-write, even when the token is already notified. Each notification heads a release
  sequence containing later notification RMWs. Those sequences overlap, so one Acquire token
  consumption synchronizes with every producer publication represented by the coalesced token.

Replacing the multi-producer RMW with a store would weaken that guarantee. Allowing concurrent
use of `SingleUnparker` would invalidate its store-based proof, so safe Rust prevents it.

A backend must not weaken these orderings based on the ordering properties of a particular
instruction. Rust's atomic contract remains the portable source of truth.

## Lost-wake invariant

The monitor must be armed before the second load, and the hardware wait must occur only if that
load still equals the expectation. Reordering or deleting the second load can create a lost wake.

Tests use a gated fake strategy to pause after the simulated arm and before recheck, after recheck
and before the simulated wait, and during that wait. A separate pre-notification test covers the
token-before-park case. Hardware tests complement those deterministic tests but do not replace
them.

## Monitor granularity and false sharing

Hardware may monitor more bytes than the atomic itself. A store to unrelated data in the same
monitoring granule can end the wait. That is an unclassified wake, not memory unsafety.

The internal Parker token is placed on an isolated cache line to reduce false sharing. Direct
atomic callers control their own layout and should isolate a hot watched word from unrelated
writes when unnecessary wakes matter. Cache-line isolation reduces likely interference; it does
not claim a universal hardware monitoring granule.

## Capability and operating-system checks

Architecture-specific instructions are unreachable until static detection confirms their
advertised capability. AMD checks its extended CPUID feature; Intel checks vendor identity and
`WAITPKG`. Both require the timing capabilities used by their bounded wait. Forced-unsupported
test seams verify that detection errors return typed failures instead of executing assembly.

Capability bits are necessary but insufficient because user waits can be constrained by the
operating system, hypervisor, firmware, or microcode. `HardwareWait::preflight()` therefore runs a
bounded native baseline and monitored-store operational probe before `HardwareWait::new()` can
succeed. Its result is cached process-wide, including failure, so concurrent callers cannot race
different hardware verdicts and later callers cannot silently retry a failed gate. A passed
`PreflightReport` establishes only that the controlled trials completed in that process. Because
hardware waits have multiple wake causes, the report does not prove the exact cause of a return and
is not performance evidence.

`HardwareWaitError` keeps startup states distinct: preflight not yet run, initializer panic,
statically unsupported, or a runtime preflight failure naming its backend and failure class. Every
terminal result, including a caught panic, is cached. Callers must branch on those types, never
display text. No error path may construct busy-spin, yield, park, or the other vendor's backend as
a fallback.

`capabilities()` is side-effect-free CPUID discovery. It does not calibrate a timer, create probe
threads, execute a hardware wait, or satisfy the preflight gate.

Preflight belongs after the final allowed CPU domain and power policy are established, while at
least two logical CPUs can run its waiter and producer concurrently. Workers may later narrow
their own affinity within that domain; the process must not widen or replace the domain or lose the
TSC access on which the selected backend was checked. A child
created with `fork` inherits cached memory without reproducing the probe; it must `exec` before
constructing or using `HardwareWait`. CPU hotplug, microcode update, or virtual-machine migration
after preflight invalidates the report as operational and latency evidence. If the change only
degrades monitored-store wake effectiveness, the bounded safety deadline and final Acquire
rechecks still protect functional progress and observation. Revoking the selected instruction set
or user-space TSC access is outside the execution contract and can fault the process; a cheap
constructor cannot make that mutable environment immutable. Callers must not present measurements
from a changed environment as preflighted evidence.

## Progress and timeouts

Every production hardware wait uses a bounded TSC deadline. Its expiry is an internal,
unclassified wake and is distinct from a caller-requested timeout:

- a raw timed operation may report changed, unclassified, or timed out;
- a filtered timed operation reports only its condition or timed out;
- repeated hardware timer expirations cause another condition check and do not masquerade as a
  public timeout.

AMD calibrates the MWAITX timer outside the hot loop. Intel UMWAIT uses the bounded deadline and
requests C0.1 only; C0.1 is an instruction hint and must not be described as Linux CPU C1, C2, or
C3. If a usable timer or native wait cannot be established, preflight fails rather than selecting
an unbounded wait or a silent fallback.

## Inline assembly rules

Architecture-specific assembly is confined to the architecture module. Every `unsafe` block
must:

- be as small as the instruction boundary permits;
- have an adjacent `// SAFETY:` comment naming the established invariants;
- declare all inputs, outputs, clobbers, and memory effects accurately;
- avoid assuming register preservation not guaranteed by the instruction or compiler interface;
- remain unreachable on an unsupported target or processor.

Instruction encodings are verified separately from mutable Rust logic. Mutation testing is useful
for the surrounding checks but is not evidence that machine-code operands are correct.

## What the library does not make safe

Snoozer does not make a logically racy application protocol correct. In particular:

- raw returns must be followed by an application-state recheck;
- a Parker token cannot count events;
- direct equality waiting cannot detect an ABA transition;
- a timeout does not cancel a producer or roll back published work;
- changing affinity, scheduler policy, governor, or idle states remains the caller's operational
  responsibility.

Benchctl has additional privileged-state recovery requirements documented in
[Benchmarking](benchmarking.md), including the trust boundary for custom sysfs roots and write
helpers. Those operations are not performed by the library.
