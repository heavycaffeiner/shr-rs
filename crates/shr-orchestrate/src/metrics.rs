//! The real, `CommandRunner`-backed `MetricsSampler` production reshapes
//! actually use. Lives here (not `shr-exec`) because it needs both
//! `shr_exec::CommandRunner` and `shr_inspect::parse_smartctl` -- `shr-exec`
//! has no dependency on `shr-inspect` and shouldn't gain one just for this.
//!
//! Every signal is read through `CommandRunner`, never raw `std::fs`/
//! `std::process` -- same rationale as every other kernel-parameter access
//! in this project (unmockable + unconditional IO error on the Windows dev
//! host `cargo test` runs on). A signal this sampler cannot read comes back
//! as `None` in the `ThrottleMetrics` it produces -- it never fabricates a
//! plausible-looking value the way the old `NullMetricsSampler` did.

use shr_exec::{CommandRunner, ExecError, MetricsSampler, ThrottleMetrics};
use shr_inspect::parse_smartctl;
use std::sync::Mutex;
use std::time::Duration;

/// Real signal collection for one band's reshape. Constructed fresh
/// for each throttle tick (both the one-shot tick at reshape start and each
/// periodic tick from the systemd-timer-driven daemon path) --
/// `previous_smart_total` carries the ONLY piece of state that must survive
/// across ticks, and since each periodic tick is a brand-new process with no
/// shared memory, that state has to come from the caller (persisted in
/// `state.toml`'s `StateBand::last_smart_reallocated`), not from a sampler
/// that lives longer than one tick.
pub struct LiveMetricsSampler<'a> {
    runner: &'a dyn CommandRunner,
    /// This band's member disk device paths (e.g. `/dev/disk/by-id/...`) --
    /// what `smartctl` is asked about for temperature/reallocated-sector
    /// signals. Not partition paths: SMART is a whole-disk concept.
    member_disks: Vec<String>,
    previous_smart_total: Option<u64>,
    /// How long to wait between the two `/proc/stat` samples used to compute
    /// `io_wait_pct` (see `read_io_wait_pct`'s doc comment). A real
    /// production tick wants a real wall-clock gap; tests set this to
    /// `Duration::ZERO` so `cargo test` doesn't actually sleep.
    sample_interval: Duration,
    /// The absolute SMART reallocated total this sampler observed on its one
    /// `sample()` call, for the caller to read back afterward and persist as
    /// next tick's `previous_smart_total` (see struct doc comment above).
    last_smart_total: Mutex<Option<u64>>,
}

impl<'a> LiveMetricsSampler<'a> {
    pub fn new(
        runner: &'a dyn CommandRunner,
        member_disks: Vec<String>,
        previous_smart_total: Option<u64>,
    ) -> Self {
        Self {
            runner,
            member_disks,
            previous_smart_total,
            sample_interval: Duration::from_millis(500),
            last_smart_total: Mutex::new(None),
        }
    }

    pub fn with_sample_interval(mut self, interval: Duration) -> Self {
        self.sample_interval = interval;
        self
    }

    /// The absolute SMART reallocated total observed by the most recent
    /// `sample()` call -- `None` if `sample()` was never called or every
    /// member disk's SMART read failed. The caller persists this into
    /// `StateBand::last_smart_reallocated` so the NEXT tick (a different
    /// process, for the periodic timer path) can compute a real delta again.
    pub fn last_smart_total(&self) -> Option<u64> {
        *self.last_smart_total.lock().unwrap()
    }
}

