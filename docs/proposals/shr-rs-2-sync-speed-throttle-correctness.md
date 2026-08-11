# Sync Speed Profiles and Throttle Correctness - Spec Proposal

| Item       | Detail                           |
|------------|----------------------------------|
| Author     | heavycaffeiner(Dong Hyun Kim)    |
| Created    | 2026-08-11                       |
| Status     | Draft / In Review / **Implemented** |
| Reviewers  |                                  |

---

## 1. Summary

The three speed profiles are defined as absolute KB/s constants that mean
nothing on any particular machine, and the adaptive throttle that adjusts them
brakes work it was told not to brake and never lets go once it has braked. A
reshape started at the default profile decays from 100 MB/s to the 10 MB/s floor
within 14 minutes and stays there for the rest of a multi-day operation. A scrub
started with `--priority max` is still slowed by the kernel, because `max` writes
only a ceiling and leaves the sync floor at 1 MB/s.

This proposal replaces the constant-based profile model with one defined
relative to the array's measured sync capability, gives each profile a floor
high enough that the sync work streams rather than stutters, extends the model
from reshape to every md sync operation (resync, resilver, scrub), and fixes the
seven defects in the decision algorithm and the metrics that feed it.

Field report that prompted this: a 3-disk 4 TB SHR group grew to 4 disks and the
reshape ran for days rather than the expected 11 to 18 hours, and a subsequent
`--priority max` scrub was throttled anyway.

## 2. Background and Motivation

### 2.1 What already exists

Three speed profiles (`ReshapePriority::{Background, Balanced, Max}`,
`crates/shr-exec/src/throttle.rs:171`) are shared by two commands:
`expand --priority` (an mdadm reshape) and `fs scrub start --priority` (an
mdadm `check`). Each carries an initial speed, a ceiling, and a
`SafetyThresholds` set, all as absolute constants.

Two components act on them.

- `ReshapeThrottle::tick` (`throttle.rs:137`) is the pure decision function. It
  reads one `ThrottleMetrics` sample and returns `EmergencyBrake`,
  `Decrease(0.7)`, `Increase(1.2)`, or `Hold`.
- `ThrottleController` (`throttle.rs:243`) applies a decision to
  `/proc/sys/dev/raid/speed_limit_max`, clamped to
  `[RESHAPE_SPEED_FLOOR_KB, RESHAPE_SPEED_CEILING_KB]` = `[10000, 500000]` KB/s.

`LiveMetricsSampler` (`crates/shr-orchestrate/src/metrics.rs:26`) supplies the
sample from `/proc/loadavg`, two `/proc/stat` reads, and one `smartctl -j -H -A
-i` per member disk.

Ticking is external: `shr-rs-throttle-tick.timer` fires
`shr-rs internal reshape-throttle-tick` every two minutes, and
`OrchestrationEngine::tick_active_reshapes` (`engine.rs:4130`) sweeps every band
whose `sync_action` reads `reshape`. Scrubs are not ticked (`engine.rs:4147`);
a scrub's speed is set once by `scrub_start` (`engine.rs:2086`) and never
revisited. Resync after `create` and recovery after `replace_disk` are governed
by nothing at all.

### 2.2 The defects

**D1. `max` writes a 1 MB/s sync floor, so the kernel throttles it anyway.**

`ThrottleController::apply_initial` (`throttle.rs:295`) writes
`speed_limit_min = SPEED_LIMIT_MIN_DEFAULT_KB` (1000 KB/s) unconditionally, for
every profile including `Max`. `scrub_start` writes no floor at all
(`engine.rs:2005` records this as deliberate), so a scrub inherits whatever floor
is in place, which after any prior `expand` is that same 1000.

The md sync throttle reduces the sync rate toward `speed_limit_min` whenever
non-sync IO touches the member devices. On a live NAS there is always some: the
mounted Btrfs, smartd, Cockpit's own polling, any client. So `--priority max`
raises a ceiling the operation never reaches and pins the floor at 1 MB/s. This
is the direct cause of the reported "max scrub was still throttled".

**D2. An unreadable signal decays a `max` operation to the floor.**

