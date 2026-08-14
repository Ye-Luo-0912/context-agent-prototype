//! Bounded artifact identity: scheme, run, owner, and content digest.
//!
//! Locators are semantic names, not filesystem paths. The workspace maps a
//! sealed locator onto `.focus-agent/artifacts/<run>/<owner>/<digest>` and a
//! draft locator onto a staging file. A producer-supplied path-shaped
//! `artifact://.focus-agent/...` string is not an identity and must fail
//! closed at parse time.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::{AgentError, RunId};

/// Wire scheme. The only admitted artifact scheme in v1.
pub const ARTIFACT_SCHEME: &str = "artifact";
/// Locator version segment. Reject anything else rather than guessing.
pub const ARTIFACT_LOCATOR_VERSION: &str = "v1";
/// Owner names share the tool-name budget so a grant/tool identity fits.
pub const MAX_ARTIFACT_OWNER_BYTES: usize = 64;
/// SHA-256 digest of the artifact bytes, encoded as lowercase hex.
pub const ARTIFACT_DIGEST_BYTES: usize = 32;
pub const ARTIFACT_DIGEST_HEX_BYTES: usize = ARTIFACT_DIGEST_BYTES * 2;
/// Typed cap for the entire locator string, including scheme and slashes.
/// Identity form tops out well below this; the older 1024-byte path cap is
/// retired with path-shaped locators.
pub const MAX_ARTIFACT_REFERENCE_BYTES: usize = 256;
const DRAFT_SEGMENT: &str = "draft";

/// SHA-256 of exact artifact bytes. This is not a JSON canonicalizer.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentDigest([u8; ARTIFACT_DIGEST_BYTES]);

impl ContentDigest {
    pub const fn from_bytes(bytes: [u8; ARTIFACT_DIGEST_BYTES]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; ARTIFACT_DIGEST_BYTES] {
        &self.0
    }

    pub fn sha256_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&encode_lower_hex(&self.0))
    }
}

impl fmt::Debug for ContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ContentDigest")
            .field(&self.to_string())
            .finish()
    }
}

impl FromStr for ContentDigest {
    type Err = ArtifactLocatorError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        decode_lower_hex(value).map(Self)
    }
}

impl Serialize for ContentDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ContentDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).map_err(D::Error::custom)
    }
}

/// Parse failure for an artifact locator or one of its components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactLocatorError(String);

