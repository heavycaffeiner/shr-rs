//! Reshape adaptive speed control (the design). Two layers:
//!
//! - Pure algorithm (`SafetyThresholds`, `ThrottleMetrics`,
//!   `ReshapeThrottle::tick`) -- no I/O, unit-testable from injected metrics
//!   alone.
//! - `ThrottleController` -- applies a `ThrottleDecision` to the real kernel
//!   parameters via `CommandRunner`. Never raw `std::fs`: these are
//!   `/proc`/`/sys` paths, unmockable and always an IO error on the Windows
//!   dev host `cargo test` runs on natively (same rationale as
//!   `MdadmExecutor::degraded_count`'s doc comment).

use crate::cmd::{write_sysfs, CommandRunner, ExecError};

/// Initial value and hard ceiling/floor -- `ThrottleController::apply`
/// never proposes a speed outside `[RESHAPE_SPEED_FLOOR_KB,
/// RESHAPE_SPEED_CEILING_KB]` (a priority profile's own ceiling, if lower,
/// is enforced separately -- see `ReshapePriority::ceiling_kb`).
pub const RESHAPE_SPEED_INITIAL_KB: u64 = 100_000;
pub const RESHAPE_SPEED_CEILING_KB: u64 = 500_000;
pub const RESHAPE_SPEED_FLOOR_KB: u64 = 10_000;
pub const STRIPE_CACHE_DEFAULT: u32 = 4096;
/// `/proc/sys/dev/raid/speed_limit_min` default (the design) -- kept
/// separate from `RESHAPE_SPEED_FLOOR_KB` (the throttle's own floor for
/// `speed_limit_max`): the kernel treats `speed_limit_min` as "never resync
/// slower than this even under contention", not a throttle target.
pub(crate) const SPEED_LIMIT_MIN_DEFAULT_KB: u64 = 1_000;

/// The design, verbatim -- the safety limits `ReshapeThrottle::tick` checks every
/// sample against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SafetyThresholds {
    pub max_disk_temp_c: u8,
    pub max_cpu_load: f64,
    pub max_io_wait_pct: f64,
    pub max_smart_reallocated: u64,
    pub user_io_latency_ms: u64,
}

impl Default for SafetyThresholds {
    fn default() -> Self {
        Self {
            max_disk_temp_c: 50,
            max_cpu_load: 0.85,
            max_io_wait_pct: 30.0,
            max_smart_reallocated: 0,
            user_io_latency_ms: 100,
        }
    }
}

/// One sample of the signals `ReshapeThrottle::tick` decides on. Every field
/// is `Option`: a real sampler can fail to read any one signal
/// independently of the others (smartctl not installed, a transient `/proc`
/// read error, no prior sample yet to diff against), and `tick()` must never
/// treat a missing signal as "known good" (see `tick`'s doc comment).
/// `smart_delta_reallocated` is the increase in reallocated-sector count
/// since the previous sample (not the raw counter) -- the design's rule is
/// "increase triggers emergency brake", and a nonzero *absolute* count from
/// a drive with pre-existing reallocations would otherwise emergency-brake
/// every tick forever.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ThrottleMetrics {
    pub cpu_load: Option<f64>,
    pub io_wait_pct: Option<f64>,
    pub user_io_latency_p99_ms: Option<u64>,
    pub disk_temp_max: Option<u8>,
    pub smart_delta_reallocated: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ThrottleDecision {
    EmergencyBrake,
    Decrease(f64),
    Increase(f64),
    Hold,
}

/// Supplies one `ThrottleMetrics` sample per `tick()`. Implemented by a
/// live, `CommandRunner`/`Inspector`-backed sampler in `shr-orchestrate`
/// (which has access to both); test doubles here just return a fixed value.
///
/// `None` means "not monitoring at all" (see `NullMetricsSampler`) -- a
/// distinct, honest declaration from a `Some(ThrottleMetrics)` whose
/// individual fields are themselves `None` (a live sampler that tried to
/// read a signal and couldn't). `tick()` treats the two differently: no
/// monitoring configured holds the current speed (nothing to react to);
/// a live sampler that's failing to read a safety-critical signal leans
/// toward decelerating, per the project's rule: when you don't know, take
/// the safe side.
pub trait MetricsSampler {
    fn sample(&self) -> Option<ThrottleMetrics>;
}

