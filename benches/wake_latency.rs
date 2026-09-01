#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("wake_latency currently requires Linux x86_64");
    std::process::exit(2);
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "support/platform.rs"]
mod platform;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "support/pure.rs"]
mod pure;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "support/snoozer_api.rs"]
mod snoozer_api;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux {
    use std::error::Error;
    use std::fs::{self, File};
    use std::hint::black_box;
    use std::io::{BufWriter, Write};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use snoozer::{AmdMwaitx, BusySpin, SpinThenAmdMwaitx, SpinThenYield, WaitStrategy, pair};

    use crate::platform::{
        CpuIdleState, CpuPowerPolicy, Topology, TscClock, TscSkew, cpu_metadata, cpu_power_policy,
        pin_current, stamp,
    };
    use crate::pure::{GapSchedule, correct_latency, json_escape, percentile_sorted};
    use crate::snoozer_api::{
        Observation, wait_direct_filtered, wait_direct_raw, wait_parker_filtered, wait_parker_raw,
    };

    type AnyError = Box<dyn Error + Send + Sync>;
    type AnyResult<T> = Result<T, AnyError>;

    const CSTATE_WARNING: &str = "C2/C3 and every deeper CPU idle state are disabled because their exit latency conflicts with the minimum-wake-latency objective. These results do not represent the default power-saving configuration.";
    const SMOKE_CSTATE_WARNING: &str = "NON-OFFICIAL SMOKE: C2/C3 and deeper CPU idle states may be enabled. Official runs disable them because their exit latency conflicts with the minimum-wake-latency objective.";
    const RESULT_SCHEMA_VERSION: &str = "snoozer-wake-latency-v1";
    const BURSTY_SCHEDULE_VERSION: &str = "bursty-v1";
    const BURSTY_SEED: u64 = 0x5a17_9d3c_e821_4b6f;
    const SPIN_SWEEP: [usize; 4] = [32, 128, 512, 2_048];
    const VICTIM_CHUNK_OPERATIONS: u64 = 4_096;
    const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
    const MAX_THROUGHPUT_LOSS_PERCENT: f64 = 5.0;
    const MAX_P99_DEGRADATION_PERCENT: f64 = 10.0;
    const COMPILED_COMMIT: Option<&str> = option_env!("SNOOZER_BENCHMARK_COMMIT");
    const COMPILED_REPOSITORY: Option<&str> = option_env!("SNOOZER_BENCHMARK_REPOSITORY");
    const COMPILED_RUSTC: Option<&str> = option_env!("SNOOZER_BENCHMARK_RUSTC");
    const COMPILED_RUSTUP_TOOLCHAIN: Option<&str> =
        option_env!("SNOOZER_BENCHMARK_RUSTUP_TOOLCHAIN");
    const COMPILED_DIRTY: Option<&str> = option_env!("SNOOZER_BENCHMARK_DIRTY");

    pub(crate) fn main() -> AnyResult<()> {
        let arguments = Arguments::parse()?;
        if arguments.mode == Mode::Official && !cfg!(feature = "benchmark-only") {
            return Err("official mode requires --features benchmark-only for the complete diagnostic matrix".into());
        }
        let cstate_warning = match arguments.mode {
            Mode::Official => CSTATE_WARNING,
            Mode::Smoke => SMOKE_CSTATE_WARNING,
        };
        println!("{cstate_warning}");
        println!("Benchmark mode: {}", arguments.mode.as_str());

        let sysfs_root = std::env::var_os("SNOOZER_SYSFS_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/sys/devices/system/cpu"));
        let topology = Topology::discover(arguments.cpu_overrides(), sysfs_root.clone())?;
        let cpuidle = match arguments.mode {
            Mode::Official => topology.validate_cpuidle()?,
            Mode::Smoke => topology.read_cpuidle()?,
        };

