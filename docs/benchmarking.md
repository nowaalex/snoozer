# Benchmarking

> [!CAUTION]
> **Official benchmarks disable C2, C3, and every deeper CPU idle state on every assigned logical
> CPU. Their exit latency conflicts with the minimum-wake-latency objective. These results do not
> represent the machine's default power-saving configuration.**

This page is for engineers who run or interpret wake-latency measurements. It defines the
experimental controls and recovery obligations. It is not a guide for changing a production
machine's global power policy.

## Why the project owns its harness

Wake latency is a coordinated event between producer and consumer threads, not the execution time
of one function in isolation. The benchmark also needs a simultaneously running SMT-neighbor
victim, strict acknowledgements, CPU-migration rejection, raw cycle samples, and a paired
victim-only baseline.

For those reasons the project uses a `harness = false` benchmark binary:

- Criterion's statistical loop does not own the required cross-thread protocol or paired
  victim-only control.
- Gungraun measures instruction and Callgrind events rather than the wall-clock wake distribution
  that decides this project.

The benchmark still applies statistical discipline: it preserves samples, alternates treatment
and control order, records every input needed for interpretation, and summarizes multiple
repetitions.

## Official and smoke runs

An **official run** uses the privileged runner. The benchmark discovers and validates topology
and pins its participating threads. The runner applies the documented CPU-idle policy, verifies it
by reading sysfs back, starts the benchmark, and restores the exact prior state.

A **smoke run** uses shorter measurement settings and does not satisfy the official evidence
contract. It reads and reports the current CPU-idle configuration, leaves it unchanged, and prints
a distinct `NON-OFFICIAL` warning that deeper states may be enabled. Smoke mode is suitable for
checking output shape and detecting obvious regressions, not for publishing a winner.

Use the commands in [Contributing](../CONTRIBUTING.md). The
[runner](../scripts/run_with_cpuidle.sh) and
[benchmark binary](../benches/wake_latency.rs) own their current flags and numeric defaults. The
runner accepts the compiled binary, the four CPU roles, and benchmark arguments after `--`;
the benchmark's `--help` output is the option reference. The
[build helper](../scripts/build_benchmark.sh) enables the repository-only feature required for the
C1 diagnostic and prints the exact official-run artifact path.

## Compared strategies

The comparison includes:

- `std::thread::park` as a contextual scheduler baseline;
- busy spin;
- spin then yield;
- AMD `MONITORX/MWAITX`;
- spin then AMD `MONITORX/MWAITX`;
- a benchmark-only AMD C1-hint variant as a diagnostic boundary.

Both direct atomic and Parker forms are measured where applicable, including raw operations that
expose unclassified wakes and filtered operations that absorb them. Hybrid spin counts are swept
over the values owned by the benchmark configuration. A result must name its exact strategy and
parameters; the repository does not assert one universal optimum.

The C1 diagnostic is available only when the `benchmark-only` Cargo feature is enabled. The
official build helper enables it; an ordinary library build does not expose that diagnostic API.

## Workloads

### Saturated handoff

The producer publishes the next generation immediately after the consumer acknowledges the
previous one. Strict acknowledgement makes every recorded wake correspond to one publication and
prevents token coalescing from merging samples.

This workload answers: how quickly does the waiter return when another thread repeatedly drives it
as hard as the protocol allows?

### Bursty handoff

The producer follows a deterministic, versioned, seeded gap schedule with a mixture of immediate,
short, and longer gaps. The schedule version, seed, and selected configuration are written into
the result.

This workload answers: how does the strategy behave when it sometimes catches work while spinning
and sometimes reaches its sleeping phase?

### SMT-neighbor interference

The waiter and a compute-bound victim are pinned to the two logical CPUs of one physical core. The
producer and controller run on other physical cores. The control is a victim-only baseline of
equal duration with the victim pinned identically; the producer and waiter sibling are dormant.

The benchmark reports victim throughput loss and the change in the victim's high-percentile chunk
latency. Reported loss therefore conservatively includes both producer and waiter activity.
Because the baseline omits producer load, it cannot hide contender interference by charging the
same producer load to the control. Treatment/control order alternates to reduce thermal and
frequency drift. The acceptance limits are owned by the benchmark configuration and are emitted
with every result rather than copied into this document.

## Timing validity

Cycle timing is accepted only when preflight can establish the required processor and operating
system properties. The benchmark checks:

- invariant timestamp-counter support and the required ordered timestamp instruction;
- the active Linux clocksource;
- stable cycle-to-time calibration;
- measured producer-to-waiter timestamp skew within the code-owned uncertainty bound;
- the expected CPU before and after each sample;
- no migration during an accepted sample;
- the requested sibling and separate-core topology.