/// The pure algorithm. Holds no CommandRunner and does no I/O itself --
/// `tick()` only asks its `MetricsSampler` for one sample and returns a
/// decision; applying that decision to real kernel parameters is
/// `ThrottleController`'s job.
pub struct ReshapeThrottle<'a> {
    thresholds: SafetyThresholds,
    sampler: &'a dyn MetricsSampler,
}

impl<'a> ReshapeThrottle<'a> {
    pub fn new(thresholds: SafetyThresholds, sampler: &'a dyn MetricsSampler) -> Self {
        Self { thresholds, sampler }
    }

    /// The design, verbatim, extended by the "unknown never means safe" rule.
    /// Checked in priority order -- emergency brake beats decrease beats
    /// increase beats hold -- so a sample that satisfies both a danger
    /// condition and a "system is idle" condition always brakes, never
    /// accelerates.
    ///
    /// `self.sampler.sample() == None` means no monitoring is configured at
    /// all (`NullMetricsSampler`) -- holds the current speed, since there is
    /// no signal, dangerous or otherwise, to react to (this is the
    /// pre-throttle behavior every caller that doesn't opt into live
    /// monitoring must keep seeing).
    ///
    /// A `Some(metrics)` whose individual fields are `None` is different: a
    /// live sampler tried to measure and could not. Any missing
    /// safety-critical field is treated exactly like a confirmed
    /// over-threshold reading would be for the OTHER checks -- `Decrease`,
    /// never `Hold` and never `Increase` -- because "I can't tell if it's
    /// safe" must never be read as "it's fine". "Safety-critical" here is
    /// `cpu_load`/`io_wait_pct`/`disk_temp_max`/`smart_delta_reallocated`
    /// only, NOT `user_io_latency_p99_ms`: the project's actual
    /// `LiveMetricsSampler` (`shr-orchestrate`) has no real data source for
    /// user IO latency at all (documented gap -- see its doc comment) and
    /// always reports it as `None`. Treating that as "unreadable, must
    /// decelerate" would make EVERY live-sampled tick decrease forever,
    /// never hold or accelerate, turning the one signal nobody measures yet
    /// into a permanent brake on every other signal that IS measured and
    /// healthy. `user_io_latency_p99_ms` is used opportunistically (via
    /// `over_threshold` below) when a sampler DOES supply it, and simply
    /// ignored otherwise -- never treated as a safety gap on its own.
    pub fn tick(&mut self) -> ThrottleDecision {
        let Some(m) = self.sampler.sample() else {
            return ThrottleDecision::Hold;
        };

        let confirmed_danger = m.smart_delta_reallocated.is_some_and(|v| v > 0)
            || m.disk_temp_max.is_some_and(|t| t >= self.thresholds.max_disk_temp_c);
        if confirmed_danger {
            return ThrottleDecision::EmergencyBrake;
        }

        let over_threshold = m.cpu_load.is_some_and(|v| v > self.thresholds.max_cpu_load)
            || m.io_wait_pct.is_some_and(|v| v > self.thresholds.max_io_wait_pct)
            || m.user_io_latency_p99_ms.is_some_and(|v| v > self.thresholds.user_io_latency_ms);
        let any_signal_unreadable = m.cpu_load.is_none()
            || m.io_wait_pct.is_none()
            || m.disk_temp_max.is_none()
            || m.smart_delta_reallocated.is_none();
        if over_threshold || any_signal_unreadable {
            return ThrottleDecision::Decrease(0.7);
        }

        if m.cpu_load.is_some_and(|v| v < 0.5) && m.io_wait_pct.is_some_and(|v| v < 15.0) {
            return ThrottleDecision::Increase(1.2);
        }

        ThrottleDecision::Hold
    }
}

/// User-facing speed profile, selected via `expand --priority`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReshapePriority {
    Background,
    Balanced,
    Max,
}

impl ReshapePriority {
    pub fn initial_speed_kb(self) -> u64 {
        match self {
            ReshapePriority::Background => 20_000,
            ReshapePriority::Balanced => RESHAPE_SPEED_INITIAL_KB,
            ReshapePriority::Max => RESHAPE_SPEED_CEILING_KB,
        }
    }

