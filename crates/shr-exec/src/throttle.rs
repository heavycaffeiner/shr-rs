//! Adaptive speed control for every md sync activity (reshape, resync,
//! recovery, scrub). Two layers:
//!
//! - Pure algorithm (`SyncPriority`, `SafetyThresholds`, `ThrottleMetrics`,
//!   `CapabilityEstimate`, `ReshapeThrottle::tick`) -- no I/O, unit-testable
//!   from injected metrics alone.
//! - `ThrottleController` -- applies a `ThrottleDecision` to the real kernel
//!   parameters via `CommandRunner`. Never raw `std::fs`: these are
//!   `/proc`/`/sys` paths, unmockable and always an IO error on the Windows
//!   dev host `cargo test` runs on natively (same rationale as
//!   `MdadmExecutor::degraded_count`'s doc comment).
//!
//! A profile is an intent, not a KB/s constant: `Balanced` and `Background`
//! are fractions of the array's own measured sync capability, and `Max` is
//! the absence of any artificial limit. Absolute constants would mean one
//! thing on a 2-disk SATA array and something else entirely on a 12-disk SAS
//! one.

use crate::cmd::{write_sysfs, CommandRunner, ExecError};

/// Written to both `sync_speed_min` and `sync_speed_max` for `Max`: a single
/// number above any rate the target hardware can reach, so md never backs the
/// sync off under contention. A floor md can never reach is exactly what
/// "no artificial limit" means at this interface -- there is no "unlimited"
/// value to write.
pub const UNBOUNDED_SPEED_KB: u64 = 10_000_000;
/// Lower bound on a bounded profile's floor. A floor set below the rate at
/// which md still streams produces a stuttering sync (burst, back off, seek
/// away, seek back), which costs both the sync and the foreground work. This
/// only guards against a nonsensical capability estimate; the real
/// anti-stutter guarantee is the `min_fraction` in the table below.
pub const STREAM_FLOOR_ABS_KB: u64 = 15_000;
pub const STRIPE_CACHE_DEFAULT: u32 = 4096;

/// Consecutive ticks below `CAPABILITY_UNCAPPED_FRACTION` of the current
/// ceiling before the capability estimate is allowed to decay -- one slow
/// sample is noise, three in a row is the array.
pub const CAPABILITY_UNCAPPED_TICKS: u32 = 3;
/// Below this fraction of the written ceiling, the ceiling is not what is
/// holding the sync back, so the observation says something about the array.
pub const CAPABILITY_UNCAPPED_FRACTION: f64 = 0.7;
/// Weight kept on the old estimate when it decays toward an observation.
pub const CAPABILITY_DECAY_OLD: f64 = 0.9;
/// Increase when every measured contention signal sits below this fraction
/// of its own decrease threshold. The gap between the two is the hysteresis
/// band that stops the throttle oscillating.
pub const INCREASE_HYSTERESIS: f64 = 0.8;

pub const DECREASE_FACTOR: f64 = 0.7;
pub const INCREASE_FACTOR: f64 = 1.2;

/// The safety limits `ReshapeThrottle::tick` checks every sample against.
/// `max_cpu_load` is a per-core fraction (load average divided by the online
/// CPU count), not a raw load average.
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

/// One sample of the signals `ReshapeThrottle::tick` decides on. Every signal
/// is `Option`: a real sampler can fail to read any one independently of the
/// others (smartctl not installed, a transient `/proc` read error, no prior
/// sample yet to diff against), and `tick()` must never treat a missing
/// signal as "known good" (see `tick`'s doc comment).
/// `smart_delta_reallocated` is the increase in reallocated-sector count
/// since the previous sample (not the raw counter) -- the rule is "increase
/// triggers emergency brake", and a nonzero *absolute* count from a drive
/// with pre-existing reallocations would otherwise brake every tick forever.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ThrottleMetrics {
    /// 1-minute load average divided by the online CPU count. `None` when
    /// either half could not be read -- never the raw average, which would
    /// be compared against a per-core threshold and trip on every tick.
    pub cpu_load: Option<f64>,
    /// The divisor `cpu_load` was normalised by, so a report can show both
    /// figures.
    pub cpu_count: Option<u32>,
    pub io_wait_pct: Option<f64>,
    pub user_io_latency_p99_ms: Option<u64>,
    pub disk_temp_max: Option<u8>,
    pub smart_delta_reallocated: Option<u64>,
    /// This is the operation's first sample, so `smart_delta_reallocated`
    /// being `None` is expected (there is no previous total to diff against)
    /// rather than a failed read. Suppresses the SMART-unreadable decrease
    /// for that one tick only.
    pub first_sample: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ThrottleDecision {
    EmergencyBrake,
    Decrease(f64),
    Increase(f64),
    Hold,
}

impl ThrottleDecision {
    /// The persisted/reported form (`StateBand::last_throttle_decision`).
    pub fn as_str(self) -> &'static str {
        match self {
            ThrottleDecision::EmergencyBrake => "emergency-brake",
            ThrottleDecision::Decrease(_) => "decrease",
            ThrottleDecision::Increase(_) => "increase",
            ThrottleDecision::Hold => "hold",
        }
    }
}

/// One `tick()` result: what to do, and the specific signal that decided it
/// (`disk_temp 52C >= 50C`, `smart unreadable`). Without the reason there is
/// no way to learn WHY a sync is running at its floor short of reading sysfs
/// by hand.
#[derive(Debug, Clone, PartialEq)]
pub struct ThrottleTick {
    pub decision: ThrottleDecision,
    pub reason: String,
}

impl ThrottleTick {
    fn new(decision: ThrottleDecision, reason: impl Into<String>) -> Self {
        Self {
            decision,
            reason: reason.into(),
        }
    }
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
    priority: SyncPriority,
    thresholds: SafetyThresholds,
    sampler: &'a dyn MetricsSampler,
}

impl<'a> ReshapeThrottle<'a> {
    pub fn new(priority: SyncPriority, sampler: &'a dyn MetricsSampler) -> Self {
        Self {
            priority,
            thresholds: priority.thresholds(),
            sampler,
        }
    }