Invalid or migrated samples are counted and retained as diagnostics, not silently discarded.
Accepted samples receive the measured signed timestamp-offset correction. The output records the
offset, uncertainty, and applied correction; a run outside the allowed bound fails preflight.
Outliers remain in the distribution.

Every latency result reports cycles and calibrated time, distribution percentiles, maximum,
public-timeout count, unclassified-wake count, and invalid-sample count. Each corrected cycle
sample is also emitted in observation order as a machine-readable latency record.

## CPU-idle state lifecycle

The library never changes CPU idle states. The official runner is a separate operational boundary
with these obligations:

1. Acquire an exclusive lock so two official runs cannot modify the same state.
2. Discover all idle states on every assigned logical CPU.
3. Save each exact original `disable` value in a private recovery manifest.
4. Permit only POLL and C1; disable C2, C3, and every deeper or unknown state.
5. Read every value back and refuse timing if the requested state was not applied.
6. Print the warning at the top of this page before the first measurement.
7. Restore every original value on success, ordinary failure, and handled termination signals.
8. Preserve a dirty marker when restoration is incomplete so the next run fails closed and gives
   a recovery path.

The benchmark binary performs its own read-only verification against the kernel CPU sysfs tree. It
refuses official timing if a deeper state is enabled on an assigned CPU, even if it was launched
without the runner. `SNOOZER_SYSFS_ROOT` is accepted only by non-official smoke runs; official mode
rejects the override instead of trusting a custom tree.

`SIGKILL`, power loss, and a kernel crash cannot be handled in-process. Before another run,
inspect the recorded manifest and use the runner's explicit recovery command. Never guess the
original values.

The path checks assume the real CPU sysfs tree remains kernel-owned and cannot be renamed by an
unprivileged process while the runner is operating. `SNOOZER_SYSFS_ROOT` exists for smoke tests and
`SNOOZER_WRITE_HELPER` exists for runner tests and controlled integrations; POSIX-shell `realpath`
checks cannot make a concurrently modified custom tree safe against time-of-check/time-of-use
attacks. Do not use an untrusted or concurrently mutable tree for an official run. A custom
privileged helper that must support that threat model needs to open every component with
kernel-enforced containment, for example Linux `openat2` with
`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`, and perform the write and readback through those retained
descriptors.

## Result provenance

Machine-readable output records at least:

- result schema and workload versions;
- benchmark commit and dirty-worktree provenance;
- CPU model, microcode, kernel, and logical/physical topology;
- governor, energy-performance preference, active clocksource, and observed idle-state table;
- strategy, API contract, spin settings, timer calibration, and sample counts;
- timestamp offset, uncertainty, correction, and the victim-only control description;
- thread-to-CPU assignment;
- treatment/control order and repetition;
- whether the run is official.

### JSONL schema compatibility

The first JSONL line is the `metadata` record. Its `schema` value applies to the entire file.
Readers must select a version-specific decoder before interpreting later records and must reject
unknown values; inferring a version from field presence is not supported. The
[`RESULT_SCHEMA_VERSION`](../benches/support/pure.rs) constant owns the current writer version.

The writer builds each result in a unique same-directory partial file. Only a complete successful
run, including `--preflight-only`, flushes and synchronizes that file and atomically renames it to
the requested output path. A failed run leaves any existing published result untouched.

`snoozer-wake-latency-v1` reports tracked-only changes through
`compiled_tracked_working_tree_dirty` and `checkout_tracked_working_tree_dirty`. Its top-level
`governor` and `energy_preference` fields describe only the waiter CPU.

`snoozer-wake-latency-v2` reports tracked and non-ignored untracked changes through
`compiled_working_tree_dirty` and `checkout_working_tree_dirty`, adds `rustup_toolchain`, and
reports every assigned CPU through `power_policy` entries containing `cpu`, `governor`, and
`energy_preference`.

A reader supporting both versions must preserve these different meanings. In particular, it must
not present a v1 tracked-only flag as proof of a completely clean working tree. Writers emit only
their current schema and do not duplicate deprecated fields as aliases.

Console output begins with the mode-appropriate CPU-idle warning.

## Interpreting a winner

Only strategies within the configured SMT-neighbor interference limits are eligible. Eligible
strategies are ranked by wake-latency tail first, then deeper tail, then median. A failed preflight,
an unsupported strategy, an invalid topology, or an incomplete state restoration invalidates the
run rather than selecting a fallback.

Conclusions apply only to the recorded environment. Intel and Arm implementations require fresh
platform-specific measurements; AMD results cannot be used as their evidence.