`Max.thresholds()` (`throttle.rs:208`) sets `max_cpu_load`, `max_io_wait_pct`,
and `user_io_latency_ms` to their saturating values precisely so `over_threshold`
can never trip. But the adjacent `any_signal_unreadable` branch
(`throttle.rs:153`) is not profile-aware: it fires on `cpu_load`, `io_wait_pct`,
`disk_temp_max`, or `smart_delta_reallocated` being `None`, and returns
`Decrease(0.7)` regardless of profile.

`read_smart_signals` (`metrics.rs:166`) yields `None` for both SMART signals
whenever `smartctl` is absent, or the disks sit behind a controller needing an
explicit `-d` transport, or the drive reports no temperature attribute. And
`smart_delta_reallocated` is `None` on the first tick of every operation by
construction (`metrics.rs:81`, no previous total to diff against).

So on a host without working `smartctl`, a `--priority max` reshape decays
500000 KB/s by 0.7 every two minutes and reaches the 10000 floor after 11 ticks,
22 minutes in. The profile whose entire purpose is "do not brake this" brakes to
the slowest setting the system offers.

**D3. The throttle is a one-way ratchet.**

`Increase(1.2)` requires `cpu_load < 0.5` and `io_wait_pct < 15.0`
(`throttle.rs:161`). Both are measured while the operation being governed is
saturating the member disks, which is exactly when io wait is high. The recovery
condition cannot be satisfied by the system it is measuring. `Decrease` triggers
above 30% io wait and `Increase` requires below 15%, so anything in between
holds, and a reshape sits above 30 essentially always. Once any decrease lands,
the speed never returns for the remainder of the operation.

**D4. `cpu_load` is a raw load average compared against a per-core threshold.**

`read_cpu_load` (`metrics.rs:105`) returns `/proc/loadavg`'s first field
verbatim. `tick` compares it to `max_cpu_load: 0.85` (`throttle.rs:149`), a
number whose shape is a per-core utilisation fraction.

Linux load average counts uninterruptible-sleep tasks. During a reshape the md
sync thread plus every process blocked on the array put the 1-minute average at
roughly 2 to 6 on any real machine. The comparison against 0.85 is therefore
true on effectively every tick under both `Background` and `Balanced`.

Combined with D3, this is the mechanism behind the reported multi-day reshape:
`Balanced` starts at 100000 KB/s, decays 0.7 per tick, reaches the 10000 floor in
7 ticks (14 minutes), and cannot climb back. At 10 MB/s a 4 TB member takes about
106 hours.

**D5. The scheduled scrub cannot carry a priority at all.**

`write_scrub_timer_unit` (`crates/shr-state/src/conf.rs:213`) emits
`ExecStart={exe} fs scrub start --name {name}` with no `--priority`, and
`PolicyFile` (`crates/shr-state/src/policy.rs:32`) has no field that could hold
one. Every scheduled scrub therefore takes the "touch no kernel parameter" path
and runs under whatever `speed_limit_max` happens to be set, which after a
throttled reshape is 10000. An operator who selects `max` in Cockpit gets it for
that one manual run and never again.

**D6. The profiles are absolute constants with no relation to the machine.**

20000, 100000, and 500000 KB/s are the same three numbers on a 2-disk SATA array
and on a 12-disk SAS array. On the first, `Balanced`'s 100000 is above anything
the array can do, so the profile is indistinguishable from `Max`. On the second
it is a hard brake. Neither profile means what its name says on any specific
machine, and `Max` is not the maximum: `apply` clamps every profile with
`ceiling_kb.unwrap_or(RESHAPE_SPEED_CEILING_KB).min(RESHAPE_SPEED_CEILING_KB)`
(`throttle.rs:321`), so `Max` and `Balanced` share the same 500 MB/s effective
ceiling despite `Max.ceiling_kb()` returning `None` with the comment "unlimited"
(`throttle.rs:186`).

**D7. Nothing reports the effective limit or the reason for it.**

