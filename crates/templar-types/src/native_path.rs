//! Compatible JSON native path codec: UTF-8 stays a string; otherwise canonical base64.
//! Paths may be relative (survey entries); operation and service boundaries enforce absoluteness.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

fn decode(encoded: &str) -> Result<Vec<u8>, String> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|error| error.to_string())?;
    if STANDARD.encode(&bytes) != encoded {
        return Err("noncanonical native path base64".into());
    }
    if bytes.contains(&0) {
        return Err("native path contains NUL".into());
    }
    Ok(bytes)
}
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum WirePath {
    Text(String),
    Native { path_b64: String },
}
impl WirePath {
    fn of(path: &Path) -> Self {
        match path.to_str() {
            Some(text) => Self::Text(text.to_owned()),
            None => Self::Native {
                path_b64: STANDARD.encode(path.as_os_str().as_bytes()),
            },
        }
    }
    fn into_path(self) -> Result<PathBuf, String> {
        let bytes = match self {
            Self::Text(text) => text.into_bytes(),
            Self::Native { path_b64 } => decode(&path_b64)?,
        };
        if bytes.contains(&0) {
            return Err("native path contains NUL".into());
        }
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
}
pub fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    match path.to_str() {
        Some(text) => text.serialize(serializer),
        None => WirePath::Native {
            path_b64: STANDARD.encode(path.as_os_str().as_bytes()),
        }
        .serialize(serializer),
    }
}
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
    WirePath::deserialize(deserializer)?
        .into_path()
        .map_err(serde::de::Error::custom)
}
/// Maps retain their existing keys and encode only their native path values.
pub mod map {
    use super::{Deserialize, Deserializer, Path, PathBuf, Serialize, Serializer, WirePath};
    pub fn serialize<'a, K, M, S>(paths: &'a M, serializer: S) -> Result<S::Ok, S::Error>
    where
        K: Serialize + 'a,
        &'a M: IntoIterator<Item = (&'a K, &'a PathBuf)>,
        S: Serializer,
    {
        use serde::ser::SerializeMap;
        struct Native<'a>(&'a Path);
        impl Serialize for Native<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                super::serialize(self.0, serializer)
            }
        }
        let mut map = serializer.serialize_map(None)?;
        for (key, path) in paths {
            map.serialize_entry(key, &Native(path))?;
        }
        map.end()
    }
    pub fn deserialize<'de, K, M, D>(deserializer: D) -> Result<M, D::Error>
    where
        K: Deserialize<'de>,
        M: Default + Extend<(K, PathBuf)>,
        D: Deserializer<'de>,
    {
        struct Paths<K, M>(std::marker::PhantomData<(K, M)>);
        impl<'de, K: Deserialize<'de>, M: Default + Extend<(K, PathBuf)>> serde::de::Visitor<'de>
            for Paths<K, M>
        {
            type Value = M;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a map of native paths")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut access: A) -> Result<M, A::Error> {
                let mut paths = M::default();
                while let Some((key, value)) = access.next_entry::<K, WirePath>()? {
                    let path = value.into_path().map_err(serde::de::Error::custom)?;
                    paths.extend(std::iter::once((key, path)));
                }
                Ok(paths)
            }
        }
        deserializer.deserialize_map(Paths(std::marker::PhantomData))
    }
}
pub mod many {
    use super::{Deserialize, Deserializer, Path, PathBuf, Serialize, Serializer, WirePath};
    pub fn serialize<S: Serializer>(paths: &[PathBuf], serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        struct Native<'a>(&'a Path);
        impl Serialize for Native<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                super::serialize(self.0, serializer)
            }
        }
        let mut seq = serializer.serialize_seq(Some(paths.len()))?;
        for path in paths {
            seq.serialize_element(&Native(path))?;
        }
        seq.end()
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<PathBuf>, D::Error> {
        Vec::<WirePath>::deserialize(deserializer)?
            .into_iter()
            .map(|path| path.into_path().map_err(serde::de::Error::custom))
            .collect()
    }
}
pub mod optional {
    use super::{Deserialize, Deserializer, PathBuf, Serialize, Serializer, WirePath};
    pub fn serialize<S: Serializer>(
        path: &Option<PathBuf>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        path.as_deref().map(WirePath::of).serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<PathBuf>, D::Error> {
        Option::<WirePath>::deserialize(deserializer)?
            .map(|path| path.into_path().map_err(serde::de::Error::custom))
            .transpose()
    }
}
/// Survey-only filename codec. Native basenames are not required to be absolute paths.
pub mod names {
    use super::*;
    use std::{collections::BTreeSet, ffi::OsString};
    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum Name {
        Text(String),
        Native { name_b64: String },
    }
    pub fn serialize<S: Serializer>(
        names: &BTreeSet<OsString>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        struct Native<'a>(&'a std::ffi::OsStr);
        impl Serialize for Native<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                match self.0.to_str() {
                    Some(text) => text.serialize(serializer),
                    None => Name::Native {
                        name_b64: STANDARD.encode(self.0.as_bytes()),
                    }
                    .serialize(serializer),
                }
            }
        }
        let mut seq = serializer.serialize_seq(Some(names.len()))?;
        for name in names {
            seq.serialize_element(&Native(name))?;
        }
        seq.end()
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeSet<OsString>, D::Error> {
        Vec::<Name>::deserialize(deserializer)?
            .into_iter()
            .map(|name| {
                let bytes = match name {
                    Name::Text(text) => text.into_bytes(),
                    Name::Native { name_b64 } => {
                        decode(&name_b64).map_err(serde::de::Error::custom)?
                    }
                };
                if bytes.is_empty()
                    || bytes.contains(&0)
                    || bytes.contains(&b'/')
                    || bytes == b"."
                    || bytes == b".."
                {
                    return Err(serde::de::Error::custom("invalid native basename"));
                }
                Ok(OsString::from_vec(bytes))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Paths {
        #[serde(with = "super")]
        path: PathBuf,
        #[serde(with = "super::names")]
        names: std::collections::BTreeSet<std::ffi::OsString>,
    }
    #[test]
    fn raw_relative_paths_and_distinct_native_names_round_trip() {
        let paths = Paths {
            path: PathBuf::from(std::ffi::OsString::from_vec(b"sub/name-\xff".to_vec())),
            names: [b"name-\xfe".to_vec(), b"name-\xff".to_vec()]
                .into_iter()
                .map(std::ffi::OsString::from_vec)
                .collect(),
        };
        let json = serde_json::to_string(&paths).unwrap();
        assert_eq!(serde_json::from_str::<Paths>(&json).unwrap(), paths);
        assert!(json.contains("name_b64"));
    }
    #[test]
    fn malformed_basename_and_nul_path_are_refused() {
        for name in ["", ".", "..", "a/b", "a\0b"] {
            let json = serde_json::json!({"path": "/a", "names": [name]}).to_string();
            assert!(serde_json::from_str::<Paths>(&json).is_err());
        }
        assert!(
            serde_json::from_str::<Paths>(r#"{"path":{"path_b64":"L2EA"},"names":[]}"#).is_err()
        );
    }
}