    /// `None` means unlimited (the design's `max` row).
    pub fn ceiling_kb(self) -> Option<u64> {
        match self {
            ReshapePriority::Background => Some(100_000),
            ReshapePriority::Balanced => Some(RESHAPE_SPEED_CEILING_KB),
            ReshapePriority::Max => None,
        }
    }

    /// The design's per-profile decelerate thresholds. `max` watches disk
    /// temperature/SMART only -- so its load/io-wait/latency thresholds
    /// are set to never trigger `Decrease`, while `EmergencyBrake`'s
    /// temperature/SMART checks (not gated by this struct's other fields)
    /// still apply unconditionally.
    pub fn thresholds(self) -> SafetyThresholds {
        match self {
            ReshapePriority::Background => SafetyThresholds {
                max_cpu_load: 0.5,
                user_io_latency_ms: 50,
                ..SafetyThresholds::default()
            },
            ReshapePriority::Balanced => SafetyThresholds::default(),
            ReshapePriority::Max => SafetyThresholds {
                max_cpu_load: f64::INFINITY,
                max_io_wait_pct: f64::INFINITY,
                user_io_latency_ms: u64::MAX,
                ..SafetyThresholds::default()
            },
        }
    }
}

/// Default `MetricsSampler` for a caller that hasn't wired a real, live one
/// in yet. This used to fabricate a specific, plausible-looking "normal"
/// sample (`cpu_load: 0.5`, `disk_temp_max: 40`, ...) -- indistinguishable
/// from a live sampler that had actually measured a healthy system, which
/// meant an unmonitored reshape silently asserted "everything is fine"
/// instead of honestly saying "nobody is watching". It now returns `None`:
/// an explicit "not monitoring" declaration `ReshapeThrottle::tick` holds on
/// (verified by `null_sampler_holds_under_every_priority_profile` below), so
/// an unwired reshape still just runs at its priority profile's initial
/// speed with no adjustment -- the same behavior a reshape had before live
/// throttling existed at all -- without ever claiming to know the system is
/// healthy.
pub struct NullMetricsSampler;

impl MetricsSampler for NullMetricsSampler {
    fn sample(&self) -> Option<ThrottleMetrics> {
        None
    }
}

/// Applies `ThrottleDecision`s to one band's (`md_name`'s) real kernel
/// parameters. Tracks `current_speed_kb` itself (mirrors what it last wrote)
/// so `apply()` only issues a write when the proposed speed actually
/// changes -- ticking `Hold` every few seconds for hours must not spam
/// `speed_limit_max` writes for no reason.
pub struct ThrottleController<'a> {
    runner: &'a dyn CommandRunner,
    md_name: String,
    ceiling_kb: Option<u64>,
    floor_kb: u64,
    current_speed_kb: u64,
}

impl<'a> ThrottleController<'a> {
    pub fn new(runner: &'a dyn CommandRunner, md_name: impl Into<String>, priority: ReshapePriority) -> Self {
        Self {
            runner,
            md_name: md_name.into(),
            ceiling_kb: priority.ceiling_kb(),
            floor_kb: RESHAPE_SPEED_FLOOR_KB,
            current_speed_kb: priority.initial_speed_kb(),
        }
    }

    /// Same as `new`, but seeds `current_speed_kb` from the band's REAL,
    /// currently-written `speed_limit_max` (via `CommandRunner`) instead of
    /// assuming the priority profile's initial value. A periodic
    /// throttle tick fired by a systemd timer runs in a brand-new process
    /// every time -- there is no in-memory `ThrottleController` surviving
    /// between ticks the way there is within one `expand()` call -- so a
    /// tick that assumed "current speed = initial speed" would silently
    /// re-widen a speed a PREVIOUS tick had already narrowed for safety.
    /// Falls back to the priority's initial speed if the kernel value can't
    /// be read (dry-run, or a transient read failure) -- the same value
    /// `new()` would have started from, never a fabricated "safe" guess.
    pub fn resume(runner: &'a dyn CommandRunner, md_name: impl Into<String>, priority: ReshapePriority) -> Self {
        let md_name = md_name.into();
        let current_speed_kb =
            read_current_speed_limit_max(runner).unwrap_or_else(|| priority.initial_speed_kb());
        Self {
            runner,
            md_name,
            ceiling_kb: priority.ceiling_kb(),
            floor_kb: RESHAPE_SPEED_FLOOR_KB,
            current_speed_kb,
        }
    }