There is no way to learn that a scrub is running at the floor short of reading
`/proc/sys/dev/raid/speed_limit_max` by hand. `status` reports each band's
`sync_action` but neither the current cap, the profile in force, nor the last
throttle decision and what triggered it.

A related gap: `restore_speed_limit_if_idle` (`engine.rs:4050`) is the only thing
that hands the operator's original value back, and it runs from
`tick_active_reshapes` and `reconcile`. On a host where `schedule install` was
never run there is no timer, so a floor written by one operation persists
indefinitely and silently governs every later one.

## 3. Proposal: capability-relative profiles

### 3.1 What each profile means

The three profiles are redefined as intents, not numbers.

**Max.** The system's maximum. No artificial limit of any kind on the sync
operation. The operator has accepted that everyday IO will be slower for the
duration.

**Balanced.** Sized against what this machine can actually do, moderate, but
still claiming a generous share. This is the default and should feel like the
array is working hard, not like it has been put to sleep.

**Background.** Limited only as far as it can be while the sync work still
streams. A limit set too aggressively does not produce a slow, steady sync: it
produces a stuttering one, where md bursts and then backs off, the head leaves
the sync region, and both the sync and the foreground work pay the seek. The
floor here exists to prevent that, not to be small for its own sake.

### 3.2 Per-array limits instead of host-wide ones

md exposes `sync_speed_min` and `sync_speed_max` per array under
`/sys/block/<md>/md/`. They shadow the host-wide
`/proc/sys/dev/raid/speed_limit_{min,max}` for that array alone, and writing the
literal `system` clears the local value and returns the array to the global one.

Every write this proposal makes goes to the per-array attributes. That removes
today's host-wide caveat (`engine.rs:2001`, a second group's scrub silently
overwrites the first group's setting), makes a per-band profile actually mean
something, and makes teardown exact: clearing is a write of `system`, not a
restore of a remembered number.

The host-wide save-and-restore machinery (`saved_speed_limit_max_kb`,
`remember_speed_limit_max`, `restore_speed_limit_if_idle`) is kept only as the
fallback path, for a kernel where the per-array attributes are absent. Presence
is probed once per band and recorded; the fallback keeps today's behaviour
exactly.

**Open item, since closed.** Both attributes exist on the Rocky 10.2 guest and
accept `system`; see 9.1 for the measurement. They report their origin
alongside the value (`200000 (local)`), so every reader parses the first field
only.

### 3.3 Measuring capability

`C` is the array's sync capability in KB/s, per band, measured rather than
assumed. The source is `/sys/block/<md>/md/sync_speed`, the kernel's own report
of the current sync rate, read once per tick.

Two rules maintain the estimate:

- Any observation above the current estimate replaces it. The array demonstrably
  sustained that rate, so capability is at least that.
- When the ceiling is not the binding constraint, meaning the observed speed sits
  below 0.7 of the ceiling for three consecutive ticks, the estimate decays
  toward the observation at 0.9 old plus 0.1 new. This lets a capability learned
  on a healthy array fall back when the array is degraded, a member is failing,
  or the operation has reached the slower inner tracks.

The estimate is persisted per band as `sync_capability_kb` with an observation
timestamp, so the next operation starts from it instead of relearning. It is
discarded when band membership changes, since adding or removing a member changes
what the array can do.

There is no dedicated calibration phase. A burst to full speed at the start of a
`background` operation is exactly the disruption that profile exists to avoid, so
the estimate is learned by the same control loop that uses it.

### 3.4 Derived limits

| Profile    | `sync_speed_max` | `sync_speed_min` | Bootstrap max/min |
|------------|------------------|------------------|-------------------|
| Max        | unbounded        | unbounded        | same, no estimate needed |
| Balanced   | 0.75 * C         | 0.35 * C         | 150000 / 60000    |
| Background | 0.35 * C         | 0.20 * C         | 60000 / 25000     |

"Unbounded" is a single named constant written to both attributes, set above any
rate the target hardware can reach. Writing a floor above what the array can
actually sustain is harmless: md simply never backs off, which is precisely what
`Max` means. This is why `Max` needs no capability estimate to set its limits,
only to have a target should an emergency brake fire.