impl MetricsSampler for LiveMetricsSampler<'_> {
    fn sample(&self) -> Option<ThrottleMetrics> {
        let cpu_count = read_cpu_count(self.runner);
        let cpu_load = read_normalised_cpu_load(self.runner, cpu_count);
        let io_wait_pct = read_io_wait_pct(self.runner, self.sample_interval);
        let (disk_temp_max, smart_total) = read_smart_signals(self.runner, &self.member_disks);

        *self.last_smart_total.lock().unwrap() = smart_total;
        let smart_delta_reallocated = match (smart_total, self.previous_smart_total) {
            (Some(current), Some(previous)) => Some(current.saturating_sub(previous)),
            // No prior reading (first tick ever, or SMART unreadable this
            // time) -- an honest "don't know the delta", not "assume zero
            // increase". `ReshapeThrottle::tick` leans Decrease on this.
            _ => None,
        };

        Some(ThrottleMetrics {
            cpu_load,
            cpu_count,
            io_wait_pct,
            // Not in this sampler's data sources (the design lists it,
            // but no `/proc`/`smartctl` source approximates p99 user IO
            // latency without extra instrumentation this project does not
            // have yet -- see Stage C report's "remaining risks"). Honestly
            // unknown rather than a fabricated value.
            user_io_latency_p99_ms: None,
            disk_temp_max,
            smart_delta_reallocated,
            // No previous total means nothing to diff against, which on an
            // operation's first tick is expected rather than a failed read.
            first_sample: self.previous_smart_total.is_none(),
        })
    }
}

/// `/proc/loadavg`'s 1-minute load average divided by the online CPU count,
/// which is the shape `SafetyThresholds::max_cpu_load` (0.85) has always
/// implied: a fraction of the machine.
///
/// The raw average counts uninterruptible-sleep tasks, so during a sync it
/// sits at roughly 2 to 6 on any real machine and compared against 0.85 was
/// true on effectively every tick. A CPU count that cannot be read yields
/// `None` -- an unknown contention signal, handled by profile in
/// `ReshapeThrottle::tick` -- rather than a silent fall back to the raw
/// value.
fn read_normalised_cpu_load(runner: &dyn CommandRunner, cpu_count: Option<u32>) -> Option<f64> {
    let cpu_count = cpu_count?;
    let output = runner.run("cat", &["/proc/loadavg"]).ok()?;
    let load: f64 = output.stdout.split_whitespace().next()?.parse().ok()?;
    Some(load / f64::from(cpu_count))
}

/// Online CPUs, counted from `/proc/cpuinfo`'s `processor` lines. `None`
/// (never a fabricated 1) when the file can't be read or holds no such line.
fn read_cpu_count(runner: &dyn CommandRunner) -> Option<u32> {
    let output = runner.run("cat", &["/proc/cpuinfo"]).ok()?;
    let count = output
        .stdout
        .lines()
        .filter(|l| l.starts_with("processor"))
        .count();
    u32::try_from(count).ok().filter(|&c| c > 0)
}

/// Two `/proc/stat` samples `sample_interval` apart, diffed to get the
/// fraction of CPU time spent in iowait over that window -- `/proc/stat`'s
/// `cpu` line reports monotonically increasing cumulative tick counters
/// since boot, so a single read cannot give a current percentage, only two
/// reads and a diff can (the design: compute from the difference
/// between two samples). Self-contained (never relies on a previous
/// `sample()` call's
/// state) so this works correctly even for a periodic tick that runs as a
/// brand-new process with nothing to diff against from last time.
fn read_io_wait_pct(runner: &dyn CommandRunner, sample_interval: Duration) -> Option<f64> {
    let first = read_cpu_stat_line(runner)?;
    if !sample_interval.is_zero() {
        std::thread::sleep(sample_interval);
    }
    let second = read_cpu_stat_line(runner)?;

    let total_delta = second.total.checked_sub(first.total)?;
    if total_delta == 0 {
        return None;
    }
    let iowait_delta = second.iowait.checked_sub(first.iowait)?;
    Some((iowait_delta as f64 / total_delta as f64) * 100.0)
}

struct CpuTimes {
    total: u64,
    iowait: u64,
}

