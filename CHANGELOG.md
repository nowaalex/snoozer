# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/nowaalex/snoozer/releases/tag/v0.1.0) - 2026-09-02

### Other

- simplify API guide and add release automation
- Add cross-vendor hardware wait preflight
- Add universal Benchctl benchmark runner
- Close remaining MVP review findings
- Benchmark single and multi Parker paths
- Harden benchmark smoke and CI gate
- Optimize untimed AMD MWAITX path
- Split parker API by producer count
- Document explicit Parker producer cardinality
- Close cpuidle guardian teardown races
- Harden cpuidle runner against supervisor death
- Fix dash process-group signaling
- Add documented Just development recipes
- Isolate benchmark build fixtures from CI
- Harden benchmark CI bounds
- Bound every benchmark shell gate
- Correct waiting and benchmark operational docs
- Close benchmark review gaps
- Make official idle-state warning conditional
- Reject aborted benchmark waiter startup
- Honor temporary directory in benchmark preflight test
- Make hardware evidence gate portable
- Harden hardware evidence and API evolution
- Fix benchmark measurement and output invariants
- Make benchmark crash recovery exclusive
- Use one monotonic runner shutdown deadline
- Close benchmark runner recovery races
- Test failed cpuidle apply recovery
- Pin benchmark build provenance
- Drain benchmark descendants after timeout
- Document benchmark schema compatibility
- Document custom sysfs trust boundary
- Mutation-test CPUID and TSC arithmetic
- Tie concurrency proofs to production paths
- Expand mutation coverage across MWAITX protocol
- Make mutation testing bounded and deterministic
- Finalize benchmark integration contracts
- Harden benchmark provenance and cpuidle recovery
- Model coalesced notification ordering with Loom
- Align benchmark integration contracts
- Add precision wake latency benchmark and cpuidle runner
- Preserve coalesced producer synchronization
- Add project documentation and quality tooling
- Implement low-latency wait primitives