The bootstrap column applies only on the first operation of a band, before any
estimate exists, and is overwritten by the derived value at the first tick that
yields one. They are the last absolute constants in the model and are documented
as such.

`Background`'s floor is additionally bounded below by `STREAM_FLOOR_ABS_KB`
(15000). Its only job is to keep a nonsensical estimate from producing a
stuttering floor; the real anti-stutter guarantee is the 0.20 fraction.

The six fractions live in one table in `throttle.rs` so tuning any profile is a
one-line change with a test that pins it.

These six are settled, not placeholders. `Balanced` at 0.75 and 0.35 is the
deliberate reading of "moderate but not timid": it leaves real headroom for
everyday IO while still claiming most of the array, and an operator who wants the
array to disappear into the background has `Background` for that. Any future
change to a fraction is a measurement brought back to this document, per §4.

### 3.5 Adaptive decisions within a profile

The profile sets the band the throttle operates in. The adaptive loop moves
within that band and can no longer leave it.

- `Decrease` scales the current speed down, floored at the profile's own
  `sync_speed_min`, not at an absolute 10000. `Background` can therefore never
  fall below the rate at which it still streams, which is the structural fix for
  the reported multi-day reshape.
- `Increase` scales up, capped at the profile's own `sync_speed_max`.
- `EmergencyBrake` targets `Background`'s floor (0.20 * C) under every profile,
  including `Max`. Braking to a rate that stutters helps nothing, and the
  notification is what actually matters at that point.

Three fixes to the decision function itself:

**Unreadable signals become profile-aware (D2).** `disk_temp_max` and
`smart_delta_reallocated` are safety-critical: `EmergencyBrake` depends on them,
so not knowing them decreases under every profile including `Max`. `cpu_load`
and `io_wait_pct` are contention signals: unreadable means decrease under
`Background` and `Balanced`, and hold under `Max`, because `Max` has already
declared those two irrelevant by setting their thresholds to saturating values.
Braking `Max` on a signal `Max` does not consult is incoherent.

`smart_delta_reallocated == None` on an operation's first tick is expected, not a
gap, and is suppressed for that tick only. A later failed read still brakes. This
needs a `first_sample: bool` on `ThrottleMetrics`, set by `LiveMetricsSampler`
from its own `previous_smart_total`.

**Symmetric recovery with hysteresis (D3).** The absolute `Increase` constants
are replaced by the `Decrease` thresholds scaled by 0.8. Increase when every
measured contention signal sits below 0.8 of its own threshold. Under `Balanced`
that is increase below 24% io wait and below 0.68 normalised load, decrease above
30% and 0.85, hold between. Under `Max` the contention thresholds are saturating,
so the condition is trivially satisfied and `Max` climbs back to its ceiling
after any transient temperature brake, which is what the profile should mean.

**Normalised CPU load (D4).** `read_cpu_load` divides the 1-minute load average
by the online CPU count, read once from `/proc/cpuinfo`. `max_cpu_load` then
means what its value implies: 0.85 is 85% of the machine. `ThrottleMetrics`
carries the divisor so a report can show both figures. A CPU count that cannot be
read yields `None`, which routes into the contention-signal rule above rather
than silently using the raw value.

### 3.6 Every sync operation, not just reshape

`tick_active_reshapes` becomes `tick_active_sync` and acts on every band whose
`sync_action` is one of `reshape`, `check`, `repair`, `resync`, or `recover`,
rather than `reshape` alone. The capability estimate is a closed loop, so a scrub
cannot keep the set-once model and still be capability-relative.

This also closes two silent gaps: the resync after `create` and the recovery
after `replace_disk` are currently governed by no profile at all.

`StateBand::reshape_priority` is renamed `sync_priority` and is set by every
operation that starts a sync, not just `execute_grow`. Migration reads the old
field name when the new one is absent. Default profile when the caller specified
none:

| Operation                          | Default      |
|------------------------------------|--------------|
| `expand`                           | `balanced` (unchanged) |
| `fs scrub start` with no `--priority` | touch nothing (unchanged) |
| scheduled scrub                    | policy value, see 3.7 |
| resync after `create`              | `balanced` (new)  |
| recovery after `replace_disk`      | `balanced` (new)  |