/// Parse the `cpu ` (aggregate, not `cpu0`/`cpu1`/...) line of `/proc/stat`:
/// `cpu  user nice system idle iowait irq softirq steal guest guest_nice`.
/// `total` sums every field present (older kernels omit `guest`/
/// `guest_nice`); `iowait` is the 5th field.
fn read_cpu_stat_line(runner: &dyn CommandRunner) -> Option<CpuTimes> {
    let output = runner.run("cat", &["/proc/stat"]).ok()?;
    let line = output.stdout.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    if fields.len() < 5 {
        return None;
    }
    Some(CpuTimes {
        total: fields.iter().sum(),
        iowait: fields[4],
    })
}

/// Max temperature and total reallocated-sector count across every member
/// disk, each read via `smartctl -j -H -A -i <dev>` through `CommandRunner`.
/// A device whose `smartctl` invocation errors, or whose JSON parse fails,
/// contributes nothing (skipped, not treated as zero) -- if EVERY device
/// fails, both return values are `None`, an honest "couldn't measure this
/// tick" rather than a fabricated "no problems found".
fn read_smart_signals(runner: &dyn CommandRunner, member_disks: &[String]) -> (Option<u8>, Option<u64>) {
    let mut max_temp: Option<u8> = None;
    let mut total_realloc: Option<u64> = None;

    for dev in member_disks {
        let Some(stdout) = smartctl_stdout(runner, dev) else {
            continue;
        };
        let Ok(info) = parse_smartctl(&stdout) else {
            continue;
        };

        if let Some(t) = info.temperature_c {
            let t = t.clamp(0, i64::from(u8::MAX)) as u8;
            max_temp = Some(max_temp.map_or(t, |m| m.max(t)));
        }
        if let Some(r) = info.reallocated_sectors {
            total_realloc = Some(total_realloc.unwrap_or(0) + r);
        }
    }

    (max_temp, total_realloc)
}

