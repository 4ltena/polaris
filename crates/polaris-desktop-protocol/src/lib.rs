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

pub mod codec;
pub mod event;
pub mod ids;
pub mod request;
pub mod response;
pub mod run_state;
pub mod snapshot;
pub mod source_apply;
pub mod source_recovery;
pub mod validate;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// M1で対応する版。生成・復号した値は常にversion 1。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProtocolVersion;

impl Serialize for ProtocolVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(1)
    }
}

impl<'de> Deserialize<'de> for ProtocolVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match u32::deserialize(deserializer)? {
            1 => Ok(Self),
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