    /// Checked in priority order -- emergency brake beats decrease beats
    /// increase beats hold -- so a sample that satisfies both a danger
    /// condition and a "system is idle" condition always brakes, never
    /// accelerates.
    ///
    /// `self.sampler.sample() == None` means no monitoring is configured at
    /// all (`NullMetricsSampler`) -- holds, since there is no signal,
    /// dangerous or otherwise, to react to.
    ///
    /// A `Some(metrics)` whose individual fields are `None` is different: a
    /// live sampler tried to measure and could not, and the two kinds of
    /// signal are handled differently.
    ///
    /// - `disk_temp_max` and `smart_delta_reallocated` are safety-critical:
    ///   `EmergencyBrake` depends on them, so not knowing them decreases
    ///   under every profile, `Max` included. (Under `Max` that decrease is
    ///   a no-op, because `Max`'s own floor is `UNBOUNDED_SPEED_KB` -- the
    ///   profile still cannot be slowed by a signal it never consults, while
    ///   a confirmed danger still brakes it.)
    /// - `cpu_load` and `io_wait_pct` are contention signals: unreadable
    ///   decreases under `Background`/`Balanced` and holds under `Max`,
    ///   which has already declared them irrelevant by setting their
    ///   thresholds to saturating values. Braking `Max` on a signal `Max`
    ///   does not consult is incoherent.
    /// - `user_io_latency_p99_ms` is neither: the project's real
    ///   `LiveMetricsSampler` has no data source for it and always reports
    ///   `None`, so treating its absence as a gap would make every
    ///   live-sampled tick decrease forever. Used opportunistically when a
    ///   sampler does supply it, ignored otherwise.
    ///
    /// `smart_delta_reallocated == None` on the FIRST sample of an operation
    /// is expected rather than a failed read (nothing to diff against yet),
    /// and is suppressed for that tick only -- a later failed read still
    /// brakes, two minutes later.
    pub fn tick(&mut self) -> ThrottleTick {
        let Some(m) = self.sampler.sample() else {
            return ThrottleTick::new(ThrottleDecision::Hold, "no monitoring configured");
        };

        if let Some(delta) = m
            .smart_delta_reallocated
            .filter(|&v| v > self.thresholds.max_smart_reallocated)
        {
            return ThrottleTick::new(
                ThrottleDecision::EmergencyBrake,
                format!("smart reallocated +{delta}"),
            );
        }
        if let Some(t) = m.disk_temp_max.filter(|&t| t >= self.thresholds.max_disk_temp_c) {
            return ThrottleTick::new(
                ThrottleDecision::EmergencyBrake,
                format!("disk_temp {t}C >= {}C", self.thresholds.max_disk_temp_c),
            );
        }

        if m.disk_temp_max.is_none() {
            return ThrottleTick::new(
                ThrottleDecision::Decrease(DECREASE_FACTOR),
                "disk temperature unreadable",
            );
        }
        if m.smart_delta_reallocated.is_none() && !m.first_sample {
            return ThrottleTick::new(ThrottleDecision::Decrease(DECREASE_FACTOR), "smart unreadable");
        }

        if let Some(v) = m.cpu_load.filter(|&v| v > self.thresholds.max_cpu_load) {
            return ThrottleTick::new(
                ThrottleDecision::Decrease(DECREASE_FACTOR),
                format!("cpu load {v:.2} > {:.2}", self.thresholds.max_cpu_load),
            );
        }
        if let Some(v) = m.io_wait_pct.filter(|&v| v > self.thresholds.max_io_wait_pct) {
            return ThrottleTick::new(
                ThrottleDecision::Decrease(DECREASE_FACTOR),
                format!("io wait {v:.1}% > {:.1}%", self.thresholds.max_io_wait_pct),
            );
        }
        if let Some(v) = m
            .user_io_latency_p99_ms
            .filter(|&v| v > self.thresholds.user_io_latency_ms)
        {
            return ThrottleTick::new(
                ThrottleDecision::Decrease(DECREASE_FACTOR),
                format!("user io latency {v}ms > {}ms", self.thresholds.user_io_latency_ms),
            );
        }

        if m.cpu_load.is_none() || m.io_wait_pct.is_none() {
            return if self.priority.ignores_contention() {
                ThrottleTick::new(
                    ThrottleDecision::Hold,
                    "contention signals unreadable, and this profile does not consult them",
                )
            } else {
                ThrottleTick::new(
                    ThrottleDecision::Decrease(DECREASE_FACTOR),
                    "contention signals unreadable",
                )
            };
        }

        let quiet =
            |value: Option<f64>, threshold: f64| value.is_some_and(|v| v < threshold * INCREASE_HYSTERESIS);
        // A saturating threshold means the profile has declared this signal
        // irrelevant; scaling it by 0.8 would turn that declaration into a
        // finite limit that a large reading could still fail.
        let latency_quiet = self.thresholds.user_io_latency_ms == u64::MAX
            || m.user_io_latency_p99_ms
                .is_none_or(|v| (v as f64) < self.thresholds.user_io_latency_ms as f64 * INCREASE_HYSTERESIS);
        if quiet(m.cpu_load, self.thresholds.max_cpu_load)
            && quiet(m.io_wait_pct, self.thresholds.max_io_wait_pct)
            && latency_quiet
        {
            return ThrottleTick::new(
                ThrottleDecision::Increase(INCREASE_FACTOR),
                "every contention signal below its increase threshold",
            );
        }

        ThrottleTick::new(ThrottleDecision::Hold, "within the hysteresis band")
    }
}

/// User-facing speed profile, selected via `--priority` on `expand` and
/// `fs scrub start`, and applied to every md sync activity this project
/// starts (reshape, post-`create` resync, post-`replace_disk` recovery,
/// scrub).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPriority {
    Background,
    Balanced,
    Max,
}

/// A profile's shape: what fraction of the array's measured capability it
/// claims, and the absolute pair to use until an estimate exists. The six
/// fractions are the whole model, so they live in exactly one place.
struct ProfileShape {
    max_fraction: f64,
    min_fraction: f64,
    bootstrap_max_kb: u64,
    bootstrap_min_kb: u64,
}

/// One profile's derived kernel limits, in KB/s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncLimits {
    pub max_kb: u64,
    pub min_kb: u64,
}

impl SyncPriority {
    /// `None` for `Max`: no fraction of anything, no estimate needed.
    fn shape(self) -> Option<ProfileShape> {
        match self {
            SyncPriority::Background => Some(ProfileShape {
                max_fraction: 0.35,
                min_fraction: 0.20,
                bootstrap_max_kb: 60_000,
                bootstrap_min_kb: 25_000,
            }),
            SyncPriority::Balanced => Some(ProfileShape {
                max_fraction: 0.75,
                min_fraction: 0.35,
                bootstrap_max_kb: 150_000,
                bootstrap_min_kb: 60_000,
            }),
            SyncPriority::Max => None,
        }
    }