`fs scrub start` with no flag keeps meaning "change no kernel parameter", because
that is a real choice the Cockpit dialog offers and removing it would be a
behaviour change nobody asked for.

### 3.7 Scheduled scrub priority (D5)

Add to `PolicyFile`:

```toml
[scrub]
priority = "balanced"   # "background" | "balanced" | "max" | omitted
```

Omitted keeps today's meaning exactly. `write_scrub_timer_unit` appends
`--priority {value}` when set. The value is validated against the same
three-variant parse `shr-cli` uses, and an unrecognised string is a load error
rather than a silent fallback, because silently scrubbing at the wrong speed is
the failure this proposal is about.

Cockpit's schedule dialog gains the selector, reusing the existing
`ReshapePriority` union in `cockpit/src/actions.ts:111`.

### 3.8 Reporting (D7)

Per band, persisted in `StateBand` with `#[serde(default)]`:

- `sync_capability_kb: Option<u64>` and its observation timestamp
- `last_throttle_decision: Option<String>` (`emergency-brake`, `decrease`,
  `increase`, `hold`)
- `last_throttle_reason: Option<String>`, the specific trigger, for example
  `disk_temp 52C >= 50C` or `smart unreadable`
- `last_throttle_speed_kb: Option<u64>`

`status` and `status --json` report, per band, the live `sync_speed`, the
effective `sync_speed_min`/`sync_speed_max`, the profile in force, the capability
estimate, and the three fields above. The Cockpit band panel shows the current
rate against the estimate and, when the last decision was not `hold`, the reason.

`status` also warns when a host-wide saved value is set while every band is idle,
which is the "a floor was left behind and no timer exists to restore it" state,
and names the command that fixes it.

### 3.9 Naming

`ReshapePriority` becomes `SyncPriority` and `crates/shr-exec/src/throttle.rs`
keeps its name. The type now governs four kinds of md sync activity and the old
name would be actively misleading in the scrub and resilver paths. The CLI flag
stays `--priority` on both commands, so this is internal only.

## 4. Non-goals

- Throttling Btrfs scrub. These attributes govern md sync threads only.
- Adding a real `user_io_latency_p99_ms` source. Still unmeasured, still used
  opportunistically, unchanged by this proposal.
- `stripe_cache_size` tuning beyond today's fixed 4096.
- A user-configurable fraction table. The six fractions are constants with a
  pinning test. If a real machine needs different ones, that is a measurement to
  bring back here, not a knob to ship.

## 5. Implementation plan

**Phase 1, per-array limits and profile semantics (D1, D6).** Probe the sysfs
attributes on the guest first, per 3.2's open item. `throttle.rs` gains the
capability estimate, the fraction table, and the unbounded sentinel; `engine.rs`
writes per-array attributes; `schema.rs` gains `sync_capability_kb`.

**Phase 2, decision correctness (D2, D3, D4).** `throttle.rs` and `metrics.rs`
only. Pure functions, no new IO surface.

**Phase 3, every sync operation (3.6).** `engine.rs` rename and predicate
widening, `schema.rs` field rename with migration, `scrub_start` and
`replace_disk` persist a profile.

**Phase 4, policy and reporting (D5, D7).** `policy.rs`, `conf.rs`, `shr-cli`,
`shr-tui`, `cockpit/src/actions.ts` and the band panel.

Phases 1 and 2 are what the deployed server needs. 3 and 4 are what keep the
problem from recurring unnoticed.

## 6. Testing

Unit, in `crates/shr-exec/src/throttle.rs` and
`crates/shr-orchestrate/tests/orchestrate.rs`:

