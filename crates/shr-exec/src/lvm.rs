use crate::cmd::{CommandRunner, ExecError};

pub struct LvmExecutor<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> LvmExecutor<'a> {
    pub fn new(runner: &'a dyn CommandRunner) -> Self {
        Self { runner }
    }

    /// Verify LVM userspace is available before any destructive step (D11).
    pub fn ensure_supported(&self) -> Result<(), ExecError> {
        if self.runner.is_dry_run() {
            return Ok(());
        }
        self.runner.run("pvcreate", &["--version"])?;
        Ok(())
    }

    /// Run pvcreate on a device
    pub fn pvcreate(&self, dev_path: &str) -> Result<(), ExecError> {
        self.runner.run("pvcreate", &["-ff", "-y", dev_path])?;
        Ok(())
    }

    /// Create Volume Group over devices
    pub fn vgcreate(&self, vg_name: &str, dev_paths: &[&str]) -> Result<(), ExecError> {
        let mut args = vec![vg_name];
        args.extend_from_slice(dev_paths);
        self.runner.run("vgcreate", &args)?;
        Ok(())
    }

    /// Extend existing Volume Group with a new PV
    pub fn vgextend(&self, vg_name: &str, dev_path: &str) -> Result<(), ExecError> {
        self.runner.run("vgextend", &[vg_name, dev_path])?;
        Ok(())
    }

    /// Create Logical Volume using 100% of free space in VG.
    ///
    /// Layer 3: on a reused disk the new LV lands on the old one's Btrfs
    /// signature, and `lvcreate`'s "Wipe it? [y/n]" prompt has no terminal to
    /// answer it, so it defaults to `[n]` and aborts with "Failed to wipe
    /// start of new LV".
    ///
    /// Measured on the guest, same disks, three variants back to back: bare
    /// `lvcreate` and `--wipesignatures y` both hit the identical `[n]`
    /// default; only `-Wy -Zy --yes` printed "Wiping btrfs signature" and
    /// succeeded. `-Wy` is the short form of the flag that already failed, so
    /// `--yes` is what suppresses the prompt on this lvm2 (EL9). The run did
    /// not isolate `-Zy`; it is kept because this LV always receives a fresh
    /// `mkfs.btrfs` anyway.
    pub fn lvcreate_max(&self, vg_name: &str, lv_name: &str) -> Result<(), ExecError> {
        self.runner.run(
            "lvcreate",
            &["-l", "100%FREE", "-n", lv_name, "-Wy", "-Zy", "--yes", vg_name],
        )?;
        Ok(())
    }

    /// Remove a logical volume created by an unsuccessful create transaction.
    pub fn lvremove(&self, lv_path: &str) -> Result<(), ExecError> {
        self.runner.run("lvremove", &["-f", lv_path])?;
        Ok(())
    }

    /// Remove an empty volume group created by an unsuccessful transaction.
    pub fn vgremove(&self, vg_name: &str) -> Result<(), ExecError> {
        self.runner.run("vgremove", &["-f", vg_name])?;
        Ok(())
    }

    /// Remove an LVM label created by an unsuccessful transaction.
    pub fn pvremove(&self, dev_path: &str) -> Result<(), ExecError> {
        self.runner.run("pvremove", &["-ff", "-y", dev_path])?;
        Ok(())
    }

    /// Tell LVM a PV's underlying device grew (e.g. after an mdadm reshape)
    /// so the extra space becomes available to `lvextend`.
    pub fn pvresize(&self, dev_path: &str) -> Result<(), ExecError> {
        self.runner.run("pvresize", &[dev_path])?;
        Ok(())
    }

    /// The VG `pv_path` currently belongs to, or `""` if it isn't in one.
    ///
    /// An earlier review finding: `vgextend` can fail partway through
    /// committing its metadata update across every PV in the VG. Before
    /// blindly rolling back a failed `vgextend` (which would `pvremove -ff
    /// -y` the new PV), callers must check whether the PV actually ended up
    /// joined to the VG despite the reported failure -- wiping a PV that's
    /// live in a shared VG risks the OTHER, pre-existing PVs' data.
    pub fn pv_vg_name(&self, pv_path: &str) -> Result<String, ExecError> {
        if self.runner.is_dry_run() {
            return Ok(String::new());
        }
        let output = self.runner.run("pvs", &["--noheadings", "-o", "vg_name", pv_path])?;
        Ok(output.stdout.trim().to_string())
    }

    /// Extend Logical Volume to 100% of free space
    pub fn lvextend_max(&self, vg_name: &str, lv_name: &str) -> Result<(), ExecError> {
        let lv_path = format!("/dev/{}/{}", vg_name, lv_name);
        self.runner
            .run("lvextend", &["-l", "+100%FREE", &lv_path])?;
        Ok(())
    }

    /// Whether a volume group named `vg_name` already exists on the host,
    /// read via `vgs --noheadings -o vg_name <vg_name>`. This backs
    /// `OrchestrationEngine::create`'s preflight-stage collision guard:
    /// `vgcreate` itself doesn't run until deep inside `create()`'s
    /// destructive sequence (after partitions and mdadm arrays already
    /// exist), so letting IT be the first thing to notice a duplicate name
    /// turns an ordinary validation error into a partial-apply-then-rollback.
    ///
    /// Must read LIVE LVM state, never `state.toml`: a VG can exist on the
    /// host without shr-rs knowing about it at all (hand-created, another
    /// tool, an older shr-rs install that predates multi-group support) --
    /// exactly the case a state.toml-only uniqueness check would miss.
    /// `vgs` exits nonzero when the name doesn't exist (`Volume group "x"
    /// not found`) -- that's the "doesn't exist" answer, not an error to
    /// propagate.
    pub fn vg_exists(&self, vg_name: &str) -> Result<bool, ExecError> {
        if self.runner.is_dry_run() {
            return Ok(false);
        }
        match self.runner.run("vgs", &["--noheadings", "-o", "vg_name", vg_name]) {
            Ok(_) => Ok(true),
            Err(ExecError::NonZeroExit { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Whether logical volume `lv_name` already exists inside VG `vg_name`,
    /// read via `lvs --noheadings -o lv_name <vg_name>/<lv_name>` -- same
    /// live-vs-`state.toml` rationale as `vg_exists`. Only meaningful
    /// once `vg_exists` is known false-or-not-yet-checked for THIS `create()`
    /// attempt: an LV cannot exist inside a VG that doesn't, but this is
    /// still its own guard (not folded into `vg_exists`) so a future caller
    /// that legitimately targets an already-existing, empty VG still gets a
    /// specific, correct answer about the LV alone.
    pub fn lv_exists(&self, vg_name: &str, lv_name: &str) -> Result<bool, ExecError> {
        if self.runner.is_dry_run() {
            return Ok(false);
        }
        let target = format!("{vg_name}/{lv_name}");
        match self.runner.run("lvs", &["--noheadings", "-o", "lv_name", &target]) {
            Ok(_) => Ok(true),
            Err(ExecError::NonZeroExit { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }
}
