# Benchctl control plane

`benchctl` is the operational control plane for Snoozer benchmark builds and official
CPU-idle runs. It is separate from the `wake_latency` benchmark: Benchctl validates and
records an operation, controls host state, starts one supplied workload, and restores or exposes
recovery state. The benchmark owns its experiment, API matrix, timing, and result schema.

This separation is deliberate. Benchctl correctness and lifecycle tests use a disposable sysfs
tree and a minimal fixture workload. They do not run Snoozer strategies, `cargo bench`, smoke
mode, official measurement scenarios, or result-schema tests. Benchmark tests in turn do not
exercise CPU-idle mutation, locking, journals, or process cleanup.

## Commands

Run the tool through Cargo while developing:

```console
cargo run --locked --package benchctl -- build cargo-bench \
  --manifest-path Cargo.toml --bench wake_latency \
  --feature benchmark-only --receipt target/snoozer-bench/receipt.json

cargo run --locked --package benchctl -- run \
  --receipt target/snoozer-bench/receipt.json --cpuidle poll-c1 \
  --cpu "$SNOOZER_WAITER_CPU" --cpu "$SNOOZER_VICTIM_CPU" \
  --cpu "$SNOOZER_PRODUCER_CPU" --cpu "$SNOOZER_CONTROLLER_CPU" -- \
  --official --waiter-cpu "$SNOOZER_WAITER_CPU" --victim-cpu "$SNOOZER_VICTIM_CPU" \
  --producer-cpu "$SNOOZER_PRODUCER_CPU" --controller-cpu "$SNOOZER_CONTROLLER_CPU"

cargo run --locked --package benchctl -- status
cargo run --locked --package benchctl -- recover
```

The [`benchctl` package README](../tools/benchctl/README.md) and `benchctl --help` own the exact
command-line interface. The [`wake_latency` benchmark](../benches/wake_latency.rs) owns workload
arguments after `--`; do not copy its numeric measurement settings into this page.

The contract is intentionally reusable for similar Linux command-line tools: the control plane
accepts a verified executable plus arguments, supervises one cooperative process group, journals
typed host-state changes, and restores them. The first adapter is Snoozer's exact `POLL`/`C1`
CPU-idle policy. This is an internal typed design, not a plugin API or workflow language.

Both Cargo metadata resolution and compilation run under the crash guardian and share one build
deadline. Benchctl validates every reachable local manifest and Cargo target against tracked Git
inputs, then rechecks `HEAD`, the complete working tree, and the actual lockfile after compilation
before publishing a receipt. This detects cooperative or accidental checkout changes during a
build; deliberately racing same-UID code is outside the trusted-caller contract.

## Ownership and recovery contract

| Concern | Owner | Required behavior |
| --- | --- | --- |
| Build request, workload arguments, and selected CPUs | Caller | Supplies explicit input; a caller cannot claim an official result by bypassing receipt validation. |
| Receipt, operation ID, and request hash | Authenticated Benchctl coordinator | Binds the accepted build and run request to one durable operation; status and recovery identify that operation, not a guessed path. |
| CPU-idle inventory, write authorization, and original values | Benchctl | Validates the selected CPUs, writes a durable journal before the first mutation, reads each requested value back, and retains the journal until exact restoration. |
| Benchmark process group and cleanup | Benchctl | Starts only the receipt-authorized workload, gives it bounded grace and drain phases, and refuses restoration while the group cannot be proved drained. |
| Journal, receipt, and completed-operation records | Stored Benchctl state | Remains authoritative after caller disconnect or process loss; `status` discovers the operation and `recover` resumes only validated recorded work. |

The workload is trusted to remain in its process group; Benchctl is not a sandbox and does not
claim containment of a hostile process that calls `setsid` or moves itself elsewhere. On real
sysfs, one invocation of the same executable crosses `sudo`; only the coordinator stays root and
the workload is launched with the invoking UID/GID and cleared supplementary groups.

The coordinator opens a Linux pidfd for the unprivileged client. Client exit means cancellation
without consuming the workload's standard input. The coordinator asks the guardian to terminate
and prove the process group empty before restoring. If the
coordinator is killed, the guardian still drains the group but deliberately leaves the durable
journal and applied CPU-idle policy for explicit `recover`; it never guesses restoration state.

Every accepted request is named by an operation ID and a request hash. A retry with different
inputs is a new request, not a continuation of a prior operation. A missing, malformed, or
ambiguous journal fails closed: the tool reports the operation through `status` and requires
`recover` rather than guessing original CPU-idle values.

The build timeout, workload timeout, graceful-stop interval, forced-drain interval, and recovery
lock-wait interval are independent boundaries. Their current values are owned by
[`benchctl`](../tools/benchctl/README.md), not this document. A build timeout prevents an
unbounded compiler invocation; a workload timeout begins workload shutdown; grace and drain prove
the process group has ended before restore; and the recovery wait prevents a second controller
from racing an active or guardian-owned operation.

## Evidence boundary

`benchctl` can make an official run operationally controlled only after it has applied and
read back the policy and later restored it. That does not make a performance conclusion portable:
the benchmark output and [benchmarking method](benchmarking.md) remain the source of truth for
hardware, topology, timing, and result interpretation.

Historical result decoders retain their existing version-specific meaning. Benchctl owns operation
receipts and recovery records; it does not reinterpret benchmark JSONL schemas or silently map an
old result schema to a new one.

The coordinator snapshots both the accepted receipt and executable into its root-owned operation
directory. It hashes the executable snapshot, opens that exact inode with `O_NOFOLLOW`, hashes the
open descriptor again, and executes it through the inherited descriptor after dropping privilege;
the caller-owned artifact path is never reopened for execution. The workload receives the
validated receipt snapshot as `BENCHCTL_BUILD_RECEIPT_JSON`, not a caller-owned path,
and inherits a root-owned FIFO descriptor named by `BENCHCTL_PRODUCTION_CONTROL_FD` only for the
fixed real-sysfs backend. The one-shot proof binds the operation ID and build ID; the benchmark
checks the kernel-reported owner, FIFO type, and a namespace-root-to-host-UID-0 mapping. A copied
receipt, swapped artifact path, forged environment variable, or user-namespace root pipe therefore
cannot turn a direct or fake-root run into official evidence. `wake_latency` binds the running
snapshot to the receipt by hashing `/proc/self/exe`, not by comparing its root-owned snapshot path
with the original Cargo output path.
