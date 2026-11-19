//! Strongly typed BLAKE3 content-addressed identifiers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{MemError, Result};

const DIGEST_LEN: usize = 32;

macro_rules! define_hash_id {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; DIGEST_LEN]);

        impl $name {
            /// Wraps a raw 32-byte BLAKE3 digest.
            pub const fn from_bytes(bytes: [u8; DIGEST_LEN]) -> Self {
                Self(bytes)
            }

            /// Returns the underlying 32-byte BLAKE3 digest.
            pub const fn as_bytes(&self) -> &[u8; DIGEST_LEN] {
                &self.0
            }

            /// Computes a new identifier by hashing `data` with BLAKE3.
            pub fn hash_bytes(data: &[u8]) -> Self {
                let digest = blake3::hash(data);
                Self(*digest.as_bytes())
            }

            /// Formats the digest as a 64-character lowercase hexadecimal string.
            pub fn to_hex(&self) -> String {
                hex::encode(self.0)
            }

            /// Parses a 64-character hexadecimal string into this identifier type.
            pub fn from_hex(s: &str) -> Result<Self> {
                let bytes = hex::decode(s).map_err(|e| {
                    MemError::InvalidHash(format!("invalid hex for {}: {e}", stringify!($name)))
                })?;
                if bytes.len() != DIGEST_LEN {
                    return Err(MemError::InvalidHash(format!(
                        "expected {DIGEST_LEN} bytes for {}, got {}",
                        stringify!($name),
                        bytes.len()
                    )));
                }
                let mut arr = [0u8; DIGEST_LEN];
                arr.copy_from_slice(&bytes);
                Ok(Self(arr))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_hex())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.to_hex())
            }
        }

        impl FromStr for $name {
            type Err = MemError;

            fn from_str(s: &str) -> Result<Self> {
                Self::from_hex(s)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_hex())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let hex_str = String::deserialize(deserializer)?;
                Self::from_hex(&hex_str).map_err(serde::de::Error::custom)
            }
        }
    };
}

define_hash_id!(BlobId, "Content-addressed BLAKE3 hash of raw memory bytes.");
define_hash_id!(TreeId, "Content-addressed BLAKE3 hash of a sorted Merkle tree node.");
define_hash_id!(CommitId, "Content-addressed BLAKE3 hash of an immutable commit snapshot.");