    /// This profile's `sync_speed_min`/`sync_speed_max` for an array whose
    /// capability is `capability_kb`. The bootstrap pair applies only until
    /// the first estimate exists and is overwritten by the derived value at
    /// the first tick that yields one.
    pub fn limits(self, capability_kb: Option<u64>) -> SyncLimits {
        let Some(shape) = self.shape() else {
            return SyncLimits {
                max_kb: UNBOUNDED_SPEED_KB,
                min_kb: UNBOUNDED_SPEED_KB,
            };
        };
        let (max_kb, min_kb) = match capability_kb {
            Some(c) if c > 0 => (scale(c, shape.max_fraction), scale(c, shape.min_fraction)),
            _ => (shape.bootstrap_max_kb, shape.bootstrap_min_kb),
        };
        // Applied to both bounded profiles, not `Background` alone: a
        // capability estimate small enough to push `Background`'s floor
        // under the streaming bound pushes `Balanced`'s under it too, and a
        // `Balanced` floor below `Background`'s would invert the profiles.
        let min_kb = min_kb.max(STREAM_FLOOR_ABS_KB);
        SyncLimits {
            max_kb: max_kb.max(min_kb),
            min_kb,
        }
    }

    /// Where `EmergencyBrake` aims under EVERY profile, `Max` included:
    /// `Background`'s own floor. Braking to a rate that stutters helps
    /// nothing, and at that point the notification is what matters.
    pub fn emergency_target_kb(capability_kb: Option<u64>) -> u64 {
        SyncPriority::Background.limits(capability_kb).min_kb
    }

    /// `Max` sets its contention thresholds to saturating values, which is
    /// the same statement as "this profile does not consult them" -- so an
    /// unreadable contention signal must not brake it either.
    pub fn ignores_contention(self) -> bool {
        matches!(self, SyncPriority::Max)
    }

    /// Per-profile decelerate thresholds. `Max` watches disk temperature and
    /// SMART only -- its load/io-wait/latency thresholds are set to never
    /// trigger `Decrease`, while `EmergencyBrake`'s temperature/SMART checks
    /// still apply unconditionally.
    pub fn thresholds(self) -> SafetyThresholds {
        match self {
            SyncPriority::Background => SafetyThresholds {
                max_cpu_load: 0.5,
                user_io_latency_ms: 50,
                ..SafetyThresholds::default()
            },
            SyncPriority::Balanced => SafetyThresholds::default(),
            SyncPriority::Max => SafetyThresholds {
                max_cpu_load: f64::INFINITY,
                max_io_wait_pct: f64::INFINITY,
                user_io_latency_ms: u64::MAX,
                ..SafetyThresholds::default()
            },
        }
    }

    /// The on-disk/CLI string form, round-tripped by `parse`.
    pub fn as_str(self) -> &'static str {
        match self {
            SyncPriority::Background => "background",
            SyncPriority::Balanced => "balanced",
            SyncPriority::Max => "max",
        }
    }

    /// `None` for anything else, so a caller decides for itself whether an
    /// unrecognised value is a load error or a fallback.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "background" => Some(SyncPriority::Background),
            "balanced" => Some(SyncPriority::Balanced),
            "max" => Some(SyncPriority::Max),
            _ => None,
        }
    }
}

/// The array's sync capability in KB/s, learned from `sync_speed` by the
/// same control loop that uses it -- there is no calibration phase, because
/// a burst to full speed at the start of a `background` operation is exactly
/// the disruption that profile exists to avoid.
///
/// `uncapped_ticks` counts consecutive observations that the ceiling was not
/// what held the sync back; it has to be carried across ticks because each
/// periodic tick is a brand-new process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CapabilityEstimate {
    pub kb: Option<u64>,
    pub uncapped_ticks: u32,
}

impl CapabilityEstimate {
    pub fn new(kb: Option<u64>, uncapped_ticks: u32) -> Self {
        Self { kb, uncapped_ticks }
    }

    /// Fold one `sync_speed` observation in, given the ceiling in force when
    /// it was taken.
    ///
    /// - Any observation above the estimate replaces it: the array
    ///   demonstrably sustained that rate, so capability is at least that.
    /// - An observation below `CAPABILITY_UNCAPPED_FRACTION` of the ceiling
    ///   for `CAPABILITY_UNCAPPED_TICKS` in a row decays the estimate toward
    ///   it, which is how a capability learned on a healthy array falls back
    ///   when the array is degraded, a member is failing, or the operation
    ///   has reached the slower inner tracks.
    ///
    /// A zero or absent observation changes nothing: md reports `none`
    /// between operations, and treating that as "the array can do nothing"
    /// would collapse every derived limit.
    pub fn observe(self, observed_kb: Option<u64>, ceiling_kb: u64) -> Self {
        let Some(observed) = observed_kb.filter(|&v| v > 0) else {
            return self;
        };
        if self.kb.is_none_or(|est| observed > est) {
            return Self {
                kb: Some(observed),
                uncapped_ticks: 0,
            };
        }
        let estimate = self.kb.unwrap_or(observed);
        if (observed as f64) >= ceiling_kb as f64 * CAPABILITY_UNCAPPED_FRACTION {
            return Self {
                kb: Some(estimate),
                uncapped_ticks: 0,
            };
        }
        let uncapped_ticks = (self.uncapped_ticks + 1).min(CAPABILITY_UNCAPPED_TICKS);
        if uncapped_ticks < CAPABILITY_UNCAPPED_TICKS {
            return Self {
                kb: Some(estimate),
                uncapped_ticks,
            };
        }
        let decayed =
            (estimate as f64) * CAPABILITY_DECAY_OLD + (observed as f64) * (1.0 - CAPABILITY_DECAY_OLD);
        Self {
            kb: Some(decayed.round() as u64),
            uncapped_ticks,
        }
    }
}

/// Default `MetricsSampler` for a caller that hasn't wired a real, live one
/// in yet. Returns `None`: an explicit "not monitoring" declaration
/// `ReshapeThrottle::tick` holds on, so an unwired sync still just runs at
/// its profile's limits with no adjustment, without ever claiming to know
/// the system is healthy. (It used to fabricate a plausible-looking
/// "normal" sample, indistinguishable from a live sampler that had actually
/// measured a healthy system.)
pub struct NullMetricsSampler;

impl MetricsSampler for NullMetricsSampler {
    fn sample(&self) -> Option<ThrottleMetrics> {
        None
    }
}

