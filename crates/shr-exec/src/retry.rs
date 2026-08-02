use crate::cmd::ExecError;
use std::time::Duration;

/// Attempt budget for [`retry_identity_read`]. `udevadm settle
/// --timeout=10` (`PartedExecutor::settle_udev`) already runs before every
/// caller of this helper, so the race it covers is only the residual gap
/// between "settle returned" and "this specific device's identity metadata
/// is queryable" -- confirmed by reproduction: the exact same operation run
/// alone on an idle guest always succeeds on the FIRST read. Five short
/// attempts is a generous margin over that gap without coming anywhere
/// close to the length of an actual resync/reshape (minutes), so exhausting
/// the budget can't be confused with "waited out background array
/// activity" -- it means the read is genuinely, persistently failing.
const MAX_ATTEMPTS: u32 = 5;

/// Delay between attempts. Total added latency on a fully-failing read is
/// `(MAX_ATTEMPTS - 1) * RETRY_DELAY` = 1.6s -- negligible next to how long
/// partitioning/array creation already takes, and zero on the common,
/// non-racy path (first attempt succeeds, loop returns immediately).
const RETRY_DELAY: Duration = Duration::from_millis(400);

/// Bounded retry for reads of identity metadata (a PARTUUID, a filesystem
/// UUID, an mdadm array UUID) taken immediately after creating or changing
/// the device/array being read.
///
/// Why this exists: found running Phase 4's real-guest smoke test creating
/// four SHR groups back-to-back while three OTHER mdadm arrays were
/// actively resyncing on the same slow (TCG-emulated) disk -- the fourth
/// group's `blkid` call returned exit code 2 ("nothing found") even though
/// `settle_udev` had already returned. Under that I/O load, `udevadm
/// settle` can return before the specific udev rule for THIS device has
/// finished populating the identity data being queried here: the device
/// node exists, the command runs against a real device and gets a real
/// exit code, but the value itself ("nothing found", or a success exit
/// with empty/missing output) isn't there yet. On a real NAS, creating or
/// expanding one group while another array is rebuilding is an entirely
/// normal thing to do -- a transient read failure here must not abort the
/// whole `create` and force a rollback of otherwise-healthy partitions and
/// arrays.
///
/// This is emphatically NOT a substitute for waiting out a resync or
/// reshape -- those can run for a long time; see `MAX_ATTEMPTS`'s doc
/// comment for why this helper's budget can't be mistaken for that.
///
/// `read` must never be invoked while the caller is in dry-run mode --
/// there is nothing real to retry against, and retrying against a runner
/// that always returns instantly would be pure dead time. Callers are
/// responsible for checking `is_dry_run()` and returning their own
/// simulated value BEFORE calling this helper.
///
/// An `Ok` whose value is empty is treated the same as an `Err`: a command
/// that exits 0 but prints nothing is "not ready yet", never a real
/// identifier (an empty PARTUUID/UUID flowing into `/dev/disk/by-partuuid/`
/// paths or `state.toml` would be silently wrong, and
/// `ArrayState::validate_no_placeholder_identifiers` does not check
/// `part_uuid`, so nothing downstream would catch it).
///
/// `context` names what is being read (e.g. `"PARTUUID for /dev/loop10p1"`)
/// so the error, if the budget is exhausted, tells the operator exactly
/// what failed and that it was retried -- not just the raw last error.
pub(crate) fn retry_identity_read<F>(context: &str, mut read: F) -> Result<String, ExecError>
where
    F: FnMut() -> Result<String, ExecError>,
{
    let mut last_failure = String::new();
    for attempt in 1..=MAX_ATTEMPTS {
        match read() {
            Ok(value) if !value.is_empty() => return Ok(value),
            Ok(_) => last_failure = "command succeeded but returned empty output".to_string(),
            Err(err) => last_failure = err.to_string(),
        }
        if attempt < MAX_ATTEMPTS {
            std::thread::sleep(RETRY_DELAY);
        }
    }

    Err(ExecError::Prerequisite(format!(
        "{context}: still not available after {MAX_ATTEMPTS} retried attempts \
         (likely a udev settle race under I/O load, not a permanent failure -- \
         last attempt: {last_failure})"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn succeeds_on_first_attempt_without_sleeping() {
        let calls = AtomicU32::new(0);
        let result = retry_identity_read("test value", || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok("abc-123".to_string())
        });
        assert_eq!(result.unwrap(), "abc-123");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retries_past_empty_output_until_a_real_value_shows_up() {
        let calls = AtomicU32::new(0);
        let result = retry_identity_read("test value", || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Ok(String::new())
            } else {
                Ok("real-value".to_string())
            }
        });
        assert_eq!(result.unwrap(), "real-value");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn retries_past_transient_errors_until_success() {
        let calls = AtomicU32::new(0);
        let result = retry_identity_read("test value", || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 3 {
                Err(ExecError::Prerequisite("transient".to_string()))
            } else {
                Ok("real-value".to_string())
            }
        });
        assert_eq!(result.unwrap(), "real-value");
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn gives_up_after_max_attempts_and_names_the_retry_in_the_error() {
        let calls = AtomicU32::new(0);
        let result = retry_identity_read("PARTUUID for /dev/loop10p1", || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(ExecError::Prerequisite("boom".to_string()))
        });
        let err = result.unwrap_err().to_string();
        assert_eq!(calls.load(Ordering::SeqCst), MAX_ATTEMPTS);
        assert!(err.contains(&MAX_ATTEMPTS.to_string()), "{err}");
        assert!(err.contains("boom"), "{err}");
        assert!(err.contains("PARTUUID for /dev/loop10p1"), "{err}");
    }

    #[test]
    fn empty_output_on_every_attempt_is_an_error_not_ok_empty_string() {
        let calls = AtomicU32::new(0);
        let result = retry_identity_read("test value", || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(String::new())
        });
        assert!(result.is_err(), "empty output must never surface as Ok(\"\")");
        assert_eq!(calls.load(Ordering::SeqCst), MAX_ATTEMPTS);
    }
}