- `max_writes_the_unbounded_sentinel_to_both_attributes`
- `balanced_and_background_derive_limits_from_the_capability_estimate`
- `bootstrap_constants_apply_only_until_the_first_estimate`
- `capability_estimate_rises_on_observation_and_decays_when_uncapped`
- `capability_estimate_is_discarded_when_band_membership_changes`
- `decrease_is_floored_at_the_profile_minimum_not_at_ten_megabytes`
- `background_floor_never_falls_below_the_streaming_bound`
- `emergency_brake_targets_the_background_floor_under_every_profile`
- `max_holds_when_contention_signals_are_unreadable`
- `every_profile_brakes_when_temperature_is_unreadable`
- `first_sample_missing_smart_delta_does_not_decrease`
- `increase_and_decrease_thresholds_leave_a_hysteresis_band`
- `cpu_load_is_normalised_by_core_count`
- `tick_acts_on_check_repair_resync_and_recover_as_well_as_reshape`
- `scrub_timer_unit_carries_the_policy_priority`

Smoke, on the guest, alongside the existing SM-THROTTLE-1 through 3:

- **SM-THROTTLE-4**: with `smartctl` made unavailable, a `--priority max`
  reshape's limits are unchanged after three ticks.
- **SM-THROTTLE-5**: after a forced decrease, the speed climbs again once the
  trigger clears, within four ticks, and never falls below the profile floor.
- **SM-THROTTLE-6**: `fs scrub start --priority max` writes the unbounded
  sentinel to the band's own `sync_speed_min` and `sync_speed_max`, and clears
  them to `system` when the check finishes.
- **SM-THROTTLE-7**: two groups scrubbing at different profiles simultaneously
  each keep their own limits, which the host-wide parameter could not express.

Per the smoke fixture's known behaviour, back up `state.toml` before running any
of these.

## 7. Risks

**`Max` with an unbounded floor starves foreground IO.** That is the profile's
stated contract and the operator selected it. md does not hard-starve; the block
layer still interleaves. Temperature and SMART braking still apply. The dialog
and `--help` text must say plainly what `max` gives up.

**The capability estimate can be learned wrong.** An estimate taken while the
array was busy with something else reads low, and every derived limit is then too
low. Mitigated by the "any higher observation replaces it" rule, which recovers
within one tick of the array actually going faster, and by discarding the
estimate on membership change. The estimate is reported in `status` so a wrong
one is visible rather than silent.

**Normalising load average changes behaviour on every existing deployment.** A
`Balanced` reshape that used to crawl at 10 MB/s will now run near capability.
That is the intent, but it is a large behavioural change from a version bump
alone, so it belongs in a minor release with a changelog entry naming the
symptom.

**Suppressing the first-tick SMART decrease narrows a real check.** The window is
one tick, and a genuine reallocation increase is still caught on tick two, two
minutes later. Braking every operation's opening tick on a condition that is
`None` by construction was noise, not safety.

**Per-array attributes may not exist on every supported kernel.** Handled by the
probe and fallback in 3.2, at the cost of keeping the host-wide restore path
alive rather than deleting it.

## 8. Field remediation

Independent of this proposal, on the currently deployed server:

```sh
cat /proc/sys/dev/raid/speed_limit_max    # 10000 confirms the decayed floor
cat /proc/sys/dev/raid/speed_limit_min
cat /sys/block/md0/md/sync_speed          # what the array is actually doing

sudo systemctl stop shr-rs-throttle-tick.timer
echo 200000 | sudo tee /proc/sys/dev/raid/speed_limit_max
echo 50000  | sudo tee /proc/sys/dev/raid/speed_limit_min
```

Stopping the timer halts the ratchet without touching the running operation.
Re-enable it once Phase 2 ships. Confirm the member drives are CMR before
attributing the remaining runtime to software: a 4 TB SMR drive reshapes in days
regardless of any setting here.

## 9. Verification

Measured on the Rocky 10.2 guest (`shr-dev`, kernel 7.1.6, 4 vCPU / 3 GB),
against the real `tank` group's `md0` (6-member RAID5 over Hyper-V virtual
disks) and against loopback fixtures for the smoke cases.

### 9.1 The open item in 3.2 is closed

Both per-array attributes exist and behave as the proposal assumed:

```
$ cat /sys/block/md0/md/sync_speed_max      # 200000 (system)
$ echo 200000 | sudo tee .../sync_speed_max # reads back: 200000 (local)
$ echo system | sudo tee .../sync_speed_max # reads back: 200000 (system)
$ cat /sys/block/md0/md/sync_speed          # none, when nothing is syncing
```