/// Where one band's speed limits are written.
///
/// `PerArray` is `/sys/block/<md>/md/sync_speed_{min,max}`, which shadow the
/// host-wide parameters for that array alone: a per-band profile then means
/// something, two groups can scrub at different profiles at once, and
/// teardown is exact (a write of `system`, not a restore of a remembered
/// number). `HostWide` is the fallback for a kernel without those
/// attributes, and keeps the save-and-restore machinery
/// (`StateFile::saved_speed_limit_max_kb`) that exists for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitScope {
    PerArray,
    HostWide,
}

/// Applies `ThrottleDecision`s to one band's real kernel parameters. Tracks
/// `current_speed_kb` itself (mirrors what it last wrote) so `apply()` only
/// issues a write when the proposed speed actually changes -- ticking `Hold`
/// every two minutes for hours must not spam the kernel for no reason.
pub struct ThrottleController<'a> {
    runner: &'a dyn CommandRunner,
    md_name: String,
    priority: SyncPriority,
    scope: LimitScope,
    limits: SyncLimits,
    capability_kb: Option<u64>,
    current_speed_kb: u64,
    current_floor_kb: u64,
}

impl<'a> ThrottleController<'a> {
    pub fn new(
        runner: &'a dyn CommandRunner,
        md_name: impl Into<String>,
        priority: SyncPriority,
        capability_kb: Option<u64>,
        scope: LimitScope,
    ) -> Self {
        let limits = priority.limits(capability_kb);
        Self {
            runner,
            md_name: md_name.into(),
            priority,
            scope,
            limits,
            capability_kb,
            current_speed_kb: limits.max_kb,
            current_floor_kb: limits.min_kb,
        }
    }

    /// Same as `new`, but seeds `current_speed_kb` from the band's REAL,
    /// currently-written ceiling instead of assuming the profile's own. A
    /// periodic tick fired by a systemd timer runs in a brand-new process
    /// every time -- there is no in-memory controller surviving between
    /// ticks the way there is within one `expand()` call -- so a tick that
    /// assumed "current speed = the profile's ceiling" would silently
    /// re-widen a speed a PREVIOUS tick had already narrowed for safety.
    /// Falls back to the profile's ceiling when the kernel value can't be
    /// read (dry-run, or a transient read failure), never a fabricated
    /// "safe" guess.
    pub fn resume(
        runner: &'a dyn CommandRunner,
        md_name: impl Into<String>,
        priority: SyncPriority,
        capability_kb: Option<u64>,
        scope: LimitScope,
    ) -> Self {
        let mut ctrl = Self::new(runner, md_name, priority, capability_kb, scope);
        if let Some(kb) = read_limit(ctrl.runner, &ctrl.md_name, ctrl.scope, "max") {
            ctrl.current_speed_kb = kb;
        }
        if let Some(kb) = read_limit(ctrl.runner, &ctrl.md_name, ctrl.scope, "min") {
            ctrl.current_floor_kb = kb;
        }
        ctrl
    }

    /// Adopt a fresh capability estimate and re-derive this profile's limits
    /// from it. The estimate is learned by the same control loop that uses
    /// it, so the limits written when the operation started are provisional
    /// until the first observation lands.
    pub fn set_capability(&mut self, capability_kb: Option<u64>) {
        self.capability_kb = capability_kb;
        self.limits = self.priority.limits(capability_kb);
    }

    pub fn current_speed_kb(&self) -> u64 {
        self.current_speed_kb
    }

    pub fn limits(&self) -> SyncLimits {
        self.limits
    }

    pub fn priority(&self) -> SyncPriority {
        self.priority
    }

    /// Set this band's kernel parameters when its sync starts -- both the
    /// ceiling AND the floor, because the floor is what the kernel reduces
    /// the sync rate toward whenever non-sync IO touches the members, and on
    /// a live NAS there is always some. Writing only a ceiling is why
    /// `--priority max` was still throttled: it raised a limit the operation
    /// never reached and left the floor at 1 MB/s.
    pub fn apply_initial(&mut self) -> Result<(), ExecError> {
        self.current_floor_kb = self.limits.min_kb;
        self.write_limit("min", &self.limits.min_kb.to_string())?;
        self.write_limit("max", &self.current_speed_kb.to_string())?;
        // Best-effort, unlike the two limits above: `stripe_cache_size` is a
        // RAID456 write-path knob and simply does not exist on a RAID1
        // array, where the write fails (measured on the guest: a RAID1
        // band's scrub aborted on it). Sizing a cache that isn't there is
        // not a reason to refuse to start the operation.
        if let Err(e) = write_sysfs(
            self.runner,
            &self.sysfs_md_path("stripe_cache_size"),
            &STRIPE_CACHE_DEFAULT.to_string(),
        ) {
            tracing::debug!(
                target: "shr_rs::throttle",
                "{}: stripe_cache_size not set ({e})",
                self.md_name
            );
        }
        Ok(())
    }

    /// Apply one `ThrottleDecision`, returning the resulting ceiling (KB/s).
    ///
    /// Every proposed speed stays inside the profile's own band: `Decrease`
    /// is floored at `limits.min_kb` (so `Background` can never fall below
    /// the rate at which it still streams, and `Max` cannot be slowed at
    /// all), `Increase` is capped at `limits.max_kb`. `EmergencyBrake` is
    /// the one decision allowed out of the band, downward only -- it targets
    /// `Background`'s floor under every profile.
    pub fn apply(&mut self, decision: ThrottleDecision) -> Result<u64, ExecError> {
        // The floor moves when the capability estimate does, so it is
        // re-asserted here rather than only at `apply_initial` -- and only
        // when it actually differs from what this controller last saw
        // written, for the same reason `Hold` issues no ceiling write.
        if self.limits.min_kb != self.current_floor_kb {
            self.write_limit("min", &self.limits.min_kb.to_string())?;
            self.current_floor_kb = self.limits.min_kb;
        }
        let proposed = match decision {
            ThrottleDecision::EmergencyBrake => SyncPriority::emergency_target_kb(self.capability_kb),
            ThrottleDecision::Decrease(factor) => {
                scale(self.current_speed_kb, factor).clamp(self.limits.min_kb, self.limits.max_kb)
            }
            ThrottleDecision::Increase(factor) => {
                scale(self.current_speed_kb, factor).clamp(self.limits.min_kb, self.limits.max_kb)
            }
            ThrottleDecision::Hold => self
                .current_speed_kb
                .clamp(self.limits.min_kb, self.limits.max_kb),
        };

        if proposed != self.current_speed_kb {
            self.write_limit("max", &proposed.to_string())?;
        }
        self.current_speed_kb = proposed;
        Ok(proposed)
    }