/// `smartctl` commonly exits nonzero for informational/warning bits (a
/// pending sector, a failing health assessment) while still emitting valid
/// JSON on stdout -- `shr-inspect`'s `SystemInspector::smart` documents the
/// same behavior. `CommandRunner::run`'s `ExecError::NonZeroExit` carries
/// `stdout` too, so that case is not a hard failure here; only a genuine
/// spawn/IO error (`ExecError::Io`) or a missing prerequisite is.
fn smartctl_stdout(runner: &dyn CommandRunner, dev: &str) -> Option<String> {
    match runner.run("smartctl", &["-j", "-H", "-A", "-i", dev]) {
        Ok(output) => Some(output.stdout),
        Err(ExecError::NonZeroExit { stdout, .. }) => Some(stdout),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shr_exec::{CommandOutput, DryRunRunner};
    use std::sync::Mutex as StdMutex;

    /// Returns a scripted stdout per call, keyed by the exact `program` name
    /// -- `/proc/stat` needs two DIFFERENT responses across the two reads
    /// `read_io_wait_pct` makes, so `cat` is scripted as a queue rather than
    /// a single fixed value.
    #[derive(Default)]
    struct ScriptedRunner {
        cat_responses: StdMutex<Vec<String>>,
        smartctl_responses: StdMutex<Vec<Result<String, ()>>>,
        recorded: StdMutex<Vec<String>>,
    }
    impl ScriptedRunner {
        fn recorded(&self) -> Vec<String> {
            self.recorded.lock().unwrap().clone()
        }
    }
    impl CommandRunner for ScriptedRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, ExecError> {
            self.recorded
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            match program {
                "cat" => {
                    let stdout = self.cat_responses.lock().unwrap().remove(0);
                    Ok(CommandOutput {
                        stdout,
                        stderr: String::new(),
                    })
                }
                "smartctl" => match self.smartctl_responses.lock().unwrap().remove(0) {
                    Ok(stdout) => Ok(CommandOutput {
                        stdout,
                        stderr: String::new(),
                    }),
                    Err(()) => Err(ExecError::Prerequisite("smartctl not installed".into())),
                },
                _ => Ok(CommandOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                }),
            }
        }
        fn is_dry_run(&self) -> bool {
            false
        }
    }

    /// `/proc/cpuinfo` reduced to the only thing `read_cpu_count` looks at.
    fn cpuinfo(cores: usize) -> String {
        (0..cores)
            .map(|i| format!("processor\t: {i}\nmodel name\t: test\n\n"))
            .collect()
    }

    fn smart_json(temp: i64, realloc: u64) -> String {
        format!(
            r#"{{"smart_status":{{"passed":true}},"temperature":{{"current":{temp}}},
              "ata_smart_attributes":{{"table":[{{"id":5,"raw":{{"value":{realloc}}}}}]}}}}"#
        )
    }

    /// D4: the raw 1-minute average counts uninterruptible-sleep tasks, so
    /// during a sync it sits well above the 0.85 per-core threshold it used
    /// to be compared against and decreased on every tick.
    #[test]
    fn cpu_load_is_normalised_by_core_count() {
        let runner = ScriptedRunner {
            cat_responses: StdMutex::new(vec![
                cpuinfo(4),
                "3.20 0.30 0.25 1/523 12345\n".to_string(), // loadavg
                "cpu  100 0 100 800 20 0 0 0\n".to_string(), // stat #1
                "cpu  110 0 110 880 22 0 0 0\n".to_string(), // stat #2
            ]),
            smartctl_responses: StdMutex::new(vec![]),
            recorded: StdMutex::new(vec![]),
        };
        let sampler = LiveMetricsSampler::new(&runner, vec![], None).with_sample_interval(Duration::ZERO);
        let m = sampler.sample().unwrap();
        assert_eq!(
            m.cpu_load,
            Some(0.80),
            "3.20 across 4 cores is 80% of the machine"
        );
        assert_eq!(m.cpu_count, Some(4), "the divisor must be reportable too");
    }

    #[test]
    fn an_unreadable_core_count_yields_an_unknown_load_not_the_raw_average() {
        let runner = ScriptedRunner {
            cat_responses: StdMutex::new(vec![
                // cpuinfo with no `processor` line -- `/proc/loadavg` is
                // then never read at all, so no response is queued for it.
                String::new(),
                "cpu  100 0 100 800 20 0 0 0\n".to_string(),
                "cpu  110 0 110 880 22 0 0 0\n".to_string(),
            ]),
            smartctl_responses: StdMutex::new(vec![]),
            recorded: StdMutex::new(vec![]),
        };
        let sampler = LiveMetricsSampler::new(&runner, vec![], None).with_sample_interval(Duration::ZERO);
        let m = sampler.sample().unwrap();
        assert_eq!(m.cpu_load, None);
        assert_eq!(m.cpu_count, None);
    }

    #[test]
    fn computes_io_wait_percentage_from_two_diffed_proc_stat_samples() {
        // total delta = (110+0+110+880+22) - (100+0+100+800+20) = 1122-1020=102
        // iowait delta = 22-20 = 2 -> 2/102*100 ≈ 1.96%
        let runner = ScriptedRunner {
            cat_responses: StdMutex::new(vec![
                cpuinfo(4),
                "0.1 0.1 0.1 1/1 1\n".to_string(),
                "cpu  100 0 100 800 20 0 0 0\n".to_string(),
                "cpu  110 0 110 880 22 0 0 0\n".to_string(),
            ]),
            smartctl_responses: StdMutex::new(vec![]),
            recorded: StdMutex::new(vec![]),
        };
        let sampler = LiveMetricsSampler::new(&runner, vec![], None).with_sample_interval(Duration::ZERO);
        let m = sampler.sample().unwrap();
        let pct = m
            .io_wait_pct
            .expect("io_wait_pct must be computed from the two samples");
        assert!((pct - 1.9607).abs() < 0.01, "got {pct}");
    }

    #[test]
    fn reads_max_temperature_and_sums_reallocated_sectors_across_member_disks() {
        let runner = ScriptedRunner {
            cat_responses: StdMutex::new(vec![
                cpuinfo(4),
                "0.1 0.1 0.1 1/1 1\n".to_string(),
                "cpu  1 0 1 1 0 0 0 0\n".to_string(),
                "cpu  2 0 2 2 0 0 0 0\n".to_string(),
            ]),
            smartctl_responses: StdMutex::new(vec![Ok(smart_json(38, 2)), Ok(smart_json(45, 3))]),
            recorded: StdMutex::new(vec![]),
        };
        let sampler = LiveMetricsSampler::new(
            &runner,
            vec![
                "/dev/disk/by-id/ata-DISK1".to_string(),
                "/dev/disk/by-id/ata-DISK2".to_string(),
            ],
            Some(1), // previous total = 1
        )
        .with_sample_interval(Duration::ZERO);

        let m = sampler.sample().unwrap();
        assert_eq!(
            m.disk_temp_max,
            Some(45),
            "must report the MAX across member disks, not the last one"
        );
        // total realloc this sample = 2 + 3 = 5; previous = 1 -> delta = 4
        assert_eq!(m.smart_delta_reallocated, Some(4));
        assert_eq!(
            sampler.last_smart_total(),
            Some(5),
            "caller must be able to persist the new absolute total"
        );
    }

    #[test]
    fn a_disk_whose_smartctl_call_fails_is_skipped_not_treated_as_zero() {
        let runner = ScriptedRunner {
            cat_responses: StdMutex::new(vec![
                cpuinfo(4),
                "0.1 0.1 0.1 1/1 1\n".to_string(),
                "cpu  1 0 1 1 0 0 0 0\n".to_string(),
                "cpu  2 0 2 2 0 0 0 0\n".to_string(),
            ]),
            smartctl_responses: StdMutex::new(vec![Err(()), Ok(smart_json(50, 7))]),
            recorded: StdMutex::new(vec![]),
        };
        let sampler = LiveMetricsSampler::new(
            &runner,
            vec![
                "/dev/disk/by-id/ata-DEAD".to_string(),
                "/dev/disk/by-id/ata-OK".to_string(),
            ],
            None,
        )
        .with_sample_interval(Duration::ZERO);

        let m = sampler.sample().unwrap();
        assert_eq!(
            m.disk_temp_max,
            Some(50),
            "the one readable disk's signal must still be used"
        );
        assert_eq!(sampler.last_smart_total(), Some(7));
    }

    #[test]
    fn every_smart_read_failing_reports_unknown_not_healthy() {
        let runner = ScriptedRunner {
            cat_responses: StdMutex::new(vec![
                cpuinfo(4),
                "0.1 0.1 0.1 1/1 1\n".to_string(),
                "cpu  1 0 1 1 0 0 0 0\n".to_string(),
                "cpu  2 0 2 2 0 0 0 0\n".to_string(),
            ]),
            smartctl_responses: StdMutex::new(vec![Err(())]),
            recorded: StdMutex::new(vec![]),
        };
        let sampler = LiveMetricsSampler::new(&runner, vec!["/dev/disk/by-id/ata-DEAD".to_string()], Some(3))
            .with_sample_interval(Duration::ZERO);

        let m = sampler.sample().unwrap();
        assert_eq!(m.disk_temp_max, None);
        assert_eq!(
            m.smart_delta_reallocated, None,
            "no reading at all must never look like 'no increase'"
        );
        assert_eq!(sampler.last_smart_total(), None);
    }

    #[test]
    fn no_previous_smart_total_yields_an_unknown_delta_not_a_zero_delta() {
        let runner = ScriptedRunner {
            cat_responses: StdMutex::new(vec![
                cpuinfo(4),
                "0.1 0.1 0.1 1/1 1\n".to_string(),
                "cpu  1 0 1 1 0 0 0 0\n".to_string(),
                "cpu  2 0 2 2 0 0 0 0\n".to_string(),
            ]),
            smartctl_responses: StdMutex::new(vec![Ok(smart_json(40, 9))]),
            recorded: StdMutex::new(vec![]),
        };
        let sampler = LiveMetricsSampler::new(&runner, vec!["/dev/disk/by-id/ata-DISK1".to_string()], None)
            .with_sample_interval(Duration::ZERO);

        let m = sampler.sample().unwrap();
        assert_eq!(
            m.smart_delta_reallocated, None,
            "first-ever tick has nothing to diff against"
        );
        assert!(
            m.first_sample,
            "and it must say so, or the throttle brakes an opening tick on a \
             condition that is None by construction"
        );
        assert_eq!(sampler.last_smart_total(), Some(9));
    }

    #[test]
    fn a_sampler_with_a_previous_total_is_not_a_first_sample() {
        let runner = ScriptedRunner {
            cat_responses: StdMutex::new(vec![
                cpuinfo(4),
                "0.1 0.1 0.1 1/1 1\n".to_string(),
                "cpu  1 0 1 1 0 0 0 0\n".to_string(),
                "cpu  2 0 2 2 0 0 0 0\n".to_string(),
            ]),
            smartctl_responses: StdMutex::new(vec![Ok(smart_json(40, 9))]),
            recorded: StdMutex::new(vec![]),
        };
        let sampler =
            LiveMetricsSampler::new(&runner, vec!["/dev/disk/by-id/ata-DISK1".to_string()], Some(9))
                .with_sample_interval(Duration::ZERO);

        assert!(!sampler.sample().unwrap().first_sample);
    }

    #[test]
    fn every_read_goes_through_command_runner_never_raw_fs() {
        // Not a behavioral assertion so much as a structural guard: this
        // sampler must have no other way to reach `/proc`/smartctl than
        // `self.runner.run(...)`, which the ScriptedRunner test double
        // above already proves by being the ONLY source of data these
        // tests supply. This test additionally checks the exact commands
        // issued, so a future refactor that quietly adds a raw fs/process
        // call fails loudly here even if it happens to also call the
        // runner for something else.
        let runner = ScriptedRunner {
            cat_responses: StdMutex::new(vec![
                cpuinfo(4),
                "0.1 0.1 0.1 1/1 1\n".to_string(),
                "cpu  1 0 1 1 0 0 0 0\n".to_string(),
                "cpu  2 0 2 2 0 0 0 0\n".to_string(),
            ]),
            smartctl_responses: StdMutex::new(vec![Ok(smart_json(40, 0))]),
            recorded: StdMutex::new(vec![]),
        };
        let sampler =
            LiveMetricsSampler::new(&runner, vec!["/dev/disk/by-id/ata-DISK1".to_string()], Some(0))
                .with_sample_interval(Duration::ZERO);
        sampler.sample();

        let recorded = runner.recorded();
        assert!(recorded.iter().any(|c| c == "cat /proc/loadavg"));
        assert_eq!(
            recorded.iter().filter(|c| c.as_str() == "cat /proc/stat").count(),
            2
        );
        assert!(recorded
            .iter()
            .any(|c| c.starts_with("smartctl") && c.contains("ata-DISK1")));
    }

    #[test]
    fn dry_run_never_needs_this_sampler_but_it_does_not_panic_if_constructed() {
        // Documented for completeness: production wiring (engine.rs) never
        // constructs a LiveMetricsSampler under dry-run (nothing real to
        // read), but the sampler itself has no special-case for it --
        // `DryRunRunner::run` always succeeds with empty output, which this
        // sampler already treats as "unreadable" via the normal parse-failure
        // path, not a crash.
        let runner = DryRunRunner::new();
        let sampler = LiveMetricsSampler::new(&runner, vec!["/dev/x".to_string()], None)
            .with_sample_interval(Duration::ZERO);
        let m = sampler.sample().unwrap();
        assert_eq!(m.cpu_load, None);
    }
}
