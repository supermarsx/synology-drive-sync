//! A minimal TZif (RFC 8536) reader for tests: real zone data without a tz crate.
//!
//! Only the transition table is read (the v2+ 64-bit block when present). The
//! POSIX TZ footer is ignored, so a fixture must be a "fat" file whose table
//! covers the tested instants; [`TzifClock::last_transition`] lets a test prove it.

use super::{LocalClock, WallFields, fields_at_offset};

pub(crate) struct TzifClock {
    transitions: Vec<i64>,
    indices: Vec<u8>,
    offsets: Vec<i64>,
}

fn be32(bytes: &[u8], at: usize) -> Result<u32, String> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| format!("truncated at {at}"))
}

fn be64(bytes: &[u8], at: usize) -> Result<i64, String> {
    bytes
        .get(at..at + 8)
        .map(|b| i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
        .ok_or_else(|| format!("truncated at {at}"))
}

struct Header {
    version: u8,
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

fn header(bytes: &[u8], at: usize) -> Result<Header, String> {
    if bytes.get(at..at + 4) != Some(b"TZif") {
        return Err("not a TZif file".into());
    }
    let count = |index: usize| be32(bytes, at + 20 + index * 4).map(|v| v as usize);
    Ok(Header {
        version: *bytes.get(at + 4).ok_or("truncated header")?,
        isutcnt: count(0)?,
        isstdcnt: count(1)?,
        leapcnt: count(2)?,
        timecnt: count(3)?,
        typecnt: count(4)?,
        charcnt: count(5)?,
    })
}

impl TzifClock {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, String> {
        let first = header(bytes, 0)?;
        let (h, base, wide) = if first.version >= b'2' {
            let v1_len = first.timecnt * 5
                + first.typecnt * 6
                + first.charcnt
                + first.leapcnt * 8
                + first.isstdcnt
                + first.isutcnt;
            let at = 44 + v1_len;
            (header(bytes, at)?, at + 44, true)
        } else {
            (first, 44, false)
        };
        let width = if wide { 8 } else { 4 };
        let mut transitions = Vec::with_capacity(h.timecnt);
        for i in 0..h.timecnt {
            let at = base + i * width;
            transitions.push(if wide {
                be64(bytes, at)?
            } else {
                i64::from(be32(bytes, at)? as i32)
            });
        }
        let index_base = base + h.timecnt * width;
        let indices = bytes
            .get(index_base..index_base + h.timecnt)
            .ok_or("truncated indices")?
            .to_vec();
        let type_base = index_base + h.timecnt;
        let mut offsets = Vec::with_capacity(h.typecnt);
        for i in 0..h.typecnt {
            offsets.push(i64::from(be32(bytes, type_base + i * 6)? as i32));
        }
        if offsets.is_empty() || indices.iter().any(|i| usize::from(*i) >= offsets.len()) {
            return Err("invalid local time types".into());
        }
        Ok(Self {
            transitions,
            indices,
            offsets,
        })
    }

    pub(crate) fn last_transition(&self) -> i64 {
        self.transitions.last().copied().unwrap_or(i64::MIN)
    }

    fn offset_at(&self, epoch: i64) -> i64 {
        match self.transitions.partition_point(|t| *t <= epoch) {
            // Before the first transition: the first type, as RFC 8536 3.2 says.
            0 => self.offsets[0],
            n => self.offsets[usize::from(self.indices[n - 1])],
        }
    }
}

impl LocalClock for TzifClock {
    fn fields(&self, epoch: i64) -> Option<WallFields> {
        Some(fields_at_offset(epoch, self.offset_at(epoch)))
    }
}