    /// Hand the array back to the host-wide parameters once its sync has
    /// finished. Exact by construction under `PerArray`: `system` clears the
    /// local value rather than restoring a remembered number. A no-op under
    /// `HostWide`, where `restore_speed_limit_if_idle` owns the restore
    /// because there is nothing per-array to clear.
    pub fn clear(&self) -> Result<(), ExecError> {
        if self.scope == LimitScope::HostWide {
            return Ok(());
        }
        self.write_limit("min", "system")?;
        self.write_limit("max", "system")?;
        Ok(())
    }

    fn write_limit(&self, leaf: &str, value: &str) -> Result<(), ExecError> {
        write_sysfs(self.runner, &limit_path(&self.md_name, self.scope, leaf), value)
    }

    fn sysfs_md_path(&self, leaf: &str) -> String {
        format!("/sys/block/{}/md/{leaf}", self.md_name)
    }
}

/// `sync_speed_{min,max}` (per array) or `speed_limit_{min,max}`
/// (host-wide), for `leaf` in `{"min", "max"}`.
fn limit_path(md_name: &str, scope: LimitScope, leaf: &str) -> String {
    match scope {
        LimitScope::PerArray => format!("/sys/block/{md_name}/md/sync_speed_{leaf}"),
        LimitScope::HostWide => format!("/proc/sys/dev/raid/speed_limit_{leaf}"),
    }
}

/// Whether this kernel exposes the per-array limit attributes for `md_name`.
/// Probed by reading one of them: a kernel without them fails the read, and
/// so does an array that isn't assembled -- both cases are "do not write
/// per-array limits for this band", which is exactly what the caller needs.
/// `HostWide` under dry-run, where nothing real can be read and nothing real
/// will be written either.
pub fn probe_limit_scope(runner: &dyn CommandRunner, md_name: &str) -> LimitScope {
    if runner.is_dry_run() {
        return LimitScope::HostWide;
    }
    match read_limit(runner, md_name, LimitScope::PerArray, "max") {
        Some(_) => LimitScope::PerArray,
        None => LimitScope::HostWide,
    }
}

/// Read one currently-written limit. The per-array attributes report their
/// origin alongside the number (`200000 (local)`, `1000 (system)`, measured
/// on the Rocky 10.2 guest), so only the first field is parsed.
fn read_limit(runner: &dyn CommandRunner, md_name: &str, scope: LimitScope, leaf: &str) -> Option<u64> {
    if runner.is_dry_run() {
        return None;
    }
    let path = limit_path(md_name, scope, leaf);
    let output = runner.run("cat", &[path.as_str()]).ok()?;
    output.stdout.split_whitespace().next()?.parse().ok()
}

/// The kernel's own report of what this array is currently syncing at, in
/// KB/s -- the source the capability estimate is learned from. `None` when
/// nothing is syncing (md writes `none`), under dry-run, or on a read
/// failure: an honest "no observation this tick", never a zero.
pub fn read_sync_speed_kb(runner: &dyn CommandRunner, md_name: &str) -> Option<u64> {
    if runner.is_dry_run() {
        return None;
    }
    let path = format!("/sys/block/{md_name}/md/sync_speed");
    let output = runner.run("cat", &[path.as_str()]).ok()?;
    output.stdout.split_whitespace().next()?.parse().ok()
}

/// Read the host-wide `/proc/sys/dev/raid/speed_limit_max`'s CURRENT value
/// via `CommandRunner` (never raw `std::fs` -- see this module's doc
/// comment). `None` under dry-run (nothing real to read) or any read/parse
/// failure.
///
/// Public because that parameter is HOST-WIDE and this crate is not the only
/// layer that has to care about it: `shr-orchestrate` reads it once, before
/// the first write of an operation on a kernel without per-array attributes,
/// so the operator's own prior value can be put back afterward (see
/// `StateFile::saved_speed_limit_max_kb`). The `None`-on-failure contract is
/// what makes that safe -- a caller can never mistake "could not read it"
/// for a real number worth saving.
pub fn read_speed_limit_max(runner: &dyn CommandRunner) -> Option<u64> {
    read_limit(runner, "", LimitScope::HostWide, "max")
}

