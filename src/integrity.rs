use std::fmt;

use crc32fast::Hasher as Crc32Hasher;
use md5::{Digest as _, Md5};
use sha2::{Digest as _, Sha256};

use crate::{Error, Result};

/// A complete content fingerprint used by content-mode comparison.
///
/// MD5 remains part of the value for File Station compatibility and stable
/// observability fields. New fingerprints also carry IEEE CRC32 and SHA-256;
/// equality for content correspondence is exposed through [`Self::full_match`]
/// and requires every digest to be present on both sides.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContentFingerprint {
    md5: [u8; 16],
    strong: Option<StrongContentDigests>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct StrongContentDigests {
    crc32: u32,
    sha256: [u8; 32],
}

/// Source-compatible name for callers that consume File Station's MD5.
/// Values returned by the sync hasher are complete [`ContentFingerprint`]s.
/// Derived equality compares the whole representation, so callers that need
/// MD5-only compatibility must use [`ContentFingerprint::md5_matches`].
pub type ContentMd5 = ContentFingerprint;

impl ContentFingerprint {
    /// Construct an MD5-only legacy value.
    ///
    /// It remains useful for the public File Station MD5 helper, but it is not
    /// sufficient to prove equality in content comparison.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self {
            md5: bytes,
            strong: None,
        }
    }

    pub fn from_digests(md5: [u8; 16], crc32: u32, sha256: [u8; 32]) -> Self {
        Self {
            md5,
            strong: Some(StrongContentDigests { crc32, sha256 }),
        }
    }

    pub fn from_content(content: &[u8]) -> Self {
        let mut hasher = ContentHasher::new();
        hasher.update(content);
        hasher.finalize()
    }

    pub fn parse_hex(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.len() != 32 {
            return Err(Error::InvalidContentHash);
        }
        let mut bytes = [0_u8; 16];
        for (index, output) in bytes.iter_mut().enumerate() {
            let offset = index * 2;
            let high = hex_value(value.as_bytes()[offset]).ok_or(Error::InvalidContentHash)?;
            let low = hex_value(value.as_bytes()[offset + 1]).ok_or(Error::InvalidContentHash)?;
            *output = (high << 4) | low;
        }
        Ok(Self::from_bytes(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.md5
    }

    pub fn crc32(&self) -> Option<u32> {
        self.strong.map(|digests| digests.crc32)
    }

    pub fn sha256(&self) -> Option<&[u8; 32]> {
        self.strong.as_ref().map(|digests| &digests.sha256)
    }

    pub fn crc32_hex(&self) -> Option<String> {
        self.crc32().map(|digest| format!("{digest:08x}"))
    }

    pub fn sha256_hex(&self) -> Option<String> {
        self.sha256().map(hex_encode)
    }

    pub fn has_full_proof(&self) -> bool {
        self.strong.is_some()
    }

    /// Compare all three digests. `None` means one side is an MD5-only legacy
    /// value and therefore cannot establish content equality.
    pub fn full_match(&self, other: &Self) -> Option<bool> {
        let (Some(left), Some(right)) = (self.strong, other.strong) else {
            return None;
        };
        Some(self.md5 == other.md5 && left == right)
    }

    /// Compare the compatibility MD5 component only. Content planning must use
    /// [`Self::full_match`]; this is reserved for explicit legacy MD5 helpers.
    pub fn md5_matches(&self, other: &Self) -> bool {
        self.md5 == other.md5
    }
}

impl fmt::Display for ContentFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.md5 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

pub struct ContentHasher {
    md5: Md5,
    crc32: Crc32Hasher,
    sha256: Sha256,
}

impl ContentHasher {
    pub fn new() -> Self {
        Self {
            md5: Md5::new(),
            crc32: Crc32Hasher::new(),
            sha256: Sha256::new(),
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.md5.update(bytes);
        self.crc32.update(bytes);
        self.sha256.update(bytes);
    }

    pub fn finalize(self) -> ContentFingerprint {
        ContentFingerprint::from_digests(
            self.md5.finalize().into(),
            self.crc32.finalize(),
            self.sha256.finalize().into(),
        )
    }
}

impl Default for ContentHasher {
    fn default() -> Self {
        Self::new()
    }
}

fn hex_encode<const N: usize>(bytes: &[u8; N]) -> String {
    let mut output = String::with_capacity(N * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_md5_round_trips_canonical_hex() {
        let digest = ContentMd5::parse_hex("D41D8CD98F00B204E9800998ECF8427E").unwrap();
        assert_eq!(digest.to_string(), "d41d8cd98f00b204e9800998ecf8427e");
        assert!(ContentMd5::parse_hex("not-a-digest").is_err());
    }

    #[test]
    fn complete_fingerprint_matches_only_when_crc32_and_sha256_also_match() {
        let complete = ContentFingerprint::from_content(b"payload");
        let same = ContentFingerprint::from_content(b"payload");
        let md5_only = ContentFingerprint::from_bytes(*complete.as_bytes());

        assert_eq!(complete.full_match(&same), Some(true));
        assert_eq!(complete.full_match(&md5_only), None);
        assert_eq!(md5_only.full_match(&complete), None);
        assert_eq!(
            md5_only.full_match(&ContentFingerprint::from_bytes([0xff; 16])),
            None
        );
        assert_eq!(
            complete.full_match(&ContentFingerprint::from_bytes([0xff; 16])),
            None
        );
        assert!(complete.has_full_proof());
        assert!(!md5_only.has_full_proof());
        assert_eq!(complete.crc32_hex().as_deref(), Some("422c6a15"));
        assert_eq!(
            complete.sha256_hex().as_deref(),
            Some("239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5")
        );
    }

    #[test]
    fn equal_crc32_cannot_hide_a_different_sha256() {
        let md5 = [0x42; 16];
        let crc32 = 0x1234_5678;
        let left = ContentFingerprint::from_digests(md5, crc32, [0x11; 32]);
        let right = ContentFingerprint::from_digests(md5, crc32, [0x22; 32]);

        assert_eq!(left.crc32(), right.crc32());
        assert_ne!(left.sha256(), right.sha256());
        assert_eq!(left.full_match(&right), Some(false));
    }

    #[test]
    fn content_md5_preserves_decoded_bytes_and_trims_transport_whitespace() {
        let digest = ContentMd5::parse_hex(" \t00112233445566778899AaBbCcDdEeFf\r\n").unwrap();

        assert_eq!(
            *digest.as_bytes(),
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ]
        );
    }

    #[test]
    fn content_md5_rejects_invalid_high_and_low_nibbles_at_the_expected_length() {
        for value in [
            "g0112233445566778899aabbccddeeff",
            "0g112233445566778899aabbccddeeff",
        ] {
            assert!(
                matches!(ContentMd5::parse_hex(value), Err(Error::InvalidContentHash)),
                "{value}"
            );
        }
    }
}