    pub fn current_speed_kb(&self) -> u64 {
        self.current_speed_kb
    }

    /// Set this band's initial kernel parameters when its
    /// reshape starts -- called once, right after `mdadm --grow` succeeds.
    pub fn apply_initial(&self) -> Result<(), ExecError> {
        write_sysfs(
            self.runner,
            "/proc/sys/dev/raid/speed_limit_min",
            &SPEED_LIMIT_MIN_DEFAULT_KB.to_string(),
        )?;
        write_sysfs(
            self.runner,
            "/proc/sys/dev/raid/speed_limit_max",
            &self.current_speed_kb.to_string(),
        )?;
        write_sysfs(self.runner, &self.sysfs_md_path("stripe_cache_size"), &STRIPE_CACHE_DEFAULT.to_string())?;
        Ok(())
    }

    /// Apply one `ThrottleDecision`, returning the resulting speed (KB/s).
    /// Every proposed speed is clamped to `[floor_kb, min(ceiling_kb,
    /// RESHAPE_SPEED_CEILING_KB)]` regardless of decision -- `EmergencyBrake`
    /// is the only decision allowed to ignore the profile's OWN ceiling (it
    /// never needs to; the floor is always <= any ceiling) but must still
    /// never go negative.
    pub fn apply(&mut self, decision: ThrottleDecision) -> Result<u64, ExecError> {
        let hard_ceiling = self.ceiling_kb.unwrap_or(RESHAPE_SPEED_CEILING_KB).min(RESHAPE_SPEED_CEILING_KB);
        let proposed = match decision {
            ThrottleDecision::EmergencyBrake => self.floor_kb,
            ThrottleDecision::Decrease(factor) => scale(self.current_speed_kb, factor).max(self.floor_kb),
            ThrottleDecision::Increase(factor) => scale(self.current_speed_kb, factor).min(hard_ceiling),
            ThrottleDecision::Hold => self.current_speed_kb,
        };

        if proposed != self.current_speed_kb {
            write_sysfs(self.runner, "/proc/sys/dev/raid/speed_limit_max", &proposed.to_string())?;
        }
        self.current_speed_kb = proposed;
        Ok(proposed)
    }

    fn sysfs_md_path(&self, leaf: &str) -> String {
        format!("/sys/block/{}/md/{leaf}", self.md_name)
    }
}

/// Read `/proc/sys/dev/raid/speed_limit_max`'s CURRENT value via
/// `CommandRunner` (never raw `std::fs` -- see this module's doc comment).
/// `None` under dry-run (nothing real to read) or any read/parse failure --
/// `ThrottleController::resume`'s caller falls back to the priority's
/// initial speed in that case.
fn read_current_speed_limit_max(runner: &dyn CommandRunner) -> Option<u64> {
    if runner.is_dry_run() {
        return None;
    }
    let output = runner.run("cat", &["/proc/sys/dev/raid/speed_limit_max"]).ok()?;
    output.stdout.trim().parse().ok()
}