/// Write the host-wide `/proc/sys/dev/raid/speed_limit_max` directly, with
/// none of `ThrottleController`'s clamping or change-tracking. The one
/// caller that wants exactly this is `restore_speed_limit_if_idle`, putting
/// the operator's saved value back on the `HostWide` fallback path.
pub fn write_speed_limit_max(runner: &dyn CommandRunner, kb: u64) -> Result<(), ExecError> {
    write_sysfs(
        runner,
        &limit_path("", LimitScope::HostWide, "max"),
        &kb.to_string(),
    )
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

    /// Deliberately inside the hysteresis band under `Balanced` (below every
    /// decrease threshold, above every increase one), so a test that changes
    /// one signal is testing that signal.
    fn metrics() -> ThrottleMetrics {
        ThrottleMetrics {
            cpu_load: Some(0.75),
            cpu_count: Some(4),
            io_wait_pct: Some(27.0),
            user_io_latency_p99_ms: Some(90),
            disk_temp_max: Some(40),
            smart_delta_reallocated: Some(0),
            first_sample: false,
        }
    }

    fn decide(priority: SyncPriority, m: ThrottleMetrics) -> ThrottleTick {
        let sampler = some_metrics(m);
        ReshapeThrottle::new(priority, &sampler).tick()
    }

    #[test]
    fn tick_holds_when_metrics_are_in_the_normal_band() {
        assert_eq!(
            decide(SyncPriority::Balanced, metrics()).decision,
            ThrottleDecision::Hold
        );
    }

    #[test]
    fn tick_emergency_brakes_when_smart_reallocated_count_increases() {
        let tick = decide(
            SyncPriority::Balanced,
            ThrottleMetrics {
                smart_delta_reallocated: Some(1),
                ..metrics()
            },
        );
        assert_eq!(tick.decision, ThrottleDecision::EmergencyBrake);
        assert!(tick.reason.contains("smart reallocated"), "{}", tick.reason);
    }

    #[test]
    fn tick_emergency_brakes_when_disk_temperature_hits_the_threshold() {
        let tick = decide(
            SyncPriority::Balanced,
            ThrottleMetrics {
                disk_temp_max: Some(50),
                ..metrics()
            },
        );
        assert_eq!(tick.decision, ThrottleDecision::EmergencyBrake);
        assert_eq!(tick.reason, "disk_temp 50C >= 50C");
    }

    #[test]
    fn tick_decreases_when_cpu_load_exceeds_threshold() {
        assert_eq!(
            decide(
                SyncPriority::Balanced,
                ThrottleMetrics {
                    cpu_load: Some(0.9),
                    ..metrics()
                }
            )
            .decision,
            ThrottleDecision::Decrease(DECREASE_FACTOR)
        );
    }

    #[test]
    fn tick_decreases_when_user_io_latency_exceeds_threshold() {
        assert_eq!(
            decide(
                SyncPriority::Balanced,
                ThrottleMetrics {
                    user_io_latency_p99_ms: Some(150),
                    ..metrics()
                }
            )
            .decision,
            ThrottleDecision::Decrease(DECREASE_FACTOR)
        );
    }

    #[test]
    fn danger_signal_wins_over_an_otherwise_idle_system() {
        assert_eq!(
            decide(
                SyncPriority::Balanced,
                ThrottleMetrics {
                    cpu_load: Some(0.1),
                    io_wait_pct: Some(2.0),
                    smart_delta_reallocated: Some(3),
                    ..metrics()
                }
            )
            .decision,
            ThrottleDecision::EmergencyBrake
        );
    }

    /// No monitoring configured at all (`sample() == None`) must hold,
    /// not brake or accelerate -- there is no signal to react to.
    #[test]
    fn tick_holds_when_the_sampler_reports_no_monitoring_at_all() {
        let sampler = FixedSampler(None);
        assert_eq!(
            ReshapeThrottle::new(SyncPriority::Balanced, &sampler)
                .tick()
                .decision,
            ThrottleDecision::Hold
        );
    }

    /// The project's real `LiveMetricsSampler` never measures user IO
    /// latency at all. Its permanent absence must not, by itself, force
    /// every live-sampled tick to decelerate forever.
    #[test]
    fn a_missing_user_io_latency_signal_alone_does_not_force_a_decrease() {
        assert_eq!(
            decide(
                SyncPriority::Balanced,
                ThrottleMetrics {
                    user_io_latency_p99_ms: None,
                    ..metrics()
                }
            )
            .decision,
            ThrottleDecision::Hold
        );
    }

    /// D2: `max` sets its contention thresholds to saturating values, which
    /// is the same statement as "this profile does not consult them". An
    /// unreadable one must therefore hold, not brake.
    #[test]
    fn max_holds_when_contention_signals_are_unreadable() {
        for m in [
            ThrottleMetrics {
                cpu_load: None,
                ..metrics()
            },
            ThrottleMetrics {
                io_wait_pct: None,
                ..metrics()
            },
        ] {
            let tick = decide(SyncPriority::Max, m);
            assert_eq!(tick.decision, ThrottleDecision::Hold, "{}", tick.reason);
        }
    }

    #[test]
    fn bounded_profiles_still_brake_when_contention_signals_are_unreadable() {
        for priority in [SyncPriority::Background, SyncPriority::Balanced] {
            assert_eq!(
                decide(
                    priority,
                    ThrottleMetrics {
                        io_wait_pct: None,
                        ..metrics()
                    }
                )
                .decision,
                ThrottleDecision::Decrease(DECREASE_FACTOR),
                "{priority:?} did not brake"
            );
        }
    }

    /// `EmergencyBrake` depends on temperature, so not knowing it decreases
    /// under every profile -- including `max`, where the decrease is a
    /// deliberate no-op (its own floor is the unbounded sentinel).
    #[test]
    fn every_profile_brakes_when_temperature_is_unreadable() {
        for priority in [
            SyncPriority::Background,
            SyncPriority::Balanced,
            SyncPriority::Max,
        ] {
            let tick = decide(
                priority,
                ThrottleMetrics {
                    disk_temp_max: None,
                    ..metrics()
                },
            );
            assert_eq!(
                tick.decision,
                ThrottleDecision::Decrease(DECREASE_FACTOR),
                "{priority:?} did not brake"
            );
            assert_eq!(tick.reason, "disk temperature unreadable");
        }
    }

    /// D2: `smart_delta_reallocated` is `None` on an operation's first tick
    /// by construction (no previous total to diff against). Braking on it
    /// was noise, not safety.
    #[test]
    fn first_sample_missing_smart_delta_does_not_decrease() {
        assert_eq!(
            decide(
                SyncPriority::Balanced,
                ThrottleMetrics {
                    smart_delta_reallocated: None,
                    first_sample: true,
                    ..metrics()
                }
            )
            .decision,
            ThrottleDecision::Hold
        );
    }

    #[test]
    fn a_later_unreadable_smart_signal_still_decreases() {
        let tick = decide(
            SyncPriority::Max,
            ThrottleMetrics {
                smart_delta_reallocated: None,
                first_sample: false,
                ..metrics()
            },
        );
        assert_eq!(tick.decision, ThrottleDecision::Decrease(DECREASE_FACTOR));
        assert_eq!(tick.reason, "smart unreadable");
    }

    /// D3: the old `Increase` condition (`cpu_load < 0.5 && io_wait < 15%`)
    /// was measured while the operation being governed saturated the member
    /// disks, so it could never be satisfied and the throttle was a one-way
    /// ratchet. Increase is now the decrease thresholds scaled by 0.8.
    #[test]
    fn increase_and_decrease_thresholds_leave_a_hysteresis_band() {
        let below = ThrottleMetrics {
            cpu_load: Some(0.6),
            io_wait_pct: Some(20.0),
            user_io_latency_p99_ms: Some(50),
            ..metrics()
        };
        assert_eq!(
            decide(SyncPriority::Balanced, below).decision,
            ThrottleDecision::Increase(INCREASE_FACTOR),
            "24% io wait is the balanced increase threshold; 20% must climb"
        );

        let inside = ThrottleMetrics {
            io_wait_pct: Some(27.0),
            ..below
        };
        assert_eq!(
            decide(SyncPriority::Balanced, inside).decision,
            ThrottleDecision::Hold,
            "between 24% and 30% must neither climb nor brake"
        );

        let above = ThrottleMetrics {
            io_wait_pct: Some(31.0),
            ..below
        };
        assert_eq!(
            decide(SyncPriority::Balanced, above).decision,
            ThrottleDecision::Decrease(DECREASE_FACTOR)
        );
    }

    /// Under `max` the contention thresholds are saturating, so the increase
    /// condition is trivially satisfied and `max` climbs back to its ceiling
    /// after any transient temperature brake.
    #[test]
    fn max_climbs_back_after_a_transient_brake() {
        assert_eq!(
            decide(
                SyncPriority::Max,
                ThrottleMetrics {
                    cpu_load: Some(9.0),
                    io_wait_pct: Some(99.0),
                    ..metrics()
                }
            )
            .decision,
            ThrottleDecision::Increase(INCREASE_FACTOR)
        );
    }

    #[test]
    fn max_priority_thresholds_never_trigger_decrease_from_load_alone() {
        assert_eq!(
            decide(
                SyncPriority::Max,
                ThrottleMetrics {
                    cpu_load: Some(999.0),
                    io_wait_pct: Some(999.0),
                    user_io_latency_p99_ms: Some(u64::MAX - 1),
                    ..metrics()
                }
            )
            .decision,
            ThrottleDecision::Increase(INCREASE_FACTOR)
        );
    }

    #[test]
    fn balanced_and_background_derive_limits_from_the_capability_estimate() {
        let c = Some(400_000);
        assert_eq!(
            SyncPriority::Balanced.limits(c),
            SyncLimits {
                max_kb: 300_000,
                min_kb: 140_000
            }
        );
        assert_eq!(
            SyncPriority::Background.limits(c),
            SyncLimits {
                max_kb: 140_000,
                min_kb: 80_000
            }
        );
    }

    #[test]
    fn max_is_unbounded_in_both_directions_and_needs_no_estimate() {
        for c in [None, Some(50_000), Some(4_000_000)] {
            assert_eq!(
                SyncPriority::Max.limits(c),
                SyncLimits {
                    max_kb: UNBOUNDED_SPEED_KB,
                    min_kb: UNBOUNDED_SPEED_KB
                }
            );
        }
    }

    #[test]
    fn bootstrap_constants_apply_only_until_the_first_estimate() {
        assert_eq!(
            SyncPriority::Balanced.limits(None),
            SyncLimits {
                max_kb: 150_000,
                min_kb: 60_000
            }
        );
        assert_eq!(
            SyncPriority::Background.limits(None),
            SyncLimits {
                max_kb: 60_000,
                min_kb: 25_000
            }
        );
        assert_ne!(
            SyncPriority::Balanced.limits(Some(400_000)),
            SyncPriority::Balanced.limits(None)
        );
    }

    #[test]
    fn background_floor_never_falls_below_the_streaming_bound() {
        // 0.20 * 20000 = 4000, which would stutter rather than stream.
        let limits = SyncPriority::Background.limits(Some(20_000));
        assert_eq!(limits.min_kb, STREAM_FLOOR_ABS_KB);
        assert!(limits.max_kb >= limits.min_kb);
    }

    #[test]
    fn capability_estimate_rises_on_observation_and_decays_when_uncapped() {
        let est = CapabilityEstimate::default().observe(Some(180_000), 300_000);
        assert_eq!(
            est.kb,
            Some(180_000),
            "any observation replaces an absent estimate"
        );

        let est = est.observe(Some(240_000), 300_000);
        assert_eq!(
            est.kb,
            Some(240_000),
            "a higher observation replaces the estimate"
        );

        // Below 0.7 of the ceiling, so the ceiling is not the binding
        // constraint -- but two ticks are noise, not the array.
        let est = est
            .observe(Some(100_000), 300_000)
            .observe(Some(100_000), 300_000);
        assert_eq!(est.kb, Some(240_000));
        let est = est.observe(Some(100_000), 300_000);
        assert_eq!(est.kb, Some(226_000), "0.9 * 240000 + 0.1 * 100000");
    }

    #[test]
    fn an_observation_at_the_ceiling_never_decays_the_estimate() {
        let est = CapabilityEstimate::new(Some(300_000), 0);
        let capped = (0..5).fold(est, |e, _| e.observe(Some(290_000), 300_000));
        assert_eq!(capped.kb, Some(300_000));
    }

    #[test]
    fn an_absent_or_zero_observation_leaves_the_estimate_alone() {
        let est = CapabilityEstimate::new(Some(300_000), 2);
        assert_eq!(est.observe(None, 300_000), est);
        assert_eq!(est.observe(Some(0), 300_000), est);
    }

    /// Records every command like `DryRunRunner`, but reports
    /// `is_dry_run() == false` -- `ThrottleController` is meant to run
    /// against a REAL array, and the point of these tests is proving a write
    /// actually happens. `cat_response`, if set, is what any `cat`
    /// invocation returns.
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
            Self {
                commands: Mutex::new(Vec::new()),
                cat_response: Some(response.into()),
            }
        }
    }
    impl CommandRunner for SpyRunner {
        fn run(&self, program: &str, args: &[&str]) -> Result<CommandOutput, Err> {
            self.commands
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            let stdout = if program == "cat" {
                self.cat_response.clone().unwrap_or_default()
            } else {
                String::new()
            };
            Ok(CommandOutput {
                stdout,
                stderr: String::new(),
            })
        }
        fn is_dry_run(&self) -> bool {
            false
        }
    }

    fn controller<'a>(
        runner: &'a SpyRunner,
        priority: SyncPriority,
        capability: Option<u64>,
    ) -> ThrottleController<'a> {
        ThrottleController::new(runner, "md0", priority, capability, LimitScope::PerArray)
    }

    /// D1: writing only a ceiling left the sync floor at 1 MB/s, and the
    /// kernel reduces the sync rate toward that floor whenever non-sync IO
    /// touches the members -- which on a live NAS is always.
    #[test]
    fn max_writes_the_unbounded_sentinel_to_both_attributes() {
        let runner = SpyRunner::default();
        controller(&runner, SyncPriority::Max, Some(400_000))
            .apply_initial()
            .unwrap();
        let commands = runner.commands();
        for leaf in ["sync_speed_min", "sync_speed_max"] {
            assert!(
                commands.iter().any(
                    |c| c.contains(&format!("/md/{leaf}")) && c.contains(&UNBOUNDED_SPEED_KB.to_string())
                ),
                "{leaf} did not get the unbounded sentinel: {commands:?}"
            );
        }
    }

    #[test]
    fn apply_initial_writes_both_limits_and_the_stripe_cache_per_array() {
        let runner = SpyRunner::default();
        controller(&runner, SyncPriority::Background, Some(400_000))
            .apply_initial()
            .unwrap();
        let commands = runner.commands();
        assert!(commands
            .iter()
            .any(|c| c.contains("/sys/block/md0/md/sync_speed_min") && c.contains("80000")));
        assert!(commands
            .iter()
            .any(|c| c.contains("/sys/block/md0/md/sync_speed_max") && c.contains("140000")));
        assert!(commands
            .iter()
            .any(|c| c.contains("stripe_cache_size") && c.contains("md0")));
        assert!(
            !commands.iter().any(|c| c.contains("/proc/sys/dev/raid")),
            "per-array scope must not touch the host-wide parameters: {commands:?}"
        );
    }

    #[test]
    fn the_host_wide_fallback_writes_the_same_limits_to_the_global_parameters() {
        let runner = SpyRunner::default();
        let mut ctrl = ThrottleController::new(
            &runner,
            "md0",
            SyncPriority::Balanced,
            Some(400_000),
            LimitScope::HostWide,
        );
        ctrl.apply_initial().unwrap();
        let commands = runner.commands();
        assert!(commands
            .iter()
            .any(|c| c.contains("/proc/sys/dev/raid/speed_limit_min") && c.contains("140000")));
        assert!(commands
            .iter()
            .any(|c| c.contains("/proc/sys/dev/raid/speed_limit_max") && c.contains("300000")));
    }

    /// D4/D3 together: the reported multi-day reshape was a `Balanced`
    /// profile decaying 0.7 per tick to an absolute 10 MB/s floor it could
    /// never climb back from.
    #[test]
    fn decrease_is_floored_at_the_profile_minimum_not_at_ten_megabytes() {
        let runner = SpyRunner::default();
        let mut ctrl = controller(&runner, SyncPriority::Balanced, Some(400_000));
        for _ in 0..20 {
            ctrl.apply(ThrottleDecision::Decrease(DECREASE_FACTOR)).unwrap();
        }
        assert_eq!(ctrl.current_speed_kb(), 140_000);
    }

    #[test]
    fn decrease_under_max_cannot_slow_the_operation_at_all() {
        let runner = SpyRunner::default();
        let mut ctrl = controller(&runner, SyncPriority::Max, Some(400_000));
        for _ in 0..5 {
            ctrl.apply(ThrottleDecision::Decrease(DECREASE_FACTOR)).unwrap();
        }
        assert_eq!(ctrl.current_speed_kb(), UNBOUNDED_SPEED_KB);
    }

    #[test]
    fn increase_never_exceeds_the_profiles_own_ceiling() {
        let runner = SpyRunner::default();
        let mut ctrl = controller(&runner, SyncPriority::Background, Some(400_000));
        for _ in 0..20 {
            ctrl.apply(ThrottleDecision::Increase(INCREASE_FACTOR)).unwrap();
        }
        assert_eq!(ctrl.current_speed_kb(), 140_000);
    }

    #[test]
    fn emergency_brake_targets_the_background_floor_under_every_profile() {
        for priority in [
            SyncPriority::Background,
            SyncPriority::Balanced,
            SyncPriority::Max,
        ] {
            let runner = SpyRunner::default();
            let mut ctrl = controller(&runner, priority, Some(400_000));
            let after = ctrl.apply(ThrottleDecision::EmergencyBrake).unwrap();
            assert_eq!(after, 80_000, "{priority:?} braked to the wrong target");
            assert!(runner
                .commands()
                .iter()
                .any(|c| c.contains("sync_speed_max") && c.contains("80000")));
        }
    }

    #[test]
    fn hold_does_not_issue_a_redundant_kernel_write() {
        let runner = SpyRunner::default();
        let mut ctrl = controller(&runner, SyncPriority::Balanced, Some(400_000));
        let before_count = runner.commands().len();
        ctrl.apply(ThrottleDecision::Hold).unwrap();
        assert_eq!(runner.commands().len(), before_count);
    }

    #[test]
    fn clear_returns_the_array_to_the_host_wide_parameters() {
        let runner = SpyRunner::default();
        controller(&runner, SyncPriority::Max, None).clear().unwrap();
        let commands = runner.commands();
        for leaf in ["sync_speed_min", "sync_speed_max"] {
            assert!(
                commands
                    .iter()
                    .any(|c| c.contains(&format!("/md/{leaf}")) && c.contains("system")),
                "{leaf} was not cleared: {commands:?}"
            );
        }
    }

    #[test]
    fn clear_is_a_no_op_on_the_host_wide_fallback() {
        let runner = SpyRunner::default();
        ThrottleController::new(&runner, "md0", SyncPriority::Max, None, LimitScope::HostWide)
            .clear()
            .unwrap();
        assert!(runner.commands().is_empty());
    }

    #[test]
    fn resume_seeds_current_speed_from_the_arrays_real_ceiling() {
        let runner = SpyRunner::with_cat_response("142000 (local)\n");
        let ctrl = ThrottleController::resume(
            &runner,
            "md0",
            SyncPriority::Balanced,
            Some(400_000),
            LimitScope::PerArray,
        );
        assert_eq!(
            ctrl.current_speed_kb(),
            142_000,
            "must read the REAL current ceiling, including the `(local)` suffix md appends"
        );
    }

    #[test]
    fn resume_falls_back_to_the_profiles_ceiling_when_the_kernel_value_is_unreadable() {
        let runner = SpyRunner::default(); // cat returns "" -- unparseable
        let ctrl = ThrottleController::resume(
            &runner,
            "md0",
            SyncPriority::Background,
            Some(400_000),
            LimitScope::PerArray,
        );
        assert_eq!(ctrl.current_speed_kb(), 140_000);
    }

    #[test]
    fn sync_speed_reads_none_as_no_observation_not_as_zero() {
        let runner = SpyRunner::with_cat_response("none\n");
        assert_eq!(read_sync_speed_kb(&runner, "md0"), None);
        let runner = SpyRunner::with_cat_response("183296\n");
        assert_eq!(read_sync_speed_kb(&runner, "md0"), Some(183_296));
    }

    #[test]
    fn probing_falls_back_to_host_wide_when_the_per_array_attribute_is_unreadable() {
        let runner = SpyRunner::default();
        assert_eq!(probe_limit_scope(&runner, "md0"), LimitScope::HostWide);
        let runner = SpyRunner::with_cat_response("200000 (system)\n");
        assert_eq!(probe_limit_scope(&runner, "md0"), LimitScope::PerArray);
    }

    #[test]
    fn null_sampler_holds_under_every_priority_profile() {
        for priority in [
            SyncPriority::Background,
            SyncPriority::Balanced,
            SyncPriority::Max,
        ] {
            assert_eq!(
                ReshapeThrottle::new(priority, &NullMetricsSampler)
                    .tick()
                    .decision,
                ThrottleDecision::Hold,
                "{priority:?} did not hold"
            );
        }
    }

    #[test]
    fn priority_round_trips_through_its_string_form() {
        for priority in [
            SyncPriority::Background,
            SyncPriority::Balanced,
            SyncPriority::Max,
        ] {
            assert_eq!(SyncPriority::parse(priority.as_str()), Some(priority));
        }
        assert_eq!(SyncPriority::parse("fastest"), None);
    }
}
