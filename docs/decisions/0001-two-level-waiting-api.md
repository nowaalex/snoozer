# ADR 0001: Expose raw and filtered waiting contracts

- **Status:** Accepted
- **Date:** 2026-09-01

## Context

The processor can stop a hardware wait for several reasons. A monitored store is the desired
reason, but a safety timer, interrupt, scheduling event, hypervisor, neighboring store, or
platform event can also end it. The instruction does not provide a portable reason that can be
treated as proof of application readiness.

Always hiding these unclassified wakes makes the common API harder to misuse, but it also forces
an internal loop and repeated loads. Some low-latency consumers already have a cheap event-loop
check and do not care about an occasional extra iteration. Exposing only the raw instruction
boundary would minimize mechanism while making every caller rebuild the same filtering loop.

The same choice exists for direct caller-owned atomics and for a park/unpark-style notification
token.

## Considered alternatives

### Expose only filtered waits

This gives the strongest default postcondition. It withholds the shortest hardware boundary from
callers that can safely handle an unclassified wake, which conflicts with the project's purpose.

### Expose only raw waits

This is the smallest mechanism. It makes simple callers repeatedly implement condition loops and
increases the chance that a return is mistaken for synchronization or proof of work.

### Accept a readiness callback in a Waiter/Notifier API

The library could invoke a caller predicate before and after every wait. This is convenient for
complex application conditions, but it moves application work, callback semantics, and potential
clock bookkeeping into the critical path. It is not the minimal primitive and can be built above
the selected API.

### Expose raw and filtered contracts

This preserves the hardware boundary while providing a stronger operation from the same protocol.
The cost is additional public concepts whose names and outcomes must make the distinction
unambiguous.

## Decision

Expose two direct atomic operations:

- `wait_if_equal` performs at most one hardware wait and may return unclassified;
- `wait_until_different` filters unclassified wakes until an Acquire load observes a
  different value.

Expose the same distinction in the Parker wrapper:

- `park` may return without consuming a notification token;
- `park_until_notified` filters until it consumes the token with Acquire ordering.

Outcome enums distinguish changed, notified, unclassified, and timed-out cases where those cases
are part of the selected contract. Documentation must state that a raw return does not prove work
exists and does not itself synchronize with a producer.

Both levels use one shared arm/recheck/wait protocol. Parker is an adapter around a private
coalescing token, not a second backend.

## Consequences

- Expert callers can reach the minimal safe hardware-wait boundary.
- Ordinary callers can select a strong, state-based postcondition without recreating the loop.
- API names communicate condition and postcondition rather than an implementation detail such as
  the number of loop iterations.
- Tests must explicitly demonstrate raw unclassified returns and filtered absorption.
- Every future backend must preserve both contracts even when its wake causes differ.
- A later predicate-based helper remains possible above Parker without changing the primitive.

## Related sources

- [Waiting API](../waiting-api.md)
- [Architecture](../architecture.md)
- [Safety](../safety.md)
