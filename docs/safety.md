# Safety

This page is for reviewers and backend implementers. It records the assumptions that make the
safe public API sound and the rules that constrain architecture-specific assembly.

Snoozer uses `unsafe` because Rust has no safe intrinsic for the targeted user-mode wait
instructions. The public API remains safe: unsupported hardware must fail during strategy
construction rather than reach an illegal instruction.

## Watched-address lifetime

The address passed to an architecture monitor must remain valid for the entire monitor/wait
sequence. A public wait borrows the atomic for the call, which prevents safe Rust from destroying
or moving it during that sequence. A backend must not retain the pointer after returning.

The Parker token is reference-counted with its Unparker handles. Its allocation must remain alive
until no Parker or Unparker can access it.

Only naturally aligned `AtomicU32` and `AtomicU64` are supported in the first version. The
sealed atomic trait prevents downstream implementations from supplying a type with incompatible
layout, load semantics, or pointer provenance.

## Memory ordering

Address monitoring is a sleep mechanism, not a Rust memory-ordering primitive.

- A producer publishes application data before a Release update to the watched atomic or before
  `Unparker::unpark`.
- The consumer must observe the relevant atomic state through an Acquire operation before reading
  the published data.
- `wait_if_equal` may return after an unclassified wake. That return alone establishes no
  synchronization with a producer.
- `wait_until_different` returns only after an Acquire load observes a different value.
- `park_until_notified` returns only after consuming the token with Acquire ordering.

A backend must not weaken these orderings based on the ordering properties of a particular
instruction. Rust's atomic contract remains the portable source of truth.

## Lost-wake invariant

The monitor must be armed before the second load, and the hardware wait must occur only if that
load still equals the expectation. Reordering or deleting the second load can create a lost wake.

Tests use a fake backend to pause at each boundary: before arming, after arming, immediately before
sleep, and during sleep. A hardware smoke test complements those deterministic tests but does not
replace them.

## Monitor granularity and false sharing

Hardware may monitor more bytes than the atomic itself. A store to unrelated data in the same
monitoring granule can end the wait. That is an unclassified wake, not memory unsafety.

The internal Parker token is placed on an isolated cache line to reduce false sharing. Direct
atomic callers control their own layout and should isolate a hot watched word from unrelated
writes when unnecessary wakes matter. Cache-line isolation reduces likely interference; it does
not claim a universal hardware monitoring granule.

## Capability and operating-system checks

Architecture-specific instructions are unreachable until construction confirms their advertised
capability. The AMD backend checks the relevant extended CPUID feature. A forced-unsupported test
seam verifies that detection errors return a typed `UnsupportedStrategy` instead of executing
assembly.

Capability bits are necessary, but a future backend may need stronger checks. Intel user waits can
be constrained by the operating system or affected by microcode behavior. Arm event and
reservation behavior varies by architecture level. Each backend owns those checks and must fail
visibly when its requirements cannot be established.

## Progress and timeouts

The AMD hardware wait always uses its bounded safety timer. Its expiry is an internal,
unclassified wake. It is distinct from a caller-requested timeout:

- a raw timed operation may report changed, unclassified, or timed out;
- a filtered timed operation reports only its condition or timed out;
- repeated hardware timer expirations cause another condition check and do not masquerade as a
  public timeout.

Timer calibration is performed outside the hot loop. If a usable timer cannot be established,
hardware-strategy construction fails rather than selecting an unbounded wait or a silent fallback.

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

The benchmark runner has additional privileged-state recovery requirements documented in
[Benchmarking](benchmarking.md). Those operations are not performed by the library.
