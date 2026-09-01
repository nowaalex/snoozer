# Architecture decisions

This index is for maintainers who need the stable rationale behind cross-cutting choices. Each
accepted record is immutable; a later change supersedes it with a new record and links both
directions.

| ID                                       | Status   | Decision                                      |
| ---------------------------------------- | -------- | --------------------------------------------- |
| [0001](0001-two-level-waiting-api.md)    | Accepted | Expose raw and filtered waiting contracts     |
| [0002](0002-custom-benchmark-harness.md) | Accepted | Use a custom coordinated wake-latency harness |
| [0003](0003-explicit-producer-cardinality.md) | Accepted | Expose producer cardinality in Parker types |
| [0004](0004-separate-benchmark-control-plane.md) | Accepted | Separate benchmark control from experiment execution |
| [0005](0005-mandatory-hardware-preflight.md) | Accepted | Require process-wide hardware-wait preflight |
