//! Byte-preserving Unix paths for the owned X11 UI helper's JSON transport.
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeSeq};
use std::{os::unix::ffi::{OsStrExt, OsStringExt}, path::{Path, PathBuf}};

struct BorrowedPath<'a>(&'a Path);
impl Serialize for BorrowedPath<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.0.as_os_str().as_bytes())
    }
}

pub(crate) mod path {
    use super::*;
    pub fn serialize<S: Serializer>(value: &Path, serializer: S) -> Result<S::Ok, S::Error> {
        BorrowedPath(value).serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
        Vec::<u8>::deserialize(deserializer)
            .map(|bytes| std::ffi::OsString::from_vec(bytes).into())
    }
}

pub(crate) mod paths {
    use super::*;
    pub fn serialize<S: Serializer>(value: &[PathBuf], serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(value.len()))?;
        for path in value { sequence.serialize_element(&BorrowedPath(path))?; }
        sequence.end()
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<PathBuf>, D::Error> {
        struct Paths;
        impl<'de> serde::de::Visitor<'de> for Paths {
            type Value = Vec<PathBuf>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a sequence of Unix path byte arrays")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
                let mut paths = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
                while let Some(bytes) = sequence.next_element::<Vec<u8>>()? {
                    paths.push(std::ffi::OsString::from_vec(bytes).into());
                }
                Ok(paths)
            }
        }
        deserializer.deserialize_seq(Paths)
    }
}
