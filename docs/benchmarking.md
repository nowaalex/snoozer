# Benchmarking

> [!CAUTION]
> **Official benchmarks enable only POLL and exact C1 on every assigned logical CPU. C1E and every
> other CPU idle state, including C2, C3, and deeper states, are disabled because their exit latency
> conflicts with the minimum-wake-latency objective. These results do not represent the machine's
> default power-saving configuration.**

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

Use the commands in [Contributing](../CONTRIBUTING.md). [`benchctl`](benchctl.md) owns the official
build receipt, CPU-idle lifecycle, and recovery interface; the
[benchmark binary](../benches/wake_latency.rs) owns arguments after `--` and its `--help` output is
the option reference. Benchctl builds with the pinned toolchain and locked dependency graph, rejects
a dirty checkout, and writes a versioned receipt for the exact benchmark executable. It verifies
that receipt and the checkout before starting an official run. `wake_latency --official` then
requires the accepted receipt JSON passed by Benchctl, checks that it matches its compile-time
stamps and executable, and embeds its identity in the JSONL metadata. It never repeats Git or Cargo
provenance checks itself. Smoke mode does not require a receipt and records `unknown` checkout
provenance rather than claiming official evidence.

Official mode also requires readable CPU governor and energy-performance preference values for the
waiter, victim, producer, and controller CPUs. Any failed check rejects the run. Smoke mode records
`unknown` for an unreadable power-policy value instead of claiming official evidence.

## Compared strategies

The comparison includes:

- `std::thread::park` as a contextual scheduler baseline;
- busy spin;
- spin then yield;
- AMD `MONITORX/MWAITX`;
- spin then AMD `MONITORX/MWAITX`;
- a benchmark-only AMD C1-hint variant as a diagnostic boundary.

Direct atomic, single-producer Parker, and multi-producer Parker forms are measured where
applicable, including raw operations that expose unclassified wakes and filtered operations that
absorb them. The same one-producer workload drives both Parker forms so their difference isolates
the notification publication contract: a Release store for `single_parker` and a Release RMW for
`multi_parker`. It does not model contention among multiple producers. Hybrid spin counts are
swept over the values owned by the benchmark configuration. A result must name its exact strategy,
surface, and parameters; the repository does not assert one universal optimum.

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

Benchctl is the current lifecycle owner. It journals the exact original values before mutation,
uses one privileged coordinator and a crash guardian to drain the workload process group, and
performs conditional restoration. `status` and `recover` operate on the durable journal; a
conflicting external CPU-idle change is retained for explicit resolution rather than overwritten.
The complete operational contract, including timeouts and recovery semantics, is in
[Benchctl](benchctl.md). The legacy shell helpers below remain compatibility evidence during the
migration; do not use them for new official runs.

The library never changes CPU idle states. Benchctl implements the operational boundary with these
obligations:

1. Acquire an exclusive lock so two official runs cannot modify the same state.
2. Discover all idle states on every assigned logical CPU.
3. Save each exact original `disable` value in a private recovery manifest.
4. Permit only POLL and exact C1; disable C1E and every other state, including C2, C3, and
   deeper or unknown states.
5. Read every value back and refuse timing if the requested state was not applied.
6. Print the warning at the top of this page before the first measurement.
7. Restore every original value on success, ordinary failure, and handled termination signals.
8. Preserve a dirty marker when restoration is incomplete so the next run fails closed and gives
   a recovery path.

The benchmark binary performs its own read-only verification against the kernel CPU sysfs tree. It
refuses official timing if any state other than exact POLL/C1 is enabled on an assigned CPU, even if
it was launched without the runner. `SNOOZER_SYSFS_ROOT` is accepted only by non-official smoke
runs; official mode rejects the override instead of trusting a custom tree.

For real sysfs, the normal-user client starts the same Benchctl executable once through `sudo`.
The root coordinator owns the fixed state directory, journal and sysfs writes. It launches the
receipt-authorized executable under the invoking UID/GID in a new process group; setting the UID
also clears supplementary groups. A Linux pidfd represents client liveness, so client exit is
cancellation even if the client itself was killed, without reserving the workload's standard input.

Before workload authorization, the coordinator starts a separate guardian outside that process
group. The guardian owns `active.lock`, receives the verified PGID and reports ready before the
coordinator publishes GO. Normal exit, timeout, cancellation and workload failure all request a
TERM/grace/KILL drain. The guardian reports success only after `killpg(..., 0)` proves the group
absent; the coordinator reaps the supervisor concurrently so a zombie group leader cannot block
that proof. CPU-idle restoration starts only after the proof.

If the coordinator dies, the guardian still drains the group and releases `active.lock`, but it
does not restore CPU-idle policy. The versioned Benchctl journal remains authoritative and
`recover` performs the later conditional restore. Recovery waits a bounded interval for the
guardian lock, validates the boot and complete current inventory, and writes an original value only
when the current value is either the recorded desired value or already the original. A third value
is an external conflict: it is retained and never overwritten.

This is trusted process-group supervision, not containment of hostile code. A workload that calls
`setsid` or otherwise escapes its group is outside the contract. Production path assumptions are
limited to the fixed kernel-owned CPU sysfs tree and fixed root-owned state directory; alternate
roots are hidden integration-test seams.

During migration Benchctl uses the same global lock as the shell runner. It rejects new work while
an old `SNOOZER_GLOBAL_DIRTY_V1` record exists and can recover its referenced
`SNOOZER_CPUIDLE_V2` manifest after validating ownership, mode, boot, paths, names, values and exact
inventory. Unknown formats fail closed. The legacy scripts and their fixture tests remain in CI as
compatibility evidence, not as the official command path.

## Result provenance

Machine-readable output records at least:

- result schema and workload versions;
- receipt-backed benchmark commit, locked dependency, compiler, toolchain, executable, and
  dirty-worktree provenance for official runs;
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
`energy_preference`. Its `summary` and non-null `winner` records expose median p50, p99, and p99.9
in both corrected TSC cycles and calibrated nanoseconds. The cycle fields are the authoritative
ranking values; nanoseconds are the rounded reporting view.

A reader supporting both versions must preserve these different meanings. In official v2 output,
`provenance_source` is `benchctl_build_receipt`, `build_receipt` carries the versioned receipt
identity, including Cargo package and Benchctl versions, and the checkout fields mean the receipt
was accepted by Benchctl before launch; they are
not a second Git query from the benchmark process. Smoke output has `provenance_source` set to
`compile_stamps`, a null receipt, and unknown checkout fields. In particular, a reader must not
present a v1 tracked-only flag as proof of a completely clean working tree. Writers emit only their
current schema and do not duplicate deprecated fields as aliases.

Console output begins with the mode-appropriate CPU-idle warning.

## Interpreting a winner

For each strategy and workload, eligibility compares the median victim-throughput loss and median
victim-p99 chunk-latency degradation with the code-owned limits emitted in metadata. Eligibility
also requires zero invalid samples, zero migrated samples, and no repetition that reached the
sample cap.

Eligible strategies are ranked lexicographically by median p99 corrected TSC cycles, then median
p99.9 cycles, then median p50 cycles. Cycle counts, rather than rounded nanoseconds, own the
selection. A failed preflight, an unsupported strategy, an invalid topology, or an incomplete
state restoration invalidates the run rather than selecting a fallback.

Conclusions apply only to the recorded environment. Intel and Arm implementations require fresh
platform-specific measurements; AMD results cannot be used as their evidence.
