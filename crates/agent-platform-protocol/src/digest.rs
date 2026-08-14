use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use sha2::{Digest as _, Sha256};

pub const DIGEST_BYTES: usize = 32;
pub const DIGEST_HEX_BYTES: usize = DIGEST_BYTES * 2;

/// Parse failure for a fixed-size lower-hex digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DigestParseError;

impl fmt::Display for DigestParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "expected exactly {DIGEST_HEX_BYTES} lowercase hexadecimal characters"
        )
    }
}

impl std::error::Error for DigestParseError {}

fn encode_lower_hex(bytes: &[u8; DIGEST_BYTES]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(DIGEST_HEX_BYTES);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_lower_hex(value: &str) -> Result<[u8; DIGEST_BYTES], DigestParseError> {
    if value.len() != DIGEST_HEX_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(DigestParseError);
    }
    let mut decoded = [0u8; DIGEST_BYTES];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        decoded[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
    }
    Ok(decoded)
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("lower-hex input was validated before decoding"),
    }
}

fn sha256(bytes: &[u8]) -> [u8; DIGEST_BYTES] {
    Sha256::digest(bytes).into()
}

macro_rules! digest_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name([u8; DIGEST_BYTES]);

        impl $name {
            /// Hash exactly these bytes. Callers that need semantic JSON
            /// identity must first produce RFC 8785/JCS bytes.
            pub fn sha256_bytes(bytes: &[u8]) -> Self {
                Self(sha256(bytes))
            }

            pub const fn from_bytes(bytes: [u8; DIGEST_BYTES]) -> Self {
                Self(bytes)
            }

            pub const fn as_bytes(&self) -> &[u8; DIGEST_BYTES] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&encode_lower_hex(&self.0))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.debug_tuple(stringify!($name)).field(&self.to_string()).finish()
            }
        }

        impl FromStr for $name {
            type Err = DigestParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                decode_lower_hex(value).map(Self)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::from_str(&value).map_err(D::Error::custom)
            }
        }
    };
}

digest_type!(
    /// Digest of the selected protocol schema/profile.
    SchemaDigest
);