impl ArtifactLocatorError {
    fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }

    pub fn reason(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ArtifactLocatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ArtifactLocatorError {}

impl From<ArtifactLocatorError> for AgentError {
    fn from(error: ArtifactLocatorError) -> Self {
        AgentError::InvalidRequest(error.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ArtifactBody {
    Sealed { digest: ContentDigest },
    Draft { staging_id: Uuid },
}

/// Canonical artifact identity.
///
/// Sealed: `artifact://v1/<run>/<owner>/<digest>`
/// Draft:  `artifact://v1/<run>/<owner>/draft/<staging-id>`
///
/// Draft locators name a still-growing capture. Completion evidence and
/// other immutable admissions must use the sealed form after `seal`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArtifactLocator {
    run_id: RunId,
    owner: String,
    body: ArtifactBody,
}

impl ArtifactLocator {
    pub fn sealed(
        run_id: RunId,
        owner: impl Into<String>,
        digest: ContentDigest,
    ) -> Result<Self, ArtifactLocatorError> {
        let owner = owner.into();
        validate_owner(&owner)?;
        Ok(Self {
            run_id,
            owner,
            body: ArtifactBody::Sealed { digest },
        })
    }

    pub fn draft(
        run_id: RunId,
        owner: impl Into<String>,
        staging_id: Uuid,
    ) -> Result<Self, ArtifactLocatorError> {
        let owner = owner.into();
        validate_owner(&owner)?;
        Ok(Self {
            run_id,
            owner,
            body: ArtifactBody::Draft { staging_id },
        })
    }

    pub fn parse(value: &str) -> Result<Self, ArtifactLocatorError> {
        if value.len() > MAX_ARTIFACT_REFERENCE_BYTES {
            return Err(ArtifactLocatorError::new(format!(
                "artifact reference is {} bytes; maximum is {MAX_ARTIFACT_REFERENCE_BYTES}",
                value.len()
            )));
        }
        if value.contains('?') || value.contains('#') {
            return Err(ArtifactLocatorError::new(
                "artifact references cannot carry query/fragment components",
            ));
        }
        if value.as_bytes().contains(&b'\\') || value.as_bytes().contains(&0) {
            return Err(ArtifactLocatorError::new(
                "artifact references cannot contain '\\' or NUL",
            ));
        }
        let rest = value.strip_prefix("artifact://").ok_or_else(|| {
            ArtifactLocatorError::new("artifact references must start with artifact://")
        })?;
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.iter().any(|part| part.is_empty()) {
            return Err(ArtifactLocatorError::new(
                "artifact references cannot contain empty path segments",
            ));
        }
        if parts.first() != Some(&ARTIFACT_LOCATOR_VERSION) {
            return Err(ArtifactLocatorError::new(format!(
                "artifact references must use {ARTIFACT_LOCATOR_VERSION} identity form"
            )));
        }
        match parts.as_slice() {
            [_, run, owner, digest] => {
                let locator = Self::sealed(
                    parse_canonical_run_id(run)?,
                    (*owner).to_owned(),
                    ContentDigest::from_str(digest)?,
                )?;
                if locator.to_string() != value {
                    return Err(ArtifactLocatorError::new(
                        "artifact reference is not in canonical identity form",
                    ));
                }
                Ok(locator)
            }
            [_, run, owner, "draft", staging] => {
                let staging_id = parse_canonical_uuid(staging, "draft staging id")?;
                let locator = Self::draft(
                    parse_canonical_run_id(run)?,
                    (*owner).to_owned(),
                    staging_id,
                )?;
                if locator.to_string() != value {
                    return Err(ArtifactLocatorError::new(
                        "artifact reference is not in canonical identity form",
                    ));
                }
                Ok(locator)
            }
            _ => Err(ArtifactLocatorError::new(
                "artifact references must be artifact://v1/<run>/<owner>/<digest> or .../draft/<id>",
            )),
        }
    }

    /// Completion and other immutable admissions require a sealed digest.
    pub fn parse_sealed(value: &str) -> Result<Self, ArtifactLocatorError> {
        let locator = Self::parse(value)?;
        if !locator.is_sealed() {
            return Err(ArtifactLocatorError::new(
                "completion and immutable admissions require a sealed owner/digest locator",
            ));
        }
        Ok(locator)
    }

    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn is_sealed(&self) -> bool {
        matches!(self.body, ArtifactBody::Sealed { .. })
    }

    pub fn digest(&self) -> Option<ContentDigest> {
        match self.body {
            ArtifactBody::Sealed { digest } => Some(digest),
            ArtifactBody::Draft { .. } => None,
        }
    }

    pub fn staging_id(&self) -> Option<Uuid> {
        match self.body {
            ArtifactBody::Draft { staging_id } => Some(staging_id),
            ArtifactBody::Sealed { .. } => None,
        }
    }

    pub fn ensure_run(&self, run_id: RunId) -> Result<(), ArtifactLocatorError> {
        if self.run_id != run_id {
            return Err(ArtifactLocatorError::new(format!(
                "artifact reference does not belong to run {run_id}"
            )));
        }
        Ok(())
    }
}

impl fmt::Display for ArtifactLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.body {
            ArtifactBody::Sealed { digest } => write!(
                formatter,
                "artifact://{ARTIFACT_LOCATOR_VERSION}/{}/{}/{digest}",
                self.run_id, self.owner
            ),
            ArtifactBody::Draft { staging_id } => write!(
                formatter,
                "artifact://{ARTIFACT_LOCATOR_VERSION}/{}/{}/{DRAFT_SEGMENT}/{staging_id}",
                self.run_id, self.owner
            ),
        }
    }
}

impl FromStr for ArtifactLocator {
    type Err = ArtifactLocatorError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for ArtifactLocator {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ArtifactLocator {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(D::Error::custom)
    }
}

/// Turn a write prefix into a locator owner. Invalid characters are dropped;
/// an empty or digit-leading result is prefixed so the identifier rule holds.
pub fn artifact_owner_from_prefix(prefix: &str) -> Result<String, ArtifactLocatorError> {
    let mut owner: String = prefix
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|ch| {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-')
        })
        .take(MAX_ARTIFACT_OWNER_BYTES)
        .collect();
    if owner.is_empty() {
        owner.push_str("artifact");
    } else if !owner.as_bytes()[0].is_ascii_lowercase() {
        owner.insert_str(0, "a-");
        owner.truncate(MAX_ARTIFACT_OWNER_BYTES);
    }
    validate_owner(&owner)?;
    Ok(owner)
}

pub fn validate_owner(owner: &str) -> Result<(), ArtifactLocatorError> {
    if owner.is_empty() {
        return Err(ArtifactLocatorError::new(
            "artifact owner must not be empty",
        ));
    }
    if owner.len() > MAX_ARTIFACT_OWNER_BYTES {
        return Err(ArtifactLocatorError::new(format!(
            "artifact owner is {} bytes; maximum is {MAX_ARTIFACT_OWNER_BYTES}",
            owner.len()
        )));
    }
    let mut bytes = owner.bytes();
    let Some(first) = bytes.next() else {
        return Err(ArtifactLocatorError::new(
            "artifact owner must not be empty",
        ));
    };
    if !first.is_ascii_lowercase() {
        return Err(ArtifactLocatorError::new(
            "artifact owner must start with a lowercase ASCII letter",
        ));
    }
    if !bytes.all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
    }) {
        return Err(ArtifactLocatorError::new(
            "artifact owner must contain only lowercase ASCII letters, digits, '.', '_' or '-'",
        ));
    }
    Ok(())
}

