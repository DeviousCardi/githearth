//! Stable storage identifiers shared by future local and RPC service adapters.
use std::{error::Error, fmt, str::FromStr};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RepositoryId(String);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObjectId(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidId(&'static str);

impl fmt::Display for InvalidId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl Error for InvalidId {}

fn lowercase_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl FromStr for RepositoryId {
    type Err = InvalidId;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() == 32 && lowercase_hex(value) {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidId(
                "repository ID must be 32 lowercase hexadecimal characters",
            ))
        }
    }
}

impl FromStr for ObjectId {
    type Err = InvalidId;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if matches!(value.len(), 40 | 64) && lowercase_hex(value) {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidId(
                "object ID must be a full lowercase SHA-1 or SHA-256 hexadecimal ID",
            ))
        }
    }
}

impl fmt::Display for RepositoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitRef {
    pub name: String,
    pub target: ObjectId,
}

/// Read contract usable by an in-process caller and a future RPC implementation.
pub trait RepositoryReader {
    type Error: Error;
    fn list_refs(&self, repository: &RepositoryId) -> Result<Vec<GitRef>, Self::Error>;
    fn read_blob(
        &self,
        repository: &RepositoryId,
        object: &ObjectId,
        max_bytes: usize,
    ) -> Result<Vec<u8>, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_and_revision_expressions() {
        for input in [
            "../outside",
            "/absolute",
            "--help",
            "",
            "a/b",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert!(input.parse::<RepositoryId>().is_err());
        }
        for input in ["HEAD", "main:file", "--batch", "abc"] {
            assert!(input.parse::<ObjectId>().is_err());
        }
        assert!(
            "0123456789abcdef0123456789abcdef"
                .parse::<RepositoryId>()
                .is_ok()
        );
        assert!("a".repeat(40).parse::<ObjectId>().is_ok());
        assert!("b".repeat(64).parse::<ObjectId>().is_ok());
    }
}