fn scale(kb: u64, factor: f64) -> u64 {
    ((kb as f64) * factor).round().max(0.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CommandOutput, ExecError as Err};
    use std::sync::Mutex;

    struct FixedSampler(Option<ThrottleMetrics>);
    impl MetricsSampler for FixedSampler {
        fn sample(&self) -> Option<ThrottleMetrics> {
            self.0
        }
    }

    fn some_metrics(m: ThrottleMetrics) -> FixedSampler {
        FixedSampler(Some(m))
    }

    fn metrics() -> ThrottleMetrics {
        ThrottleMetrics {
            cpu_load: Some(0.6),
            io_wait_pct: Some(20.0),
            user_io_latency_p99_ms: Some(50),
            disk_temp_max: Some(40),
            smart_delta_reallocated: Some(0),
        }
    }

    #[test]
    fn tick_holds_when_metrics_are_in_the_normal_band() {
        let sampler = some_metrics(metrics());
        let mut throttle = ReshapeThrottle::new(SafetyThresholds::default(), &sampler);
        assert_eq!(throttle.tick(), ThrottleDecision::Hold);
    }

    #[test]
    fn tick_emergency_brakes_when_smart_reallocated_count_increases() {
        let sampler = some_metrics(ThrottleMetrics { smart_delta_reallocated: Some(1), ..metrics() });
        let mut throttle = ReshapeThrottle::new(SafetyThresholds::default(), &sampler);
        assert_eq!(throttle.tick(), ThrottleDecision::EmergencyBrake);
    }

    #[test]
    fn tick_emergency_brakes_when_disk_temperature_hits_the_threshold() {
        let sampler = some_metrics(ThrottleMetrics { disk_temp_max: Some(50), ..metrics() });
        let mut throttle = ReshapeThrottle::new(SafetyThresholds::default(), &sampler);
        assert_eq!(throttle.tick(), ThrottleDecision::EmergencyBrake);
    }

    #[test]
    fn tick_decreases_when_cpu_load_exceeds_threshold() {
        let sampler = some_metrics(ThrottleMetrics { cpu_load: Some(0.9), ..metrics() });
        let mut throttle = ReshapeThrottle::new(SafetyThresholds::default(), &sampler);
        assert_eq!(throttle.tick(), ThrottleDecision::Decrease(0.7));
    }

    #[test]
    fn tick_decreases_when_user_io_latency_exceeds_threshold() {
        let sampler = some_metrics(ThrottleMetrics { user_io_latency_p99_ms: Some(150), ..metrics() });
        let mut throttle = ReshapeThrottle::new(SafetyThresholds::default(), &sampler);
        assert_eq!(throttle.tick(), ThrottleDecision::Decrease(0.7));
    }

    #[test]
    fn tick_increases_when_system_is_idle() {
        let sampler = some_metrics(ThrottleMetrics { cpu_load: Some(0.2), io_wait_pct: Some(5.0), ..metrics() });
        let mut throttle = ReshapeThrottle::new(SafetyThresholds::default(), &sampler);
        assert_eq!(throttle.tick(), ThrottleDecision::Increase(1.2));
    }

    #[test]
    fn danger_signal_wins_over_an_otherwise_idle_system() {
        let sampler = some_metrics(ThrottleMetrics {
            cpu_load: Some(0.1),
            io_wait_pct: Some(2.0),
            smart_delta_reallocated: Some(3),
            ..metrics()
        });
        let mut throttle = ReshapeThrottle::new(SafetyThresholds::default(), &sampler);
        assert_eq!(throttle.tick(), ThrottleDecision::EmergencyBrake);
    }

    /// No monitoring configured at all (`sample() == None`) must hold,
    /// not brake or accelerate -- there is no signal to react to, and this
    /// is the behavior every pre-throttle caller must keep seeing.
    #[test]
    fn tick_holds_when_the_sampler_reports_no_monitoring_at_all() {
        let sampler = FixedSampler(None);
        let mut throttle = ReshapeThrottle::new(SafetyThresholds::default(), &sampler);
        assert_eq!(throttle.tick(), ThrottleDecision::Hold);
    }

    /// Core safety requirement: a LIVE sampler (one that IS monitoring)
    /// failing to read even one signal must decelerate, never hold or
    /// accelerate -- "unknown" must never be read as "known safe". This is
    /// what distinguishes a real sampler's partial failure from
    /// `NullMetricsSampler`'s honest "not monitoring" declaration above.
    #[test]
    fn tick_decreases_when_a_live_sampler_cannot_read_the_smart_signal() {
        let sampler = some_metrics(ThrottleMetrics { smart_delta_reallocated: None, ..metrics() });
        let mut throttle = ReshapeThrottle::new(SafetyThresholds::default(), &sampler);
        assert_eq!(throttle.tick(), ThrottleDecision::Decrease(0.7));
    }

    #[test]
    fn tick_decreases_when_a_live_sampler_cannot_read_disk_temperature() {
        let sampler = some_metrics(ThrottleMetrics { disk_temp_max: None, ..metrics() });
        let mut throttle = ReshapeThrottle::new(SafetyThresholds::default(), &sampler);
        assert_eq!(throttle.tick(), ThrottleDecision::Decrease(0.7));
    }

    #[test]
    fn tick_decreases_when_a_live_sampler_cannot_read_cpu_load_even_if_everything_else_is_benign() {
        let sampler = some_metrics(ThrottleMetrics { cpu_load: None, ..metrics() });
        let mut throttle = ReshapeThrottle::new(SafetyThresholds::default(), &sampler);
        assert_eq!(throttle.tick(), ThrottleDecision::Decrease(0.7));
    }

    /// The project's real `LiveMetricsSampler` never measures user IO
    /// latency at all (no data source -- see `tick`'s doc comment). Its
    /// permanent absence must not, by itself, force every live-sampled tick
    /// to decelerate forever.
    #[test]
    fn a_missing_user_io_latency_signal_alone_does_not_force_a_decrease() {
        let sampler = some_metrics(ThrottleMetrics { user_io_latency_p99_ms: None, ..metrics() });
        let mut throttle = ReshapeThrottle::new(SafetyThresholds::default(), &sampler);
        assert_eq!(throttle.tick(), ThrottleDecision::Hold);
    }

    /// An unreadable signal must not be masked by an otherwise-idle system --
    /// unknown never gets to look like "safe to accelerate".
    #[test]
    fn unknown_signal_wins_over_an_otherwise_idle_system() {
        let sampler = some_metrics(ThrottleMetrics {
            cpu_load: Some(0.1),
            io_wait_pct: None,
            ..metrics()
        });
        let mut throttle = ReshapeThrottle::new(SafetyThresholds::default(), &sampler);
        assert_eq!(throttle.tick(), ThrottleDecision::Decrease(0.7));
    }

    /// Records every command like `DryRunRunner`, but reports
    /// `is_dry_run() == false` -- needed here because `ThrottleController`
    /// is meant to run against a REAL array, and the point of these tests
    /// is proving a write actually happens, not that dry-run's own no-op
    /// path behaves. `cat_response`, if set, is what any `cat` invocation
    /// returns (used by `resume()`'s tests to simulate a real current
    /// `speed_limit_max` reading).
    #[derive(Default)]
    struct SpyRunner {
        commands: Mutex<Vec<String>>,
        cat_response: Option<String>,
    }
    impl SpyRunner {
        fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
        fn with_cat_response(response: impl Into<String>) -> Self {
            Self { commands: Mutex::new(Vec::new()), cat_response: Some(response.into()) }
        }
    }
    impl CommandRunner for SpyRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, Err> {
            self.commands.lock().unwrap().push(format!("{program} {}", args.join(" ")));
            let stdout = if program == "cat" { self.cat_response.clone().unwrap_or_default() } else { String::new() };
            Ok(CommandOutput { stdout, stderr: String::new() })
        }
        fn is_dry_run(&self) -> bool {
            false
        }
    }

    #[test]
    fn apply_initial_writes_the_priority_profiles_starting_speed_limit() {
        let runner = SpyRunner::default();
        let ctrl = ThrottleController::new(&runner, "md0", ReshapePriority::Background);
        ctrl.apply_initial().unwrap();
        let commands = runner.commands();
        assert!(commands.iter().any(|c| c.contains("speed_limit_max") && c.contains("20000")));
        assert!(commands.iter().any(|c| c.contains("speed_limit_min")));
        assert!(commands.iter().any(|c| c.contains("stripe_cache_size") && c.contains("md0")));
    }

    #[test]
    fn emergency_brake_actually_lowers_speed_limit_max_not_just_logs() {
        let runner = SpyRunner::default();
        let mut ctrl = ThrottleController::new(&runner, "md0", ReshapePriority::Balanced);
        let before = ctrl.current_speed_kb();

        let after = ctrl.apply(ThrottleDecision::EmergencyBrake).unwrap();

        assert!(after < before, "emergency brake must actually reduce the speed, not just report it");
        assert_eq!(after, RESHAPE_SPEED_FLOOR_KB);
        let commands = runner.commands();
        assert!(
            commands.iter().any(|c| c.contains("speed_limit_max") && c.contains(&after.to_string())),
            "the lowered speed must actually be written to the kernel parameter: {commands:?}"
        );
    }

    #[test]
    fn decrease_writes_a_strictly_lower_speed_than_the_current_one() {
        let runner = SpyRunner::default();
        let mut ctrl = ThrottleController::new(&runner, "md0", ReshapePriority::Balanced);
        let before = ctrl.current_speed_kb();

        let after = ctrl.apply(ThrottleDecision::Decrease(0.7)).unwrap();

        assert!(after < before);
        assert_eq!(after, (before as f64 * 0.7).round() as u64);
        let commands = runner.commands();
        assert!(commands.iter().any(|c| c.contains("speed_limit_max") && c.contains(&after.to_string())));
    }

    #[test]
    fn decrease_never_goes_below_the_floor() {
        let runner = SpyRunner::default();
        let mut ctrl = ThrottleController::new(&runner, "md0", ReshapePriority::Background);
        for _ in 0..20 {
            ctrl.apply(ThrottleDecision::Decrease(0.7)).unwrap();
        }
        assert_eq!(ctrl.current_speed_kb(), RESHAPE_SPEED_FLOOR_KB);
    }

    #[test]
    fn increase_never_exceeds_the_priority_profiles_ceiling() {
        let runner = SpyRunner::default();
        let mut ctrl = ThrottleController::new(&runner, "md0", ReshapePriority::Background);
        for _ in 0..20 {
            ctrl.apply(ThrottleDecision::Increase(1.2)).unwrap();
        }
        assert_eq!(ctrl.current_speed_kb(), 100_000, "background priority's ceiling is 100 MB/s");
    }

    #[test]
    fn increase_under_max_priority_can_exceed_the_balanced_ceiling_but_not_the_hard_ceiling() {
        let runner = SpyRunner::default();
        let mut ctrl = ThrottleController::new(&runner, "md0", ReshapePriority::Max);
        for _ in 0..20 {
            ctrl.apply(ThrottleDecision::Increase(1.2)).unwrap();
        }
        assert_eq!(ctrl.current_speed_kb(), RESHAPE_SPEED_CEILING_KB);
    }

    #[test]
    fn hold_does_not_issue_a_redundant_kernel_write() {
        let runner = SpyRunner::default();
        let mut ctrl = ThrottleController::new(&runner, "md0", ReshapePriority::Balanced);
        let before_count = runner.commands().len();
        ctrl.apply(ThrottleDecision::Hold).unwrap();
        assert_eq!(runner.commands().len(), before_count, "Hold must not write speed_limit_max again");
    }

    #[test]
    fn max_priority_thresholds_never_trigger_decrease_from_load_alone() {
        let sampler = some_metrics(ThrottleMetrics {
            cpu_load: Some(999.0),
            io_wait_pct: Some(999.0),
            user_io_latency_p99_ms: Some(u64::MAX - 1),
            ..metrics()
        });
        let mut throttle = ReshapeThrottle::new(ReshapePriority::Max.thresholds(), &sampler);
        assert_eq!(throttle.tick(), ThrottleDecision::Hold);
    }

    #[test]
    fn resume_seeds_current_speed_from_the_kernels_real_speed_limit_max() {
        let runner = SpyRunner::with_cat_response("42000\n");
        let ctrl = ThrottleController::resume(&runner, "md0", ReshapePriority::Balanced);
        assert_eq!(ctrl.current_speed_kb(), 42_000, "must read the REAL current speed, not the profile's initial one");
    }

    #[test]
    fn resume_falls_back_to_the_priority_profiles_initial_speed_when_the_kernel_value_is_unreadable() {
        let runner = SpyRunner::default(); // cat returns "" -- unparseable
        let ctrl = ThrottleController::resume(&runner, "md0", ReshapePriority::Background);
        assert_eq!(ctrl.current_speed_kb(), ReshapePriority::Background.initial_speed_kb());
    }

    #[test]
    fn null_sampler_holds_under_every_priority_profile() {
        // The engine's default when no live sampler is wired -- must never
        // silently accelerate or brake a reshape nobody asked it to monitor.
        for priority in [ReshapePriority::Background, ReshapePriority::Balanced, ReshapePriority::Max] {
            let mut throttle = ReshapeThrottle::new(priority.thresholds(), &NullMetricsSampler);
            assert_eq!(throttle.tick(), ThrottleDecision::Hold, "{priority:?} did not hold");
        }
    }
}
