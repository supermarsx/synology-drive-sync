//! Lasting, user-fixable conditions that stop a profile's automatic runs.
//!
//! Today there is one: the core was killed by the out-of-memory killer
//! (`core_oom_killed`, proven by the cgroup's OOM counter). Re-running it on
//! the retry ladder only repeats the kill, and on a NAS the OOM killer may pick
//! a DSM service instead. So the killed run is recorded as what it was (failed,
//! red, alerts fire), and from then on the profile's routine and the
//! all-profiles schedule do not run it again until the operator acts:
//! - **Run now** (a requested run of the profile) clears the block and runs;
//!   if that run is killed again it records red and blocks again;
//! - a change to the profile's settings or the performance level clears it.
//!   "A change" is a different fingerprint of those files' bytes, not a newer
//!   modification time: a save that changes nothing leaves the block in place.
//!
//! The block lives in `var/state/lasting/<profile>.state` (0600), private to
//! the daemon, so a restart does not forget it and walk into the kill again.
//! The shell engine (`var/state/engine=shell`) does not read it.

#![forbid(unsafe_code)]

/// The record's first line.
pub(crate) const SCHEMA: &str = "schema=sdsyncd.lasting.v1";

/// One profile's block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Lasting {
    /// The reason token (`core_oom_killed`).
    pub(crate) reason: String,
    /// The killed run's detail line, with the memory a run had.
    pub(crate) detail: String,
    /// When the killed run finished (epoch seconds).
    pub(crate) since: u64,
    /// [`fingerprint`] of the profile fragment and `performance.conf` at the kill.
    pub(crate) fingerprint: u64,
}

impl Lasting {
    pub(crate) fn render(&self) -> Vec<u8> {
        format!(
            "{SCHEMA}\nreason={}\nsince_epoch={}\nfingerprint={:016x}\ndetail={}\n",
            self.reason,
            self.since,
            self.fingerprint,
            one_line(&self.detail)
        )
        .into_bytes()
    }

    /// Parse a record; every way it is not one is named.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > 4096 {
            return Err(format!("{} bytes, more than a record holds", bytes.len()));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| "not UTF-8".to_owned())?;
        let mut lines = text.lines();
        if lines.next() != Some(SCHEMA) {
            return Err(format!("the first line is not {SCHEMA}"));
        }
        let (mut reason, mut since, mut fingerprint, mut detail) = (None, None, None, None);
        for line in lines {
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("line {line:?} is not key=value"));
            };
            let slot = match key {
                "reason" => &mut reason,
                "since_epoch" => &mut since,
                "fingerprint" => &mut fingerprint,
                "detail" => &mut detail,
                other => return Err(format!("unknown key {other:?}")),
            };
            if slot.replace(value.to_owned()).is_some() {
                return Err(format!("{key} appears twice"));
            }
        }
        let reason = reason.ok_or("reason is missing")?;
        if reason != "core_oom_killed" {
            return Err(format!("reason {reason:?} is not a lasting condition"));
        }
        let since = since
            .ok_or("since_epoch is missing")?
            .parse()
            .map_err(|_| "since_epoch is not a number".to_owned())?;
        let fingerprint = u64::from_str_radix(&fingerprint.ok_or("fingerprint is missing")?, 16)
            .map_err(|_| "fingerprint is not hexadecimal".to_owned())?;
        Ok(Self {
            reason,
            detail: detail.unwrap_or_default(),
            since,
            fingerprint,
        })
    }
}

/// The settings a block is tied to: the profile fragment's bytes and
/// `performance.conf`'s, each `None` when absent. FNV-1a, stable across builds.
pub(crate) fn fingerprint(fragment: Option<&[u8]>, performance: Option<&[u8]>) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for part in [fragment, performance] {
        match part {
            Some(bytes) => {
                feed(b"present:");
                feed(&(bytes.len() as u64).to_le_bytes());
                feed(bytes);
            }
            None => feed(b"absent;"),
        }
    }
    hash
}

fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(240)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_and_every_defect_is_named() {
        let record = Lasting {
            reason: "core_oom_killed".into(),
            detail: "killed by the out-of-memory killer\nsecond line".into(),
            since: 1_780_000_000,
            fingerprint: 0xdead_beef,
        };
        let bytes = record.render();
        let parsed = Lasting::parse(&bytes).unwrap();
        assert_eq!(parsed.since, record.since);
        assert_eq!(parsed.fingerprint, record.fingerprint);
        assert!(!parsed.detail.contains('\n'));
        for (bad, why) in [
            (&b""[..], "first line"),
            (b"schema=sdsyncd.lasting.v1\nreason=core_failed\nsince_epoch=1\nfingerprint=1\n", "not a lasting"),
            (b"schema=sdsyncd.lasting.v1\nreason=core_oom_killed\nfingerprint=1\n", "since_epoch is missing"),
            (b"schema=sdsyncd.lasting.v1\nreason=core_oom_killed\nsince_epoch=x\nfingerprint=1\n", "not a number"),
            (b"schema=sdsyncd.lasting.v1\nreason=core_oom_killed\nreason=core_oom_killed\n", "twice"),
            (b"schema=sdsyncd.lasting.v1\ngarbage\n", "key=value"),
            (b"\xff\xfe", "UTF-8"),
        ] {
            let error = Lasting::parse(bad).unwrap_err();
            assert!(error.contains(why), "{why}: {error}");
        }
    }

    #[test]
    fn the_fingerprint_follows_content_not_presence_of_a_save() {
        let a = fingerprint(Some(b"source = \"/a\"\n"), Some(b"level=balanced\n"));
        assert_eq!(
            a,
            fingerprint(Some(b"source = \"/a\"\n"), Some(b"level=balanced\n"))
        );
        assert_ne!(
            a,
            fingerprint(Some(b"source = \"/b\"\n"), Some(b"level=balanced\n"))
        );
        assert_ne!(
            a,
            fingerprint(Some(b"source = \"/a\"\n"), Some(b"level=gentle\n"))
        );
        assert_ne!(a, fingerprint(Some(b"source = \"/a\"\n"), None));
        // The two parts cannot trade bytes.
        assert_ne!(
            fingerprint(Some(b"ab"), Some(b"c")),
            fingerprint(Some(b"a"), Some(b"bc"))
        );
    }
}