The `(local)`/`(system)` suffix is the only surprise: every reader parses the
first field only. `sync_speed` reads `none` between operations, which the
capability estimate treats as "no observation this tick", never as zero.

### 9.2 A real `balanced` scrub, sampled every tick

`fs scrub start --name tank --priority balanced`, then one throttle tick
every 12 seconds (production fires every two minutes; the short interval
compresses the same sequence into something observable in one sitting):

| time     | `sync_speed` | `sync_speed_min` | `sync_speed_max` | `sync_capability_kb` |
|----------|--------------|------------------|------------------|----------------------|
| 12:58:05 | 154624       | 36826            | 73500            | 105216               |
| 12:58:19 | 73618        | 36826            | 51450            | 105216               |
| 12:58:32 | 53883        | 36826            | 36826            | 105216               |
| 12:58:45 | 38035        | 36826            | 36826            | 105216               |
| 12:59:11 | 41851        | 36826            | 36826            | 105216               |
| 13:00:04 | 38019        | 36826            | 36826            | 105216               |

The band converged to a capability estimate of **105216 KB/s**, from which
`Balanced` derived `0.35 * C = 36826` as its floor: the exact value the
ceiling then stopped at and stayed at. Every tick decided `decrease`, because
Hyper-V virtual disks report no SMART temperature and an unreadable
safety-critical signal decreases under every profile by design.

That is the whole point. Under the previous model the same sequence of
decreases would have run 100000 -> 70000 -> ... -> 10000 and stayed at the
absolute floor for the rest of the operation; here it stops at the rate the
array still streams at, three and a half times higher, chosen from what this
particular array was measured doing rather than from a constant.

`status --detail` and the Cockpit band panel both reported it live:
`38.2 MB/s / 최대 약 105.2 MB/s (balanced)` with
`마지막 속도 조절: decrease (disk temperature unreadable)`, verified in a
browser against the real group.

### 9.3 Smoke cases

Run on the guest against loopback fixtures, judged by independent `cat` of
the kernel files:

| case | result | what it measured |
|------|--------|------------------|
| SM-THROTTLE-1 | PASS | `expand --priority background` wrote `max=42000 min=25000 (local)` to the band's own attributes. The floor is the profile's, not the fixed 1000 it used to be. |
| SM-THROTTLE-3 | PASS | The host-wide `speed_limit_max` stayed at the operator's 137000 throughout a `--priority max` scrub; the band's own limits were cleared to `(system)` afterwards, and nothing was recorded as borrowed. |
| SM-THROTTLE-4 | PASS | With `smartctl` moved aside, a `--priority max` reshape's limits were `10000000/10000000` before and after three ticks: unchanged. |
| SM-THROTTLE-5 | PASS (climb-back skipped) | A genuine CPU-load breach decreased 105000 -> 73500 -> 51450 -> 41166 and stopped at the floor the capability estimate derived. The climb-back itself cannot be observed on loopback (no SMART temperature, so every tick decreases); it is covered at the unit layer. |
| SM-THROTTLE-6 | PASS | `fs scrub start --priority max` wrote `10000000` to BOTH attributes, and both read `(system)` again once the check ended. |
| SM-THROTTLE-7 | PASS | Two groups scrubbing at once: alpha `10000000/10000000`, beta `42000/25000`, simultaneously. The host-wide parameter could not express this at all. |

SM-THROTTLE-2 was not re-run on the guest for this change; its assertions
were updated for the per-array files and the normalised load threshold.

### 9.4 Two defects found by running it

Both were found by SM-THROTTLE-7 against real arrays, not by any unit test:

- `apply_initial` wrote `stripe_cache_size`, which does not exist on a RAID1
  array, so a RAID1 band's scrub aborted before it started. That write is
  RAID456 write-path tuning and is now best-effort.
- `scrub_start` wrote the limits and only persisted the profile afterwards,
  so a failure in between left limits in force with nothing recording that
  this project had put them there. The record is now written first.