fn parse_canonical_run_id(value: &str) -> Result<RunId, ArtifactLocatorError> {
    parse_canonical_uuid(value, "run id").map(RunId)
}

fn parse_canonical_uuid(value: &str, field: &str) -> Result<Uuid, ArtifactLocatorError> {
    let parsed = Uuid::parse_str(value)
        .map_err(|_| ArtifactLocatorError::new(format!("{field} must be a UUID")))?;
    if parsed.to_string() != value {
        return Err(ArtifactLocatorError::new(format!(
            "{field} must use the canonical lowercase hyphenated UUID form"
        )));
    }
    Ok(parsed)
}

fn encode_lower_hex(bytes: &[u8; ARTIFACT_DIGEST_BYTES]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(ARTIFACT_DIGEST_HEX_BYTES);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_lower_hex(value: &str) -> Result<[u8; ARTIFACT_DIGEST_BYTES], ArtifactLocatorError> {
    if value.len() != ARTIFACT_DIGEST_HEX_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ArtifactLocatorError::new(format!(
            "artifact digest must be exactly {ARTIFACT_DIGEST_HEX_BYTES} lowercase hexadecimal characters"
        )));
    }
    let mut decoded = [0u8; ARTIFACT_DIGEST_BYTES];
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

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(bytes: &[u8]) -> ContentDigest {
        ContentDigest::sha256_bytes(bytes)
    }

    #[test]
    fn sealed_locator_roundtrips_and_rejects_path_form() {
        let run = RunId::new();
        let locator = ArtifactLocator::sealed(run, "grep", digest_of(b"one\n")).unwrap();
        let encoded = locator.to_string();
        assert!(encoded.starts_with("artifact://v1/"));
        assert!(encoded.contains("/grep/"));
        assert_eq!(ArtifactLocator::parse(&encoded).unwrap(), locator);
        assert!(locator.is_sealed());

        assert!(
            ArtifactLocator::parse(&format!("artifact://.focus-agent/artifacts/{run}/grep.txt"))
                .is_err()
        );
        assert!(ArtifactLocator::parse(&format!("{encoded}?page=1")).is_err());
        assert!(ArtifactLocator::parse(&encoded.to_uppercase()).is_err());
    }

    #[test]
    fn draft_locator_is_distinct_from_sealed() {
        let run = RunId::new();
        let staging = Uuid::new_v4();
        let locator = ArtifactLocator::draft(run, "shell", staging).unwrap();
        let encoded = locator.to_string();
        assert!(encoded.contains("/draft/"));
        assert!(!locator.is_sealed());
        assert!(ArtifactLocator::parse_sealed(&encoded).is_err());
        assert_eq!(ArtifactLocator::parse(&encoded).unwrap(), locator);
    }

    #[test]
    fn owner_from_prefix_sanitizes_without_inventing_path_segments() {
        assert_eq!(artifact_owner_from_prefix("fs.list").unwrap(), "fs.list");
        assert_eq!(
            artifact_owner_from_prefix("assistant-response").unwrap(),
            "assistant-response"
        );
        assert_eq!(artifact_owner_from_prefix("12-logs").unwrap(), "a-12-logs");
        assert!(artifact_owner_from_prefix("ok").unwrap().contains("ok"));
    }

    #[test]
    fn content_digest_is_sha256_of_exact_bytes() {
        let digest = ContentDigest::sha256_bytes(b"hello");
        assert_eq!(
            digest.to_string(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
