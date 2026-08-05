use std::fmt;

use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContentMd5([u8; 16]);

impl ContentMd5 {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
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
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for ContentMd5 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
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
