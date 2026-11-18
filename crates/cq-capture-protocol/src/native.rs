use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
};

/// Equality uses original bytes. Display text is never an identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NativeValue(pub Vec<u8>);
impl NativeValue {
    pub fn new(value: impl AsRef<OsStr>) -> Self {
        Self(value.as_ref().as_bytes().to_vec())
    }
    pub fn os(&self) -> OsString {
        OsString::from_vec(self.0.clone())
    }
    pub fn path(&self) -> PathBuf {
        PathBuf::from(self.os())
    }
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }
    pub fn display(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }
}
impl From<&str> for NativeValue {
    fn from(s: &str) -> Self {
        Self(s.as_bytes().to_vec())
    }
}
impl Serialize for NativeValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Encoded<'a> {
            encoding: &'a str,
            hex: String,
            display: String,
        }
        Encoded {
            encoding: "unix-bytes",
            hex: self.0.iter().map(|b| format!("{b:02x}")).collect(),
            display: self.display(),
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for NativeValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Encoded {
            encoding: String,
            hex: String,
        }
        let e = Encoded::deserialize(deserializer)?;
        if e.encoding != "unix-bytes" || e.hex.len() % 2 != 0 {
            return Err(serde::de::Error::custom("invalid native value"));
        }
        let bytes = e
            .hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let h = std::str::from_utf8(pair).map_err(serde::de::Error::custom)?;
                u8::from_str_radix(h, 16).map_err(serde::de::Error::custom)
            })
            .collect::<Result<Vec<_>, D::Error>>()?;
        Ok(Self(bytes))
    }
}
