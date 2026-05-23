//! OCI digest type. An OCI digest is `<alg>:<hex>` — e.g.
//! `sha256:6c3c624...`. We support `sha256` and `sha512` per spec.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest {
    pub algorithm: Algorithm,
    /// Lowercase hex bytes — length is algorithm-determined.
    pub hex: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Algorithm {
    Sha256,
    Sha512,
}

impl Algorithm {
    pub fn hex_len(self) -> usize {
        match self {
            Algorithm::Sha256 => 64,
            Algorithm::Sha512 => 128,
        }
    }
}

#[derive(Debug, Error)]
pub enum DigestError {
    #[error("missing ':' in digest")]
    MissingColon,
    #[error("unknown algorithm: {0}")]
    UnknownAlgorithm(String),
    #[error("hex length wrong: expected {expected}, got {got}")]
    HexLen { expected: usize, got: usize },
    #[error("hex contains non-hex characters")]
    NonHex,
}

impl FromStr for Digest {
    type Err = DigestError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (alg, hex) = s.split_once(':').ok_or(DigestError::MissingColon)?;
        let algorithm = match alg {
            "sha256" => Algorithm::Sha256,
            "sha512" => Algorithm::Sha512,
            other => return Err(DigestError::UnknownAlgorithm(other.to_string())),
        };
        if hex.len() != algorithm.hex_len() {
            return Err(DigestError::HexLen {
                expected: algorithm.hex_len(),
                got: hex.len(),
            });
        }
        if !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(DigestError::NonHex);
        }
        Ok(Digest {
            algorithm,
            hex: hex.to_string(),
        })
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.algorithm {
            Algorithm::Sha256 => write!(f, "sha256:{}", self.hex),
            Algorithm::Sha512 => write!(f, "sha512:{}", self.hex),
        }
    }
}

impl TryFrom<String> for Digest {
    type Error = DigestError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Digest::from_str(&s)
    }
}

impl From<Digest> for String {
    fn from(d: Digest) -> Self {
        d.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sha256_roundtrip() {
        let s = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let d: Digest = s.parse().unwrap();
        assert_eq!(d.algorithm, Algorithm::Sha256);
        assert_eq!(d.to_string(), s);
    }

    #[test]
    fn reject_wrong_hex_len() {
        let r: Result<Digest, _> = "sha256:short".parse();
        assert!(matches!(r, Err(DigestError::HexLen { .. })));
    }

    #[test]
    fn reject_non_hex() {
        let r: Result<Digest, _> =
            "sha256:zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".parse();
        assert!(matches!(r, Err(DigestError::NonHex)));
    }

    #[test]
    fn reject_unknown_algorithm() {
        let r: Result<Digest, _> = "md5:abcd".parse();
        assert!(matches!(r, Err(DigestError::UnknownAlgorithm(_))));
    }
}
