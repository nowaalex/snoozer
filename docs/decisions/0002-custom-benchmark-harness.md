# ADR 0002: Use a custom coordinated wake-latency harness

- **Status:** Accepted
- **Date:** 2026-09-01

## Context

The project must compare end-to-end wake latency under saturated and bursty publication while
measuring interference with a compute-bound SMT sibling. A valid sample depends on a strict
producer/consumer acknowledgement, fixed topology, ordered cycle reads, migration detection, and
a paired victim-only baseline.

General microbenchmark frameworks are optimized for a different unit of work. Criterion measures
iterated routines and can provide wall-clock statistics, but it does not define this coordinated
four-thread protocol or privileged CPU-state lifecycle. Gungraun uses Callgrind event counts; its
own documentation distinguishes that from real wall-clock behavior. Adapting either framework
would still leave the experiment, sample validity, and result schema in project-owned code.

## Considered alternatives

### Criterion

Criterion is mature and convenient for ordinary Rust microbenchmarks. Its outer measurement model
does not eliminate the need for a custom synchronized sampler, per-sample CPU checks, paired
neighbor runs, or machine-readable environment metadata. Layering it around that sampler would add
another statistical owner without simplifying the experiment.

### Gungraun

Gungraun offers deterministic Callgrind measurements that are valuable for instruction-level
regression analysis. Callgrind does not measure the wake latency, scheduler effects, interrupts,
or sibling interference that decide this comparison.

### A custom `harness = false` benchmark

The project owns the complete sampling state machine and output schema. This increases maintenance
responsibility, but gives one authority for acknowledgements, topology, validity checks,
treatment/control pairing, percentiles, and recovery metadata.

## Decision

Use a custom Cargo benchmark binary with `harness = false`. It must:

- run the versioned saturated, bursty, and SMT-neighbor protocols;
- retain raw cycle data or a losslessly equivalent representation;
- reject migrated or otherwise invalid timing samples;
- alternate treatment and victim-only control order across repetitions;
- emit a machine-readable record containing the measurement configuration and environment;
- distinguish non-official smoke output from official controlled output;
- fail closed when official CPU topology or idle-state preflight cannot be proven.

Numeric schedules, thresholds, repetitions, and strategy sweeps are owned by benchmark code or
configuration and emitted at runtime. This record intentionally does not copy them.

## Consequences

- The benchmark can measure the actual cross-thread phenomenon and account for neighbor cost.
- Results remain auditable because environment and validity metadata travel with samples.
- The repository owns statistical correctness, schema evolution, and backwards-compatible result
  parsing.
- Ordinary CI can exercise smoke and protocol tests without pretending to provide controlled
  hardware evidence.
- Changes to the experiment require focused review and a version change when result meaning
  changes.

## Related sources

- [Benchmarking](../benchmarking.md)
- [Criterion documentation](https://bheisler.github.io/criterion.rs/book/)
- [Gungraun repository](https://github.com/gungraun/gungraun)
