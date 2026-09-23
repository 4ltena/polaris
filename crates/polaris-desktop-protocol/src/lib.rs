//! Desktop IPCの値、有限byte列のcodec、純粋な受付・run遷移を定義する。I/Oや認可は行わない。

// serdeの位置順struct復号を禁止する。各型でmapを要求するため、enum内部での
// bufferingやfrom_valueを通る入れ子も配列へ読み替えない。
macro_rules! object_wire {
    ($(#[$attr:meta])* $vis:vis $kind:ident $name:ident $body:tt) => {
        $(#[$attr])*
        #[serde(remote = "Self")]
        $vis $kind $name $body

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                Self::serialize(self, serializer)
            }
        }
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct ObjectVisitor;
                impl<'de> serde::de::Visitor<'de> for ObjectVisitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        f.write_str("JSON object")
                    }
                    fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                        $name::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    }
                }
                deserializer.deserialize_map(ObjectVisitor)
            }
        }
    };
}
pub(crate) use object_wire;

pub mod browse;
pub mod codec;
pub mod event;
pub mod ids;
pub mod mcp_settings;
pub mod orchestration;
pub mod request;
pub mod response;
pub mod role_bindings;
pub mod run_details;
pub mod run_state;
pub mod snapshot;
pub mod source_apply;
pub mod source_recovery;
pub mod validate;
pub mod workspace_view;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// IPC wire version.  V2 adds the bounded orchestration projection to a
/// snapshot.  Decoded V1 frames retain their version so a read/serialize
/// bridge does not silently rewrite an older peer's message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolVersion {
    version: u8,
}

/// The version used by locally-created requests, responses, hello messages,
/// and events.  Keeping this value in the value namespace preserves existing
/// `ProtocolVersion` construction sites while the type retains the decoded
/// version internally.
#[allow(non_upper_case_globals)]
pub const ProtocolVersion: ProtocolVersion = ProtocolVersion::V2;

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self::V2
    }
}

impl ProtocolVersion {
    pub const V1: Self = Self { version: 1 };
    pub const V2: Self = Self { version: 2 };

    pub const fn get(self) -> u8 {
        self.version
    }
}

impl Serialize for ProtocolVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.version as u32)
    }
}

impl<'de> Deserialize<'de> for ProtocolVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match u32::deserialize(deserializer)? {
            1 => Ok(Self::V1),
            2 => Ok(Self::V2),
            _ => Err(de::Error::custom("unsupported protocol version")),
        }
    }
}

/// 任意項目は省略可能だが、明示的なnullは値として受理しない。
pub(crate) fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

pub mod local_models;
