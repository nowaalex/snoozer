# ADR 0003: Expose producer cardinality in Parker types

- **Status:** Accepted
- **Date:** 2026-09-01

## Context

A coalescing park token can serve one producer with an ordinary Release store. A clonable handle
used by concurrent producers needs a Release read-modify-write so one Acquire consumption can
synchronize with every producer publication represented by the token. The RMW is materially more
expensive on the notification hot path because it obtains exclusive cache-line ownership even
when the token is already set.

One `Parker`/`Unparker` pair cannot communicate that cost and ownership difference in its types.
Keeping only the multi-producer implementation would make the simplest supported topology pay for
a guarantee it does not need. Silently selecting an implementation at runtime would make the hot
path and memory-ordering contract depend on hidden state.

## Considered alternatives

### Keep one clonable multi-producer pair

This is the smallest public API and supports every producer topology. It permanently requires a
Release RMW for notifications, including applications with exactly one producer.

### Configure producer cardinality at runtime

A mode flag could select store or RMW publication. The handle type would not enforce the selected
ownership rule, so accidental cloning or sharing could invalidate the store-based proof.

### Use a generic mode parameter

A sealed generic mode could preserve static dispatch. It makes constructor signatures and type
errors harder to read while still requiring users to learn two materially different contracts.

### Expose explicit single- and multi-producer pairs

Separate constructors and endpoint names make the ownership and performance choice visible.
Safe-Rust traits and method receivers enforce the single-producer boundary, while the
multi-producer handle retains clone and shared-reference ergonomics.

## Decision

Expose two pairs:

- `single_pair` returns `SingleParker` and `SingleUnparker`. The producer is `Send`, but not
  `Sync` or `Clone`; `unpark` requires exclusive access and performs a Release store.
- `multi_pair` returns `MultiParker` and `MultiUnparker`. The producer is `Send`, `Sync`, and
  `Clone`; `unpark` takes shared access and performs a Release read-modify-write.

Both Parker types remain single-consumer, use the same private consumer core, and expose the same
raw and filtered park operations. The `multi` prefix describes producer cardinality, not consumer
cardinality. The earlier unprefixed `pair`, `Parker`, and `Unparker` API is removed before the
initial stable release rather than retained as an ambiguous compatibility alias.

## Consequences

- The primary one-producer path avoids a producer-side locked RMW.
- Multi-producer publication remains explicit and retains its stronger coalesced-publication
  guarantee.
- Endpoint names and Rust ownership rules communicate the cost model without runtime branches.
- Examples and benchmarks must state which producer topology they exercise.
- A caller changing producer cardinality must deliberately migrate to the other pair.
- Both variants still coalesce notifications and are not counting primitives.

## Related sources

- [Waiting API](../waiting-api.md)
- [Architecture](../architecture.md)
- [Safety](../safety.md)
- [ADR 0001: raw and filtered waits](0001-two-level-waiting-api.md)
