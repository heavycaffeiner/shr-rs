//! Parse `/proc/mdstat` into structured array status, including reshape/resync
//! progress. This is a small hand-written line parser (the format is stable but
//! not JSON).

/// The whole `/proc/mdstat` snapshot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MdStat {
    pub personalities: Vec<String>,
    pub arrays: Vec<MdArray>,
}

/// One `mdN` array.
#[derive(Debug, Clone, PartialEq)]
pub struct MdArray {
    pub name: String,
    pub state: String,
    /// `(read-only)` or `(auto-read-only)` — mdadm will not sync while set.
    pub read_only: bool,
    pub level: Option<String>,
    pub members: Vec<MdMember>,
    pub blocks: Option<u64>,
    /// `[T/A]`: T configured raid disks.
    pub raid_disks: Option<usize>,
    /// `[T/A]`: A currently active disks.
    pub active_disks: Option<usize>,
    /// `[UU_U]` health string.
    pub health: Option<String>,
    pub sync: Option<SyncStatus>,
}

impl MdArray {
    /// Degraded if the health string has a `_`, or (when the health group is
    /// absent) fewer active than configured disks.
    pub fn is_degraded(&self) -> bool {
        if let Some(h) = self.health.as_deref() {
            return h.contains('_');
        }
        matches!((self.raid_disks, self.active_disks), (Some(t), Some(a)) if a < t)
    }
}

/// A member device line entry, e.g. `sdd1[3](F)`.
#[derive(Debug, Clone, PartialEq)]
pub struct MdMember {
    pub name: String,
    pub role: Option<u32>,
    pub faulty: bool,
    pub spare: bool,
    pub write_mostly: bool,
    /// `(R)` — a replacement device rebuilding in place.
    pub replacement: bool,
}

/// An in-progress resync/recovery/reshape/check.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncStatus {
    pub action: String,
    /// `None` for `PENDING`/`DELAYED` states that carry no percentage.
    pub percent: Option<f64>,
    pub speed_kb: Option<u64>,
    pub finish_min: Option<f64>,
}

const SYNC_ACTIONS: [&str; 5] = ["reshape", "recovery", "resync", "check", "repair"];

/// Parse the full contents of `/proc/mdstat`.
pub fn parse_mdstat(text: &str) -> MdStat {
    let mut out = MdStat::default();

    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }

        // "Personalities : [raid1] [raid6] [raid5]"
        if let Some(rest) = trimmed.strip_prefix("Personalities") {
            let rest = rest.trim_start_matches([' ', ':']);
            out.personalities = bracket_groups(rest);
            continue;
        }
        if trimmed.starts_with("unused devices") {
            continue;
        }

        // Header line: "mdN : active raidX dev[role] ..."
        if !line.starts_with(char::is_whitespace) {
            if let Some((name, rest)) = trimmed.split_once(" : ") {
                if let Some(arr) = parse_header(name.trim(), rest.trim()) {
                    out.arrays.push(arr);
                    continue;
                }
            }
        }

        // Continuation line for the most recent array.
        if let Some(arr) = out.arrays.last_mut() {
            apply_detail_line(arr, trimmed.trim());
        }
    }

    out
}

fn parse_header(name: &str, rest: &str) -> Option<MdArray> {
    let mut tokens = rest.split_whitespace();
    let state = tokens.next()?.to_string();
    if state != "active" && state != "inactive" && state != "clean" {
        return None;
    }

    let mut read_only = false;
    let mut level = None;
    let mut members = Vec::new();
    for tok in tokens {
        if tok.starts_with('(') {
            // State modifier like "(auto-read-only)" / "(read-only)".
            if tok.contains("read-only") {
                read_only = true;
            }
            continue;
        }
        if tok.contains('[') {
            members.push(parse_member(tok));
        } else if level.is_none() {
            level = Some(tok.to_string());
        }
    }

    Some(MdArray {
        name: name.to_string(),
        state,
        read_only,
        level,
        members,
        blocks: None,
        raid_disks: None,
        active_disks: None,
        health: None,
        sync: None,
    })
}

fn parse_member(tok: &str) -> MdMember {
    let name: String = tok.chars().take_while(|c| *c != '[' && *c != '(').collect();
    let role = between(tok, '[', ']').and_then(|s| s.parse().ok());
    MdMember {
        name,
        role,
        faulty: tok.contains("(F)"),
        spare: tok.contains("(S)"),
        write_mostly: tok.contains("(W)"),
        replacement: tok.contains("(R)"),
    }
}