        // Prove that every selected affinity is permitted before any thread is
        // started. The controller remains pinned after the final call.
        for cpu in topology.selected() {
            pin_current(cpu)?;
        }
        pin_current(topology.controller)?;
        let clock = TscClock::preflight()?;
        let skew = TscSkew::preflight(
            topology.producer,
            topology.waiter,
            topology.controller,
            clock,
        )?;
        let provenance = repository_provenance();
        if arguments.mode == Mode::Official
            && (provenance.compiled_commit == "unknown"
                || provenance.compiled_dirty != Some(false)
                || COMPILED_RUSTC.is_none()
                || COMPILED_RUSTUP_TOOLCHAIN.is_none()
                || provenance.checkout_commit != provenance.compiled_commit
                || provenance.checkout_dirty != Some(false))
        {
            return Err("official mode requires a build stamped by scripts/build_benchmark.sh, the same commit still checked out, and no working-tree changes (including untracked files)".into());
        }
        if arguments.mode == Mode::Official
            && let Err(error) = AmdMwaitx::new()
        {
            return Err(format!(
                "official mode requires the complete AMD MWAITX comparison matrix: {error}"
            )
            .into());
        }
        let metadata = cpu_metadata(topology.waiter);
        let power_policies = topology
            .selected()
            .into_iter()
            .map(|cpu| match cpu_power_policy(cpu, &sysfs_root) {
                Ok(policy) => Ok(policy),
                Err(_) if arguments.mode == Mode::Smoke => Ok(CpuPowerPolicy {
                    cpu,
                    governor: "unknown".to_owned(),
                    energy_preference: "unknown".to_owned(),
                }),
                Err(error) => Err(format!(
                    "official mode requires readable governor and energy preference for CPU {cpu}: {error}"
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut output = JsonlOutput::create(&arguments.output)?;
        output.metadata(MetadataInput {
            arguments: &arguments,
            topology: &topology,
            cpuidle: &cpuidle,
            clock,
            skew,
            cpu: &metadata,
            power_policies: &power_policies,
            provenance: &provenance,
            cstate_warning,
        })?;

        eprintln!(
            "preflight passed: waiter={} victim={} producer={} controller={}, TSC {:.6} cycles/ns ({:.3}% spread)",
            topology.waiter,
            topology.victim,
            topology.producer,
            topology.controller,
            clock.cycles_per_ns,
            clock.spread_percent
        );
        if arguments.preflight_only {
            output.flush()?;
            return Ok(());
        }

        let cases = all_cases(arguments.case_filter.as_deref())?;
        let mut summaries = Vec::new();
        for workload in [Workload::Saturated, Workload::Bursty] {
            let mut workload_summaries = Vec::new();
            for (case_index, case) in cases.iter().enumerate() {
                let mut repetitions = Vec::new();
                let mut unsupported = None;
                for repetition in 0..arguments.repetitions {
                    let control_first = (case_index + repetition) % 2 == 0;
                    let pair_result = if control_first {
                        let control =
                            run_victim_control(arguments.trial_duration, topology.victim, clock)?;
                        match run_case_with_smoke_retry(
                            *case, workload, &arguments, &topology, clock,
                        ) {
                            Ok(contender) => Ok((control, contender)),
                            Err(RunCaseError::Unsupported(reason)) => Err(reason),
                            Err(RunCaseError::Failed(error)) => return Err(error),
                        }
                    } else {
                        match run_case_with_smoke_retry(
                            *case, workload, &arguments, &topology, clock,
                        ) {
                            Ok(contender) => {
                                let control = run_victim_control(
                                    arguments.trial_duration,
                                    topology.victim,
                                    clock,
                                )?;
                                Ok((control, contender))
                            }
                            Err(RunCaseError::Unsupported(reason)) => Err(reason),
                            Err(RunCaseError::Failed(error)) => return Err(error),
                        }
                    };

                    let (control, contender) = match pair_result {
                        Ok(pair) => pair,
                        Err(reason) => {
                            if arguments.mode == Mode::Official {
                                return Err(format!(
                                    "official mode requires every benchmark case; {} is unsupported: {reason}",
                                    case.name()
                                )
                                .into());
                            }
                            unsupported = Some(reason);
                            break;
                        }
                    };
                    let mut result = RepetitionResult::new(
                        repetition,
                        control_first,
                        control,
                        contender,
                        clock,
                        skew.correction_cycles,
                    )?;
                    output.repetition(*case, workload, &result)?;
                    eprintln!(
                        "{} {} rep={} samples={} p99={}ns victim_loss={:.2}% p99_cost={:.2}%",
                        workload.as_str(),
                        case.name(),
                        repetition,
                        result.samples,
                        result.p99_ns,
                        result.throughput_loss_percent,
                        result.victim_p99_degradation_percent
                    );
                    result.raw_latency_cycles.clear();
                    repetitions.push(result);
                }
                if let Some(reason) = unsupported {
                    output.skipped(*case, workload, &reason)?;
                    eprintln!("SKIP {} {}: {reason}", workload.as_str(), case.name());
                    continue;
                }
                let summary = Summary::from_repetitions(*case, workload, &repetitions)?;
                output.summary(&summary)?;
                workload_summaries.push(summary.clone());
                summaries.push(summary);
            }
            if let Some(winner) = choose_winner(&workload_summaries) {
                output.winner(winner)?;
                eprintln!(
                    "winner {}: {} (median p99={}ns, victim loss={:.2}%, victim p99 cost={:.2}%)",
                    workload.as_str(),
                    winner.case.name(),
                    winner.p99_ns,
                    winner.throughput_loss_percent,
                    winner.victim_p99_degradation_percent
                );
            } else {
                output.no_winner(workload)?;
                eprintln!("no eligible winner for {}", workload.as_str());
            }
        }
        output.flush()?;
        black_box(summaries);
        Ok(())
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Mode {
        Smoke,
        Official,
    }

    impl Mode {
        fn as_str(self) -> &'static str {
            match self {
                Self::Smoke => "smoke (non-official)",
                Self::Official => "official",
            }
        }
    }

    #[derive(Debug)]
    struct Arguments {
        mode: Mode,
        trial_duration: Duration,
        repetitions: usize,
        warmup_events: usize,
        max_samples: usize,
        waiter_cpu: Option<usize>,
        victim_cpu: Option<usize>,
        producer_cpu: Option<usize>,
        controller_cpu: Option<usize>,
        output: PathBuf,
        case_filter: Option<String>,
        preflight_only: bool,
    }

    impl Arguments {
        fn parse() -> AnyResult<Self> {
            let mut mode = None;
            let mut duration_ms = None;
            let mut repetitions = None;
            let mut warmup_events = None;
            let mut max_samples = None;
            let mut waiter_cpu = None;
            let mut victim_cpu = None;
            let mut producer_cpu = None;
            let mut controller_cpu = None;
            let mut output = PathBuf::from("target/snoozer-bench/results.jsonl");
            let mut case_filter = None;
            let mut preflight_only = false;
            let mut args = std::env::args().skip(1);
            while let Some(argument) = args.next() {
                match argument.as_str() {
                    // Cargo appends this libtest-compatible marker even though
                    // this target owns its harness.
                    "--bench" => {}
                    "--smoke" => set_once(&mut mode, Mode::Smoke, "--smoke/--official")?,
                    "--official" => set_once(&mut mode, Mode::Official, "--smoke/--official")?,
                    "--duration-ms" => set_once(
                        &mut duration_ms,
                        parse_next(&mut args, "--duration-ms")?,
                        "--duration-ms",
                    )?,
                    "--repetitions" => set_once(
                        &mut repetitions,
                        parse_next(&mut args, "--repetitions")?,
                        "--repetitions",
                    )?,
                    "--warmup-events" => set_once(
                        &mut warmup_events,
                        parse_next(&mut args, "--warmup-events")?,
                        "--warmup-events",
                    )?,
                    "--max-samples" => set_once(
                        &mut max_samples,
                        parse_next(&mut args, "--max-samples")?,
                        "--max-samples",
                    )?,
                    "--waiter-cpu" => set_once(
                        &mut waiter_cpu,
                        parse_next(&mut args, "--waiter-cpu")?,
                        "--waiter-cpu",
                    )?,
                    "--victim-cpu" => set_once(
                        &mut victim_cpu,
                        parse_next(&mut args, "--victim-cpu")?,
                        "--victim-cpu",
                    )?,
                    "--producer-cpu" => set_once(
                        &mut producer_cpu,
                        parse_next(&mut args, "--producer-cpu")?,
                        "--producer-cpu",
                    )?,
                    "--controller-cpu" => set_once(
                        &mut controller_cpu,
                        parse_next(&mut args, "--controller-cpu")?,
                        "--controller-cpu",
                    )?,
                    "--output" => output = PathBuf::from(next_string(&mut args, "--output")?),
                    "--case" => case_filter = Some(next_string(&mut args, "--case")?),
                    "--preflight-only" => preflight_only = true,
                    "--help" | "-h" => {
                        print_help();
                        std::process::exit(0);
                    }
                    unknown => return Err(format!("unknown argument: {unknown}").into()),
                }
            }
            let mode = mode.unwrap_or(Mode::Smoke);
            let defaults = match mode {
                Mode::Smoke => (40_u64, 1_usize, 128_usize, 250_000_usize),
                Mode::Official => (250_u64, 7_usize, 1_024_usize, 5_000_000_usize),
            };
            let duration_ms = duration_ms.unwrap_or(defaults.0);
            let repetitions = repetitions.unwrap_or(defaults.1);
            let warmup_events = warmup_events.unwrap_or(defaults.2);
            let max_samples = max_samples.unwrap_or(defaults.3);
            if duration_ms == 0 || repetitions == 0 || max_samples == 0 {
                return Err("duration, repetitions, and max samples must be positive".into());
            }
            if mode == Mode::Official
                && (duration_ms < 250
                    || repetitions < 7
                    || warmup_events < 1_000
                    || max_samples < 10_000)
            {
                return Err("official mode requires duration>=250ms, repetitions>=7, warmup-events>=1000, and max-samples>=10000".into());
            }
            if mode == Mode::Official && case_filter.is_some() {
                return Err("--case is available only for non-official smoke runs; official mode always measures the complete matrix".into());
            }
            let role_count = [waiter_cpu, victim_cpu, producer_cpu, controller_cpu]
                .iter()
                .filter(|value| value.is_some())
                .count();
            if !matches!(role_count, 0 | 4) {
                return Err(
                    "CPU roles must be omitted together or supplied as a complete set".into(),
                );
            }
            Ok(Self {
                mode,
                trial_duration: Duration::from_millis(duration_ms),
                repetitions,
                warmup_events,
                max_samples,
                waiter_cpu,
                victim_cpu,
                producer_cpu,
                controller_cpu,
                output,
                case_filter,
                preflight_only,
            })
        }

        fn cpu_overrides(&self) -> Option<[usize; 4]> {
            Some([
                self.waiter_cpu?,
                self.victim_cpu?,
                self.producer_cpu?,
                self.controller_cpu?,
            ])
        }
    }

    fn set_once<T>(slot: &mut Option<T>, value: T, name: &str) -> AnyResult<()> {
        if slot.is_some() {
            return Err(format!("{name} may be specified only once").into());
        }
        *slot = Some(value);
        Ok(())
    }

    fn parse_next<T>(args: &mut impl Iterator<Item = String>, name: &str) -> AnyResult<T>
    where
        T: std::str::FromStr,
        T::Err: Error + Send + Sync + 'static,
    {
        Ok(next_string(args, name)?.parse()?)
    }

    fn next_string(args: &mut impl Iterator<Item = String>, name: &str) -> AnyResult<String> {
        args.next()
            .ok_or_else(|| format!("{name} requires a value").into())
    }

    fn print_help() {
        println!(
            "wake_latency [--smoke|--official] [--duration-ms N] [--repetitions N] \
             [--warmup-events N] [--max-samples N] [--output PATH] [--case NAME] \
             [--preflight-only] [--waiter-cpu N --victim-cpu N --producer-cpu N \
             --controller-cpu N]"
        );
    }

    #[derive(Debug)]
    struct RepositoryProvenance {
        compiled_commit: String,
        compiled_dirty: Option<bool>,
        checkout_commit: String,
        checkout_dirty: Option<bool>,
    }

    fn repository_provenance() -> RepositoryProvenance {
        let compiled_commit = COMPILED_COMMIT
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| "unknown".to_owned());
        let compiled_dirty = match COMPILED_DIRTY {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        };
        let checkout_commit = git_in_compiled_repository(&["rev-parse", "--verify", "HEAD"])
            .map(|output| String::from_utf8_lossy(&output).trim().to_owned())
            .unwrap_or_else(|| "unknown".to_owned());
        let checkout_dirty =
            git_in_compiled_repository(&["status", "--porcelain", "--untracked-files=all"])
                .map(|output| !output.is_empty());
        RepositoryProvenance {
            compiled_commit,
            compiled_dirty,
            checkout_commit,
            checkout_dirty,
        }
    }

    fn git_in_compiled_repository(arguments: &[&str]) -> Option<Vec<u8>> {
        let repository = COMPILED_REPOSITORY.filter(|value| !value.is_empty())?;
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(arguments)
            .output()
            .ok()?;
        output.status.success().then_some(output.stdout)
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Workload {
        Saturated,
        Bursty,
    }

    impl Workload {
        fn as_str(self) -> &'static str {
            match self {
                Self::Saturated => "saturated",
                Self::Bursty => "bursty",
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Surface {
        Direct,
        Parker,
        StdPark,
    }

    impl Surface {
        fn as_str(self) -> &'static str {
            match self {
                Self::Direct => "direct_atomic",
                Self::Parker => "parker",
                Self::StdPark => "std_parker",
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Filtering {
        Raw,
        Filtered,
    }

    impl Filtering {
        fn as_str(self) -> &'static str {
            match self {
                Self::Raw => "raw",
                Self::Filtered => "filtered",
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum StrategyKind {
        BusySpin,
        SpinThenYield(usize),
        AmdMwaitx,
        SpinThenAmdMwaitx(usize),
        AmdMwaitxC1,
        StdPark,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Case {
        strategy: StrategyKind,
        surface: Surface,
        filtering: Filtering,
    }

    impl Case {
        fn name(self) -> String {
            let strategy = match self.strategy {
                StrategyKind::BusySpin => "busy_spin".to_owned(),
                StrategyKind::SpinThenYield(iterations) => format!("spin_then_yield_{iterations}"),
                StrategyKind::AmdMwaitx => "amd_mwaitx".to_owned(),
                StrategyKind::SpinThenAmdMwaitx(iterations) => {
                    format!("spin_then_amd_mwaitx_{iterations}")
                }
                StrategyKind::AmdMwaitxC1 => "amd_mwaitx_c1_diagnostic".to_owned(),
                StrategyKind::StdPark => "std_park".to_owned(),
            };
            format!(
                "{strategy}/{}/{}",
                self.surface.as_str(),
                self.filtering.as_str()
            )
        }
    }

    fn all_cases(filter: Option<&str>) -> AnyResult<Vec<Case>> {
        let mut cases = Vec::new();
        for strategy in [
            StrategyKind::BusySpin,
            StrategyKind::AmdMwaitx,
            StrategyKind::AmdMwaitxC1,
        ] {
            append_surface_matrix(&mut cases, strategy);
        }
        for iterations in SPIN_SWEEP {
            append_surface_matrix(&mut cases, StrategyKind::SpinThenYield(iterations));
            append_surface_matrix(&mut cases, StrategyKind::SpinThenAmdMwaitx(iterations));
        }
        cases.push(Case {
            strategy: StrategyKind::StdPark,
            surface: Surface::StdPark,
            filtering: Filtering::Raw,
        });
        if let Some(filter) = filter {
            cases.retain(|case| case.name() == filter);
            if cases.is_empty() {
                return Err(format!("unknown benchmark case: {filter}").into());
            }
        }
        Ok(cases)
    }

    fn append_surface_matrix(cases: &mut Vec<Case>, strategy: StrategyKind) {
        for surface in [Surface::Direct, Surface::Parker] {
            for filtering in [Filtering::Raw, Filtering::Filtered] {
                cases.push(Case {
                    strategy,
                    surface,
                    filtering,
                });
            }
        }
    }

    #[repr(align(128))]
    struct CachePadded<T>(T);

    struct TrialShared {
        generation: CachePadded<AtomicU64>,
        sent_cycles: CachePadded<AtomicU64>,
        acknowledged: CachePadded<AtomicU64>,
        ready: CachePadded<AtomicUsize>,
        go: CachePadded<AtomicBool>,
        stop: CachePadded<AtomicBool>,
    }

    impl TrialShared {
        fn new() -> Self {
            Self {
                generation: CachePadded(AtomicU64::new(0)),
                sent_cycles: CachePadded(AtomicU64::new(0)),
                acknowledged: CachePadded(AtomicU64::new(0)),
                ready: CachePadded(AtomicUsize::new(0)),
                go: CachePadded(AtomicBool::new(false)),
                stop: CachePadded(AtomicBool::new(false)),
            }
        }

        fn thread_ready(&self) {
            self.ready.0.fetch_add(1, Ordering::Release);
            while !self.go.0.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
        }

        fn abort_startup(&self) {
            self.stop.0.store(true, Ordering::Release);
            self.go.0.store(true, Ordering::Release);
            self.generation.0.fetch_add(1, Ordering::Release);
        }
    }

    #[derive(Debug)]
    struct WaiterMetrics {
        latencies_cycles: Vec<i64>,
        unclassified_wakes: Option<u64>,
        public_timeouts: u64,
        invalid_samples: u64,
        migrated_samples: u64,
        sample_limit_reached: bool,
    }

    #[derive(Clone, Copy, Debug)]
    struct VictimMetrics {
        operations: u64,
        elapsed: Duration,
        p99_chunk_ns: u64,
    }

    impl VictimMetrics {
        fn throughput_mops(self) -> f64 {
            self.operations as f64 / self.elapsed.as_secs_f64() / 1_000_000.0
        }
    }

    #[derive(Debug)]
    struct ContenderTrial {
        waiter: WaiterMetrics,
        victim: VictimMetrics,
    }

    enum RunCaseError {
        Unsupported(String),
        Failed(AnyError),
    }

    impl From<AnyError> for RunCaseError {
        fn from(error: AnyError) -> Self {
            Self::Failed(error)
        }
    }

    fn run_case(
        case: Case,
        workload: Workload,
        arguments: &Arguments,
        topology: &Topology,
        clock: TscClock,
    ) -> Result<ContenderTrial, RunCaseError> {
        match case.strategy {
            StrategyKind::BusySpin => {
                run_strategy(BusySpin, case, workload, arguments, topology, clock)
            }
            StrategyKind::SpinThenYield(iterations) => run_strategy(
                SpinThenYield::new(iterations),
                case,
                workload,
                arguments,
                topology,
                clock,
            ),
            StrategyKind::AmdMwaitx => {
                let strategy = AmdMwaitx::new()
                    .map_err(|error| RunCaseError::Unsupported(error.to_string()))?;
                run_strategy(strategy, case, workload, arguments, topology, clock)
            }
            StrategyKind::SpinThenAmdMwaitx(iterations) => {
                let strategy = SpinThenAmdMwaitx::new(iterations)
                    .map_err(|error| RunCaseError::Unsupported(error.to_string()))?;
                run_strategy(strategy, case, workload, arguments, topology, clock)
            }
            StrategyKind::AmdMwaitxC1 => run_c1(case, workload, arguments, topology, clock),
            StrategyKind::StdPark => run_std_park(case, workload, arguments, topology, clock)
                .map_err(RunCaseError::Failed),
        }
    }

    fn run_case_with_smoke_retry(
        case: Case,
        workload: Workload,
        arguments: &Arguments,
        topology: &Topology,
        clock: TscClock,
    ) -> Result<ContenderTrial, RunCaseError> {
        let mut retry = 0_u8;
        loop {
            match run_case(case, workload, arguments, topology, clock) {
                Err(RunCaseError::Failed(error)) if arguments.mode == Mode::Smoke && retry < 2 => {
                    retry += 1;
                    eprintln!(
                        "SMOKE RETRY {retry}/2 {} {} after transient trial failure: {error}",
                        workload.as_str(),
                        case.name()
                    );
                }
                result => return result,
            }
        }
    }

    fn run_strategy<S>(
        strategy: S,
        case: Case,
        workload: Workload,
        arguments: &Arguments,
        topology: &Topology,
        clock: TscClock,
    ) -> Result<ContenderTrial, RunCaseError>
    where
        S: WaitStrategy + Send + 'static,
    {
        match case.surface {
            Surface::Direct => run_direct(
                strategy,
                case.filtering,
                workload,
                arguments,
                topology,
                clock,
            )
            .map_err(RunCaseError::Failed),
            Surface::Parker => run_parker(
                strategy,
                case.filtering,
                workload,
                arguments,
                topology,
                clock,
            )
            .map_err(RunCaseError::Failed),
            Surface::StdPark => Err(RunCaseError::Failed(
                "library strategy cannot use the std-park surface".into(),
            )),
        }
    }

    #[cfg(feature = "benchmark-only")]
    fn run_c1(
        case: Case,
        workload: Workload,
        arguments: &Arguments,
        topology: &Topology,
        clock: TscClock,
    ) -> Result<ContenderTrial, RunCaseError> {
        let strategy = snoozer::benchmark::AmdMwaitxC1::new()
            .map_err(|error| RunCaseError::Unsupported(error.to_string()))?;
        run_strategy(strategy, case, workload, arguments, topology, clock)
    }

    #[cfg(not(feature = "benchmark-only"))]
    fn run_c1(
        _case: Case,
        _workload: Workload,
        _arguments: &Arguments,
        _topology: &Topology,
        _clock: TscClock,
    ) -> Result<ContenderTrial, RunCaseError> {
        Err(RunCaseError::Unsupported(
            "diagnostic C1 requires the benchmark-only feature".to_owned(),
        ))
    }

    fn run_direct<S>(
        strategy: S,
        filtering: Filtering,
        workload: Workload,
        arguments: &Arguments,
        topology: &Topology,
        clock: TscClock,
    ) -> AnyResult<ContenderTrial>
    where
        S: WaitStrategy + Send + 'static,
    {
        let shared = Arc::new(TrialShared::new());
        let waiter_shared = Arc::clone(&shared);
        let waiter_cpu = topology.waiter;
        let warmup_events = arguments.warmup_events;
        let max_samples = arguments.max_samples;
        let waiter = thread::spawn(move || -> AnyResult<WaiterMetrics> {
            if let Err(error) = pin_current(waiter_cpu) {
                waiter_shared.abort_startup();
                return Err(error.into());
            }
            let (_, expected_aux) = stamp();
            waiter_shared.thread_ready();
            let mut observed = waiter_shared.generation.0.load(Ordering::Acquire);
            let mut metrics = WaiterMetrics {
                latencies_cycles: Vec::with_capacity(max_samples.min(1_000_000)),
                unclassified_wakes: (filtering == Filtering::Raw).then_some(0),
                public_timeouts: 0,
                invalid_samples: 0,
                migrated_samples: 0,
                sample_limit_reached: false,
            };
            let mut events = 0_usize;
            loop {
                let changed = match filtering {
                    Filtering::Raw => loop {
                        match wait_direct_raw(&strategy, &waiter_shared.generation.0, observed) {
                            Observation::Changed(value) => break value,
                            Observation::Unclassified => {
                                if let Some(count) = &mut metrics.unclassified_wakes {
                                    *count = count.saturating_add(1);
                                }
                                let value = waiter_shared.generation.0.load(Ordering::Acquire);
                                if value != observed {
                                    break value;
                                }
                            }
                        }
                    },
                    Filtering::Filtered => {
                        wait_direct_filtered(&strategy, &waiter_shared.generation.0, observed)
                    }
                };
                observed = changed;
                if waiter_shared.stop.0.load(Ordering::Acquire) {
                    break;
                }
                record_sample(
                    &waiter_shared,
                    observed,
                    expected_aux,
                    warmup_events,
                    max_samples,
                    &mut events,
                    &mut metrics,
                );
            }
            Ok(metrics)
        });

        let producer = spawn_producer(Arc::clone(&shared), topology.producer, workload, || {});
        let victim = spawn_victim(Arc::clone(&shared), topology.victim, clock);
        let drive_result = drive_trial(&shared, arguments.trial_duration);
        let waiter_result = join_thread(waiter, "direct waiter");
        let producer_result = join_thread(producer, "producer");
        let victim_result = join_thread(victim, "victim");
        drive_result?;
        let waiter_metrics = waiter_result?;
        producer_result?;
        let victim_metrics = victim_result?;
        validate_sample_set(&waiter_metrics)?;
        Ok(ContenderTrial {
            waiter: waiter_metrics,
            victim: victim_metrics,
        })
    }

    fn run_parker<S>(
        strategy: S,
        filtering: Filtering,
        workload: Workload,
        arguments: &Arguments,
        topology: &Topology,
        clock: TscClock,
    ) -> AnyResult<ContenderTrial>
    where
        S: WaitStrategy + Send + 'static,
    {
        let (mut parker, unparker) = pair(strategy);
        let shared = Arc::new(TrialShared::new());
        let waiter_shared = Arc::clone(&shared);
        let waiter_cpu = topology.waiter;
        let warmup_events = arguments.warmup_events;
        let max_samples = arguments.max_samples;
        let waiter = thread::spawn(move || -> AnyResult<WaiterMetrics> {
            if let Err(error) = pin_current(waiter_cpu) {
                waiter_shared.abort_startup();
                return Err(error.into());
            }
            let (_, expected_aux) = stamp();
            waiter_shared.thread_ready();
            let mut observed = waiter_shared.generation.0.load(Ordering::Acquire);
            let mut metrics = WaiterMetrics {
                latencies_cycles: Vec::with_capacity(max_samples.min(1_000_000)),
                unclassified_wakes: (filtering == Filtering::Raw).then_some(0),
                public_timeouts: 0,
                invalid_samples: 0,
                migrated_samples: 0,
                sample_limit_reached: false,
            };
            let mut events = 0_usize;
            loop {
                while waiter_shared.generation.0.load(Ordering::Acquire) == observed {
                    match filtering {
                        Filtering::Raw => {
                            if !wait_parker_raw(&mut parker)
                                && let Some(count) = &mut metrics.unclassified_wakes
                            {
                                *count = count.saturating_add(1);
                            }
                        }
                        Filtering::Filtered => wait_parker_filtered(&mut parker),
                    }
                }
                observed = waiter_shared.generation.0.load(Ordering::Acquire);
                if waiter_shared.stop.0.load(Ordering::Acquire) {
                    break;
                }
                record_sample(
                    &waiter_shared,
                    observed,
                    expected_aux,
                    warmup_events,
                    max_samples,
                    &mut events,
                    &mut metrics,
                );
            }
            Ok(metrics)
        });

        let producer = spawn_producer(
            Arc::clone(&shared),
            topology.producer,
            workload,
            move || unparker.unpark(),
        );
        let victim = spawn_victim(Arc::clone(&shared), topology.victim, clock);
        let drive_result = drive_trial(&shared, arguments.trial_duration);
        let waiter_result = join_thread(waiter, "parker waiter");
        let producer_result = join_thread(producer, "producer");
        let victim_result = join_thread(victim, "victim");
        drive_result?;
        let waiter_metrics = waiter_result?;
        producer_result?;
        let victim_metrics = victim_result?;
        validate_sample_set(&waiter_metrics)?;
        Ok(ContenderTrial {
            waiter: waiter_metrics,
            victim: victim_metrics,
        })
    }

    fn run_std_park(
        case: Case,
        workload: Workload,
        arguments: &Arguments,
        topology: &Topology,
        clock: TscClock,
    ) -> AnyResult<ContenderTrial> {
        if case.surface != Surface::StdPark || case.filtering != Filtering::Raw {
            return Err("invalid std-park case".into());
        }
        let shared = Arc::new(TrialShared::new());
        let waiter_shared = Arc::clone(&shared);
        let waiter_cpu = topology.waiter;
        let warmup_events = arguments.warmup_events;
        let max_samples = arguments.max_samples;
        let (handle_sender, handle_receiver) = mpsc::sync_channel(1);
        let waiter = thread::spawn(move || -> AnyResult<WaiterMetrics> {
            if let Err(error) = pin_current(waiter_cpu) {
                waiter_shared.abort_startup();
                return Err(error.into());
            }
            let (_, expected_aux) = stamp();
            handle_sender.send(thread::current())?;
            waiter_shared.thread_ready();
            let mut observed = waiter_shared.generation.0.load(Ordering::Acquire);
            let mut metrics = WaiterMetrics {
                latencies_cycles: Vec::with_capacity(max_samples.min(1_000_000)),
                unclassified_wakes: Some(0),
                public_timeouts: 0,
                invalid_samples: 0,
                migrated_samples: 0,
                sample_limit_reached: false,
            };
            let mut events = 0_usize;
            loop {
                while waiter_shared.generation.0.load(Ordering::Acquire) == observed {
                    thread::park();
                    if waiter_shared.generation.0.load(Ordering::Acquire) == observed
                        && let Some(count) = &mut metrics.unclassified_wakes
                    {
                        *count = count.saturating_add(1);
                    }
                }
                observed = waiter_shared.generation.0.load(Ordering::Acquire);
                if waiter_shared.stop.0.load(Ordering::Acquire) {
                    break;
                }
                record_sample(
                    &waiter_shared,
                    observed,
                    expected_aux,
                    warmup_events,
                    max_samples,
                    &mut events,
                    &mut metrics,
                );
            }
            Ok(metrics)
        });
        let waiter_handle = handle_receiver.recv_timeout(STARTUP_TIMEOUT)?;
        let producer = spawn_producer(
            Arc::clone(&shared),
            topology.producer,
            workload,
            move || waiter_handle.unpark(),
        );
        let victim = spawn_victim(Arc::clone(&shared), topology.victim, clock);
        let drive_result = drive_trial(&shared, arguments.trial_duration);
        let waiter_result = join_thread(waiter, "std parker waiter");
        let producer_result = join_thread(producer, "producer");
        let victim_result = join_thread(victim, "victim");
        drive_result?;
        let waiter_metrics = waiter_result?;
        producer_result?;
        let victim_metrics = victim_result?;
        validate_sample_set(&waiter_metrics)?;
        Ok(ContenderTrial {
            waiter: waiter_metrics,
            victim: victim_metrics,
        })
    }

    fn record_sample(
        shared: &TrialShared,
        observed: u64,
        expected_aux: u32,
        warmup_events: usize,
        max_samples: usize,
        events: &mut usize,
        metrics: &mut WaiterMetrics,
    ) {
        let (received, auxiliary) = stamp();
        let sent = shared.sent_cycles.0.load(Ordering::Relaxed);
        if auxiliary != expected_aux {
            metrics.migrated_samples = metrics.migrated_samples.saturating_add(1);
        } else if let Ok(raw_cycles) = i64::try_from(i128::from(received) - i128::from(sent)) {
            if *events >= warmup_events && metrics.latencies_cycles.len() < max_samples {
                metrics.latencies_cycles.push(raw_cycles);
                if metrics.latencies_cycles.len() == max_samples {
                    metrics.sample_limit_reached = true;
                    shared.stop.0.store(true, Ordering::Release);
                }
            }
        } else {
            metrics.invalid_samples = metrics.invalid_samples.saturating_add(1);
        }
        *events = events.saturating_add(1);
        shared.acknowledged.0.store(observed, Ordering::Release);
    }

    fn spawn_producer<F>(
        shared: Arc<TrialShared>,
        cpu: usize,
        workload: Workload,
        wake: F,
    ) -> thread::JoinHandle<AnyResult<()>>
    where
        F: Fn() + Send + 'static,
    {
        thread::spawn(move || {
            if let Err(error) = pin_current(cpu) {
                shared.abort_startup();
                wake();
                return Err(error.into());
            }
            let (_, expected_aux) = stamp();
            shared.thread_ready();
            let mut sequence = 0_u64;
            let mut gaps = GapSchedule::new(BURSTY_SEED);
            while !shared.stop.0.load(Ordering::Acquire) {
                wait_for_ack_or_stop(&shared, sequence);
                if shared.stop.0.load(Ordering::Acquire) {
                    break;
                }
                if workload == Workload::Bursty {
                    cancellable_gap(gaps.next(), &shared.stop.0);
                    if shared.stop.0.load(Ordering::Acquire) {
                        break;
                    }
                }
                sequence = sequence.wrapping_add(1);
                let (sent, auxiliary) = stamp();
                if auxiliary != expected_aux {
                    shared.abort_startup();
                    wake();
                    return Err("producer migrated despite fixed affinity".into());
                }
                shared.sent_cycles.0.store(sent, Ordering::Relaxed);
                shared.generation.0.store(sequence, Ordering::Release);
                wake();
            }
            shared.stop.0.store(true, Ordering::Release);
            shared.generation.0.fetch_add(1, Ordering::Release);
            wake();
            Ok(())
        })
    }

    fn wait_for_ack_or_stop(shared: &TrialShared, expected: u64) {
        while shared.acknowledged.0.load(Ordering::Acquire) != expected
            && !shared.stop.0.load(Ordering::Acquire)
        {
            std::hint::spin_loop();
        }
    }

    fn drive_trial(shared: &TrialShared, duration: Duration) -> AnyResult<()> {
        let started = Instant::now();
        while shared.ready.0.load(Ordering::Acquire) != 3 {
            if shared.stop.0.load(Ordering::Acquire) {
                shared.go.0.store(true, Ordering::Release);
                return Err("a benchmark thread failed during startup".into());
            }
            if started.elapsed() >= STARTUP_TIMEOUT {
                shared.abort_startup();
                return Err("benchmark threads did not reach the start barrier".into());
            }
            std::hint::spin_loop();
        }
        shared.go.0.store(true, Ordering::Release);
        precise_delay(duration);
        shared.stop.0.store(true, Ordering::Release);
        Ok(())
    }

    fn cancellable_gap(duration: Duration, stop: &AtomicBool) {
        let target = Instant::now() + duration;
        if duration > Duration::from_micros(80) {
            thread::sleep(duration.saturating_sub(Duration::from_micros(50)));
        }
        while Instant::now() < target && !stop.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
    }

    fn precise_delay(duration: Duration) {
        let target = Instant::now() + duration;
        if duration > Duration::from_millis(2) {
            thread::sleep(duration - Duration::from_millis(1));
        }
        while Instant::now() < target {
            std::hint::spin_loop();
        }
    }

    fn spawn_victim(
        shared: Arc<TrialShared>,
        cpu: usize,
        clock: TscClock,
    ) -> thread::JoinHandle<AnyResult<VictimMetrics>> {
        thread::spawn(move || {
            if let Err(error) = pin_current(cpu) {
                shared.abort_startup();
                return Err(error.into());
            }
            let (_, expected_aux) = stamp();
            shared.thread_ready();
            victim_loop(&shared.stop.0, expected_aux, clock)
        })
    }

    fn victim_loop(
        stop: &AtomicBool,
        expected_aux: u32,
        clock: TscClock,
    ) -> AnyResult<VictimMetrics> {
        let started = Instant::now();
        let mut operations = 0_u64;
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut chunks = Vec::new();
        while !stop.load(Ordering::Acquire) {
            let (chunk_start, start_aux) = stamp();
            for _ in 0..VICTIM_CHUNK_OPERATIONS {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state = state.wrapping_mul(0xd6e8_feb8_6659_fd93);
            }
            let (chunk_end, end_aux) = stamp();
            if start_aux != expected_aux || end_aux != expected_aux || chunk_end < chunk_start {
                return Err("victim migrated or observed a non-monotonic TSC".into());
            }
            chunks.push(clock.cycles_to_ns(chunk_end - chunk_start));
            operations = operations.saturating_add(VICTIM_CHUNK_OPERATIONS);
        }
        black_box(state);
        let elapsed = started.elapsed();
        if chunks.is_empty() || elapsed.is_zero() {
            return Err("victim produced no measurable chunks".into());
        }
        Ok(VictimMetrics {
            operations,
            elapsed,
            p99_chunk_ns: percentile(&mut chunks, 0.99),
        })
    }

    fn run_victim_control(
        duration: Duration,
        victim_cpu: usize,
        clock: TscClock,
    ) -> AnyResult<VictimMetrics> {
        struct Control {
            ready: AtomicBool,
            go: AtomicBool,
            stop: AtomicBool,
        }
        let control = Arc::new(Control {
            ready: AtomicBool::new(false),
            go: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let worker_control = Arc::clone(&control);
        let worker = thread::spawn(move || -> AnyResult<VictimMetrics> {
            pin_current(victim_cpu)?;
            let (_, expected_aux) = stamp();
            worker_control.ready.store(true, Ordering::Release);
            while !worker_control.go.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            victim_loop(&worker_control.stop, expected_aux, clock)
        });
        let startup = Instant::now();
        let mut startup_failed = false;
        while !control.ready.load(Ordering::Acquire) {
            if worker.is_finished() {
                startup_failed = true;
                break;
            }
            if startup.elapsed() >= STARTUP_TIMEOUT {
                control.go.store(true, Ordering::Release);
                control.stop.store(true, Ordering::Release);
                startup_failed = true;
                break;
            }
            std::hint::spin_loop();
        }
        if !startup_failed {
            control.go.store(true, Ordering::Release);
            precise_delay(duration);
            control.stop.store(true, Ordering::Release);
        } else {
            control.go.store(true, Ordering::Release);
            control.stop.store(true, Ordering::Release);
        }
        let worker_result = join_thread(worker, "control victim");
        if startup_failed {
            return Err("control victim did not reach the start barrier".into());
        }
        worker_result
    }

    fn join_thread<T>(handle: thread::JoinHandle<AnyResult<T>>, role: &str) -> AnyResult<T> {
        match handle.join() {
            Ok(result) => result,
            Err(_) => Err(format!("{role} panicked").into()),
        }
    }

    fn validate_sample_set(metrics: &WaiterMetrics) -> AnyResult<()> {
        if metrics.sample_limit_reached {
            return Err("latency sample limit reached; partial distributions are not emitted; rerun with a larger --max-samples".into());
        }
        if metrics.latencies_cycles.is_empty() {
            return Err("timed trial produced no post-warmup latency samples".into());
        }
        Ok(())
    }

    #[derive(Clone, Debug)]
    struct RepetitionResult {
        repetition: usize,
        control_first: bool,
        samples: usize,
        p50_cycles: u64,
        p90_cycles: u64,
        p99_cycles: u64,
        p999_cycles: u64,
        max_cycles: u64,
        p50_ns: u64,
        p90_ns: u64,
        p99_ns: u64,
        p999_ns: u64,
        max_ns: u64,
        unclassified_wakes: Option<u64>,
        public_timeouts: u64,
        invalid_samples: u64,
        migrated_samples: u64,
        sample_limit_reached: bool,
        victim_control_mops: f64,
        victim_contender_mops: f64,
        victim_control_p99_ns: u64,
        victim_contender_p99_ns: u64,
        throughput_loss_percent: f64,
        victim_p99_degradation_percent: f64,
        raw_latency_cycles: Vec<u64>,
    }

    impl RepetitionResult {
        fn new(
            repetition: usize,
            control_first: bool,
            control: VictimMetrics,
            contender: ContenderTrial,
            clock: TscClock,
            correction_cycles: i64,
        ) -> AnyResult<Self> {
            let mut invalid_samples = contender.waiter.invalid_samples;
            let raw_latency_cycles = contender
                .waiter
                .latencies_cycles
                .into_iter()
                .filter_map(|raw_cycles| {
                    let corrected = correct_latency(raw_cycles, correction_cycles);
                    if corrected.is_none() {
                        invalid_samples = invalid_samples.saturating_add(1);
                    }
                    corrected
                })
                .collect::<Vec<_>>();
            if raw_latency_cycles.is_empty() {
                return Err("TSC correction invalidated every latency sample".into());
            }
            let mut latencies = raw_latency_cycles.clone();
            latencies.sort_unstable();
            let p50_cycles = percentile_sorted(&latencies, 0.50);
            let p90_cycles = percentile_sorted(&latencies, 0.90);
            let p99_cycles = percentile_sorted(&latencies, 0.99);
            let p999_cycles = percentile_sorted(&latencies, 0.999);
            let max_cycles = *latencies.last().ok_or("latency sample set is empty")?;
            let control_mops = control.throughput_mops();
            let contender_mops = contender.victim.throughput_mops();
            if !control_mops.is_finite() || control_mops <= 0.0 {
                return Err("invalid victim-only control throughput".into());
            }
            Ok(Self {
                repetition,
                control_first,
                samples: latencies.len(),
                p50_cycles,
                p90_cycles,
                p99_cycles,
                p999_cycles,
                max_cycles,
                p50_ns: clock.cycles_to_ns(p50_cycles),
                p90_ns: clock.cycles_to_ns(p90_cycles),
                p99_ns: clock.cycles_to_ns(p99_cycles),
                p999_ns: clock.cycles_to_ns(p999_cycles),
                max_ns: clock.cycles_to_ns(max_cycles),
                unclassified_wakes: contender.waiter.unclassified_wakes,
                public_timeouts: contender.waiter.public_timeouts,
                invalid_samples,
                migrated_samples: contender.waiter.migrated_samples,
                sample_limit_reached: contender.waiter.sample_limit_reached,
                victim_control_mops: control_mops,
                victim_contender_mops: contender_mops,
                victim_control_p99_ns: control.p99_chunk_ns,
                victim_contender_p99_ns: contender.victim.p99_chunk_ns,
                throughput_loss_percent: (control_mops - contender_mops) / control_mops * 100.0,
                victim_p99_degradation_percent: percent_change(
                    control.p99_chunk_ns,
                    contender.victim.p99_chunk_ns,
                )?,
                raw_latency_cycles,
            })
        }
    }

    fn percent_change(baseline: u64, measured: u64) -> AnyResult<f64> {
        if baseline == 0 {
            return Err("zero baseline cannot produce a percentage change".into());
        }
        Ok((measured as f64 - baseline as f64) / baseline as f64 * 100.0)
    }

    #[derive(Clone, Debug)]
    struct Summary {
        case: Case,
        workload: Workload,
        repetitions: usize,
        p50_ns: u64,
        p99_ns: u64,
        p999_ns: u64,
        throughput_loss_percent: f64,
        victim_p99_degradation_percent: f64,
        invalid_samples: u64,
        migrated_samples: u64,
        sample_limit_reached: bool,
        eligible: bool,
    }

    impl Summary {
        fn from_repetitions(
            case: Case,
            workload: Workload,
            repetitions: &[RepetitionResult],
        ) -> AnyResult<Self> {
            if repetitions.is_empty() {
                return Err("cannot summarize an empty repetition set".into());
            }
            let p50_ns = median_u64(repetitions.iter().map(|value| value.p50_ns));
            let p99_ns = median_u64(repetitions.iter().map(|value| value.p99_ns));
            let p999_ns = median_u64(repetitions.iter().map(|value| value.p999_ns));
            let throughput_loss_percent = median_f64(
                repetitions
                    .iter()
                    .map(|value| value.throughput_loss_percent),
            );
            let victim_p99_degradation_percent = median_f64(
                repetitions
                    .iter()
                    .map(|value| value.victim_p99_degradation_percent),
            );
            let invalid_samples = repetitions.iter().map(|value| value.invalid_samples).sum();
            let migrated_samples = repetitions.iter().map(|value| value.migrated_samples).sum();
            let sample_limit_reached = repetitions.iter().any(|value| value.sample_limit_reached);
            let eligible = throughput_loss_percent <= MAX_THROUGHPUT_LOSS_PERCENT
                && victim_p99_degradation_percent <= MAX_P99_DEGRADATION_PERCENT
                && invalid_samples == 0
                && migrated_samples == 0
                && !sample_limit_reached;
            Ok(Self {
                case,
                workload,
                repetitions: repetitions.len(),
                p50_ns,
                p99_ns,
                p999_ns,
                throughput_loss_percent,
                victim_p99_degradation_percent,
                invalid_samples,
                migrated_samples,
                sample_limit_reached,
                eligible,
            })
        }
    }

    fn choose_winner(summaries: &[Summary]) -> Option<&Summary> {
        summaries
            .iter()
            .filter(|summary| summary.eligible)
            .min_by(|left, right| {
                (left.p99_ns, left.p999_ns, left.p50_ns).cmp(&(
                    right.p99_ns,
                    right.p999_ns,
                    right.p50_ns,
                ))
            })
    }

    fn median_u64(values: impl Iterator<Item = u64>) -> u64 {
        let mut values: Vec<_> = values.collect();
        values.sort_unstable();
        values[values.len() / 2]
    }

    fn median_f64(values: impl Iterator<Item = f64>) -> f64 {
        let mut values: Vec<_> = values.collect();
        values.sort_by(f64::total_cmp);
        values[values.len() / 2]
    }

    fn percentile(values: &mut [u64], quantile: f64) -> u64 {
        values.sort_unstable();
        percentile_sorted(values, quantile)
    }

    struct JsonlOutput {
        writer: BufWriter<File>,
    }

    struct MetadataInput<'a> {
        arguments: &'a Arguments,
        topology: &'a Topology,
        cpuidle: &'a [CpuIdleState],
        clock: TscClock,
        skew: TscSkew,
        cpu: &'a crate::platform::CpuMetadata,
        power_policies: &'a [CpuPowerPolicy],
        provenance: &'a RepositoryProvenance,
        cstate_warning: &'a str,
    }

    impl JsonlOutput {
        fn create(path: &PathBuf) -> AnyResult<Self> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            Ok(Self {
                writer: BufWriter::new(File::create(path)?),
            })
        }

        fn metadata(&mut self, input: MetadataInput<'_>) -> AnyResult<()> {
            let states = input
                .cpuidle
                .iter()
                .map(|state| {
                    format!(
                        "{{\"cpu\":{},\"name\":\"{}\",\"disabled\":{}}}",
                        state.cpu,
                        json_escape(&state.name),
                        state.disabled
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            let topology_json = input
                .topology
                .cpus
                .values()
                .map(|cpu| {
                    let siblings = cpu
                        .siblings
                        .iter()
                        .map(usize::to_string)
                        .collect::<Vec<_>>()
                        .join(",");
                    format!(
                        "{{\"cpu\":{},\"package\":{},\"core\":{},\"siblings\":[{}]}}",
                        cpu.cpu, cpu.package, cpu.core, siblings
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            let power_policy_json = input
                .power_policies
                .iter()
                .map(|policy| {
                    format!(
                        "{{\"cpu\":{},\"governor\":\"{}\",\"energy_preference\":\"{}\"}}",
                        policy.cpu,
                        json_escape(&policy.governor),
                        json_escape(&policy.energy_preference)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            let compiled_dirty = input
                .provenance
                .compiled_dirty
                .map_or_else(|| "null".to_owned(), |value| value.to_string());
            let checkout_dirty = input
                .provenance
                .checkout_dirty
                .map_or_else(|| "null".to_owned(), |value| value.to_string());
            let mwaitx_timer_hz = snoozer::capabilities()
                .mwaitx_timer_hz
                .map_or_else(|| "null".to_owned(), |value| value.to_string());
            writeln!(
                self.writer,
                "{{\"type\":\"metadata\",\"schema\":\"{}\",\"mode\":\"{}\",\"warning\":\"{}\",\"benchmark_commit\":\"{}\",\"compiled_working_tree_dirty\":{},\"checkout_commit\":\"{}\",\"checkout_working_tree_dirty\":{},\"rustc\":\"{}\",\"rustup_toolchain\":\"{}\",\"benchmark_features\":{},\"schedule_version\":\"{}\",\"schedule_seed\":\"0x{:016x}\",\"bursty_gap_us_weight_percent\":[[0,30],[1,15],[5,12],[10,10],[25,10],[50,8],[100,7],[250,5],[1000,3]],\"duration_ms\":{},\"repetitions\":{},\"warmup_events\":{},\"max_samples\":{},\"acceptance_limits\":{{\"max_victim_throughput_loss_percent\":{:.6},\"max_victim_p99_degradation_percent\":{:.6}}},\"clocksource\":\"tsc\",\"cycles_per_ns\":{:.9},\"calibration_spread_percent\":{:.6},\"mwaitx_timer_hz\":{},\"tsc_skew\":{{\"producer_offset_cycles\":{},\"waiter_offset_cycles\":{},\"producer_uncertainty_cycles\":{},\"waiter_uncertainty_cycles\":{},\"producer_to_waiter_bound_ns\":{:.6},\"applied_waiter_minus_producer_cycles\":{}}},\"matched_control\":\"victim-only baseline; reported loss conservatively includes producer and waiter activity\",\"cpu_model\":\"{}\",\"microcode\":\"{}\",\"kernel\":\"{}\",\"power_policy\":[{}],\"roles\":{{\"waiter\":{},\"victim\":{},\"producer\":{},\"controller\":{}}},\"topology\":[{}],\"cpuidle\":[{}]}}",
                RESULT_SCHEMA_VERSION,
                input.arguments.mode.as_str(),
                json_escape(input.cstate_warning),
                json_escape(&input.provenance.compiled_commit),
                compiled_dirty,
                json_escape(&input.provenance.checkout_commit),
                checkout_dirty,
                json_escape(COMPILED_RUSTC.unwrap_or("unknown")),
                json_escape(COMPILED_RUSTUP_TOOLCHAIN.unwrap_or("unknown")),
                if cfg!(feature = "benchmark-only") {
                    "[\"benchmark-only\"]"
                } else {
                    "[]"
                },
                BURSTY_SCHEDULE_VERSION,
                BURSTY_SEED,
                input.arguments.trial_duration.as_millis(),
                input.arguments.repetitions,
                input.arguments.warmup_events,
                input.arguments.max_samples,
                MAX_THROUGHPUT_LOSS_PERCENT,
                MAX_P99_DEGRADATION_PERCENT,
                input.clock.cycles_per_ns,
                input.clock.spread_percent,
                mwaitx_timer_hz,
                input.skew.producer_offset_cycles,
                input.skew.waiter_offset_cycles,
                input.skew.producer_uncertainty_cycles,
                input.skew.waiter_uncertainty_cycles,
                input.skew.producer_to_waiter_bound_ns,
                input.skew.correction_cycles,
                json_escape(&input.cpu.model),
                json_escape(&input.cpu.microcode),
                json_escape(&input.cpu.kernel),
                power_policy_json,
                input.topology.waiter,
                input.topology.victim,
                input.topology.producer,
                input.topology.controller,
                topology_json,
                states
            )?;
            Ok(())
        }

        fn repetition(
            &mut self,
            case: Case,
            workload: Workload,
            value: &RepetitionResult,
        ) -> AnyResult<()> {
            let unclassified = value
                .unclassified_wakes
                .map_or_else(|| "null".to_owned(), |count| count.to_string());
            writeln!(
                self.writer,
                "{{\"type\":\"repetition\",\"case\":\"{}\",\"workload\":\"{}\",\"repetition\":{},\"control_first\":{},\"samples\":{},\"latency_cycles\":{{\"p50\":{},\"p90\":{},\"p99\":{},\"p999\":{},\"max\":{}}},\"latency_ns\":{{\"p50\":{},\"p90\":{},\"p99\":{},\"p999\":{},\"max\":{}}},\"unclassified_wakes\":{},\"public_timeouts\":{},\"invalid_samples\":{},\"migrated_samples\":{},\"sample_limit_reached\":{},\"victim\":{{\"control_mops\":{:.6},\"contender_mops\":{:.6},\"control_p99_chunk_ns\":{},\"contender_p99_chunk_ns\":{},\"throughput_loss_percent\":{:.6},\"p99_degradation_percent\":{:.6}}}}}",
                json_escape(&case.name()),
                workload.as_str(),
                value.repetition,
                value.control_first,
                value.samples,
                value.p50_cycles,
                value.p90_cycles,
                value.p99_cycles,
                value.p999_cycles,
                value.max_cycles,
                value.p50_ns,
                value.p90_ns,
                value.p99_ns,
                value.p999_ns,
                value.max_ns,
                unclassified,
                value.public_timeouts,
                value.invalid_samples,
                value.migrated_samples,
                value.sample_limit_reached,
                value.victim_control_mops,
                value.victim_contender_mops,
                value.victim_control_p99_ns,
                value.victim_contender_p99_ns,
                value.throughput_loss_percent,
                value.victim_p99_degradation_percent
            )?;
            write!(
                self.writer,
                "{{\"type\":\"latency_samples\",\"case\":\"{}\",\"workload\":\"{}\",\"repetition\":{},\"unit\":\"TSC cycles\",\"tsc_offset_correction\":\"applied\",\"order\":\"observation\",\"values\":[",
                json_escape(&case.name()),
                workload.as_str(),
                value.repetition
            )?;
            for (index, cycles) in value.raw_latency_cycles.iter().enumerate() {
                if index != 0 {
                    self.writer.write_all(b",")?;
                }
                write!(self.writer, "{cycles}")?;
            }
            writeln!(self.writer, "]}}")?;
            Ok(())
        }

        fn summary(&mut self, summary: &Summary) -> AnyResult<()> {
            writeln!(
                self.writer,
                "{{\"type\":\"summary\",\"case\":\"{}\",\"workload\":\"{}\",\"repetitions\":{},\"median_p50_ns\":{},\"median_p99_ns\":{},\"median_p999_ns\":{},\"median_victim_throughput_loss_percent\":{:.6},\"median_victim_p99_degradation_percent\":{:.6},\"invalid_samples\":{},\"migrated_samples\":{},\"sample_limit_reached\":{},\"eligible\":{}}}",
                json_escape(&summary.case.name()),
                summary.workload.as_str(),
                summary.repetitions,
                summary.p50_ns,
                summary.p99_ns,
                summary.p999_ns,
                summary.throughput_loss_percent,
                summary.victim_p99_degradation_percent,
                summary.invalid_samples,
                summary.migrated_samples,
                summary.sample_limit_reached,
                summary.eligible
            )?;
            Ok(())
        }

        fn skipped(&mut self, case: Case, workload: Workload, reason: &str) -> AnyResult<()> {
            writeln!(
                self.writer,
                "{{\"type\":\"skip\",\"case\":\"{}\",\"workload\":\"{}\",\"reason\":\"{}\"}}",
                json_escape(&case.name()),
                workload.as_str(),
                json_escape(reason)
            )?;
            Ok(())
        }

        fn winner(&mut self, winner: &Summary) -> AnyResult<()> {
            writeln!(
                self.writer,
                "{{\"type\":\"winner\",\"workload\":\"{}\",\"case\":\"{}\",\"median_p99_ns\":{},\"median_p999_ns\":{},\"median_p50_ns\":{}}}",
                winner.workload.as_str(),
                json_escape(&winner.case.name()),
                winner.p99_ns,
                winner.p999_ns,
                winner.p50_ns
            )?;
            Ok(())
        }

        fn no_winner(&mut self, workload: Workload) -> AnyResult<()> {
            writeln!(
                self.writer,
                "{{\"type\":\"winner\",\"workload\":\"{}\",\"case\":null}}",
                workload.as_str()
            )?;
            Ok(())
        }

        fn flush(&mut self) -> AnyResult<()> {
            self.writer.flush()?;
            Ok(())
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() {
    if let Err(error) = linux::main() {
        eprintln!("wake_latency failed: {error}");
        std::process::exit(1);
    }
}
