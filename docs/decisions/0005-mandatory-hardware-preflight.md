# ADR 0005: Require process-wide hardware-wait preflight

- **Status:** Accepted
- **Date:** 2026-09-02

## Context

CPUID can advertise AMD `MONITORX/MWAITX` or Intel `UMONITOR/UMWAIT` while the effective userspace
behavior is still constrained by firmware, the kernel, a hypervisor, or microcode. Static feature
detection prevents an illegal instruction, but it cannot establish that a bounded wait behaves
operationally on the running system.

Silently selecting busy spin, yielding, parking, or the other vendor's backend would preserve
functional progress while invalidating the latency and interference contract that caused the
caller to select hardware waiting. Retrying an inconclusive or failed probe at arbitrary
construction sites would also make process behavior depend on timing and call order.

The probe creates threads and performs timed hardware waits, so making it a mandatory startup gate
is an expensive public lifecycle choice. Reversing it later would change constructor failure
semantics, test evidence, and benchmark provenance.

## Considered alternatives

### Trust static capability detection

This is the cheapest startup path. It cannot detect a userspace wait that repeatedly returns too
quickly or a probe in which a monitored publication is not observed within its bound.

### Probe in every constructor

This keeps one public constructor but repeats an expensive operation and permits concurrent
callers to observe different results. Constructor latency would be surprising and difficult to
bound operationally.

### Probe lazily on the first wait

This hides startup work in the latency-sensitive path and makes the first wait semantically
different. A failure would arrive after the caller had already accepted construction.

### Require one cached process-wide preflight

This makes the cost and failure visible at startup. Every later constructor observes one stable
backend verdict, and applications can record the report before starting latency-sensitive work.

## Decision

`HardwareWait::preflight()` is mandatory before `HardwareWait::new()` or
`SpinThenHardwareWait::new()` can succeed. It:

- selects only the native AMD or Intel backend;
- performs a bounded baseline and monitored-store operational probe;
- returns a report containing the backend, attempts, observed store-wake trials, and baseline
  duration;
- caches success or failure process-wide and is idempotent for all later callers;
- never substitutes another strategy or retries a cached failure.

Construction before the gate returns `PreflightRequired`. A caught initializer panic is cached as
`PreflightPanicked`. Static rejection remains an `UnsupportedStrategy`; runtime failure names its
backend and typed `PreflightFailure`. Benchmark smoke mode may record failure and run only its
explicitly portable comparison cases. Official mode must reject the run unless the native backend
passed preflight.

The probe is bounded operational evidence: it shows that the expected publication/wait experiment
completed under the probe's controls. It does not prove the exact microarchitectural reason that a
particular wait returned, and it is not latency, power, or interference evidence.

## Consequences

- Applications must place hardware preflight in a controlled startup phase.
- The allowed CPU domain, power policy, and TSC access must be final before the cached gate runs;
  at least two logical CPUs must remain available for its concurrent waiter/producer trial.
- A `fork` child must `exec` before hardware waiting; inherited cached evidence is not valid there.
- CPU hotplug, microcode change, or VM migration invalidates operational and latency evidence after
  preflight. If only store-wake effectiveness degrades, bounded deadlines and Acquire rechecks
  retain progress; revoking the selected ISA or user-space TSC access is outside the execution
  contract and can fault the process.
- Benchmarks must record the complete report and select a vendor-specific matrix.
- Unit and hardware tests must cover concurrent idempotence and cached failure as well as success.
- Intel production and benchmark waits request C0.1 only; C0.2 is not a production path.
- Adding another architecture requires a new backend-specific decision; Arm is not included here.

## Related sources

- [Waiting API](../waiting-api.md)
- [Architecture](../architecture.md)
- [Safety](../safety.md)
- [Benchmarking](../benchmarking.md)