fn apply_detail_line(arr: &mut MdArray, line: &str) {
    // "NNN blocks super 1.2 level 6, 512k chunk ... [4/4] [UUUU]"
    if line.contains("blocks") {
        if let Some(first) = line.split_whitespace().next() {
            arr.blocks = first.parse().ok();
        }
        for g in bracket_groups(line) {
            if let Some((t, a)) = g.split_once('/') {
                if let (Ok(t), Ok(a)) = (t.trim().parse(), a.trim().parse()) {
                    arr.raid_disks = Some(t);
                    arr.active_disks = Some(a);
                }
            } else if !g.is_empty() && g.chars().all(|c| c == 'U' || c == '_') {
                arr.health = Some(g);
            }
        }
        return;
    }

    // Progress line, either with a percentage
    //   "[===>...] reshape = 12.6% (x/y) finish=34.5min speed=12345K/sec"
    // or a pending/delayed marker "resync=PENDING" / "recovery=DELAYED".
    if let Some(action) = SYNC_ACTIONS.iter().copied().find(|a| line.contains(*a)) {
        let percent = line
            .split_whitespace()
            .find_map(|t| t.strip_suffix('%').and_then(|n| n.parse::<f64>().ok()));
        let pending = line.contains("PENDING") || line.contains("DELAYED");
        if percent.is_some() || pending {
            let speed_kb = token_value(line, "speed=").and_then(|v| v.trim_end_matches("K/sec").parse().ok());
            let finish_min =
                token_value(line, "finish=").and_then(|v| v.trim_end_matches("min").parse().ok());
            arr.sync = Some(SyncStatus {
                action: action.to_string(),
                percent,
                speed_kb,
                finish_min,
            });
        }
    }
}

/// Contents of each `[...]` group in order.
fn bracket_groups(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut group_start = None;
    for (i, c) in s.char_indices() {
        match c {
            '[' => group_start = Some(i + 1),
            ']' => {
                if let Some(start) = group_start.take() {
                    out.push(s[start..i].to_string());
                }
            }
            _ => {}
        }
    }
    out
}

/// Text between the first `open` and the next `close` after it.
fn between(s: &str, open: char, close: char) -> Option<&str> {
    let start = s.find(open)? + 1;
    let end = s[start..].find(close)? + start;
    Some(&s[start..end])
}

/// Value of a `key=...` token (whitespace-delimited).
fn token_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace().find_map(|t| t.strip_prefix(key))
}

#[cfg(test)]
mod tests {
    use super::parse_mdstat;

    /// Real-guest repro: a faulty
    /// member must be parsed out distinguishably from a healthy one, with
    /// its role index preserved, not silently folded into a plain name.
    #[test]
    fn faulty_member_is_flagged_with_its_role_index() {
        let text = "Personalities : [raid5]\n\
                     md0 : active raid5 loop15p1[4] loop12p1[3](F) loop11p1[1] loop10p1[0]\n      \
                     8378368 blocks super 1.2 level 5, 512k chunk, algorithm 2 [3/3] [UUU]\n";
        let stat = parse_mdstat(text);
        assert_eq!(stat.arrays.len(), 1);
        let arr = &stat.arrays[0];
        assert_eq!(
            arr.members.len(),
            4,
            "the faulty member is still listed, not dropped"
        );

        let faulty = arr
            .members
            .iter()
            .find(|m| m.name == "loop12p1")
            .expect("faulty member present");
        assert_eq!(faulty.role, Some(3));
        assert!(faulty.faulty);
        assert!(!faulty.spare);

        let healthy = arr
            .members
            .iter()
            .find(|m| m.name == "loop15p1")
            .expect("healthy member present");
        assert!(!healthy.faulty);
        assert!(!healthy.spare);
    }

    /// A spare member (`(S)`) must be flagged as a spare, never as faulty.
    #[test]
    fn spare_member_is_flagged_and_not_faulty() {
        let text = "md0 : active raid5 sdb1[0] sdc1[1] sdd1[2] sde1[4](S)\n      \
                     12582912 blocks super 1.2 level 5, 64k chunk, algorithm 2 [3/3] [UUU]\n";
        let stat = parse_mdstat(text);
        let arr = &stat.arrays[0];

        let spare = arr
            .members
            .iter()
            .find(|m| m.name == "sde1")
            .expect("spare member present");
        assert_eq!(spare.role, Some(4));
        assert!(spare.spare);
        assert!(!spare.faulty);

        let healthy = arr
            .members
            .iter()
            .find(|m| m.name == "sdb1")
            .expect("healthy member present");
        assert!(!healthy.spare);
        assert!(!healthy.faulty);
    }
}
