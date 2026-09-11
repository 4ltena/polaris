//! 1MiB以下の長さ付きJSONを有限byte列だけで処理し、全階層の重複キーとnullを拒否する。

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, DeserializeOwned, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};
use std::{fmt, io};

pub const MAX_FRAME_BYTES: usize = 1_048_576;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    FrameTooLarge,
    UnexpectedEof,
    InvalidUtf8,
    InvalidJson,
    DuplicateKey,
    Null,
    Schema,
    UnsupportedVersion,
    UnknownEvent,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for CodecError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decode<T> {
    NeedMore,
    Invalid(CodecError),
    Decoded { value: T, consumed: usize },
}

/// 次のフレームのbyteを本体に混ぜない。残りはinput[consumed..]で呼出側が保持する。
pub fn decode<T: DeserializeOwned>(input: &[u8]) -> Decode<T> {
    if input.len() < 4 {
        return Decode::NeedMore;
    }
    let length = u32::from_be_bytes(input[..4].try_into().expect("four-byte header")) as usize;
    if length > MAX_FRAME_BYTES {
        return Decode::Invalid(CodecError::FrameTooLarge);
    }
    let consumed = length + 4;
    if input.len() < consumed {
        return Decode::NeedMore;
    }
    match from_json(&input[4..consumed]) {
        Ok(value) => Decode::Decoded { value, consumed },
        Err(error) => Decode::Invalid(error),
    }
}

/// 入力終了時、空の残りは正常EOF。部分header/bodyは途中EOFとして確定する。
pub fn finish<T: DeserializeOwned>(remaining: &[u8]) -> Result<Option<(T, usize)>, CodecError> {
    if remaining.is_empty() {
        return Ok(None);
    }
    match decode(remaining) {
        Decode::NeedMore => Err(CodecError::UnexpectedEof),
        Decode::Invalid(error) => Err(error),
        Decode::Decoded { value, consumed } => Ok(Some((value, consumed))),
    }
}

fn classify(error: &serde_json::Error, fallback: CodecError) -> CodecError {
    let message = error.to_string();
    if message.starts_with("duplicate JSON key") {
        CodecError::DuplicateKey
    } else if message.starts_with("null is not permitted") {
        CodecError::Null
    } else if message.starts_with("unsupported protocol version") {
        CodecError::UnsupportedVersion
    } else if message.starts_with("unknown state event;") {
        CodecError::UnknownEvent
    } else {
        fallback
    }
}

/// 長さ・UTF-8・JSON構文・重複キーを先に検査してから、型へ変換する受信入口。
/// 深さはserde_jsonの既定recursion limitで制限する。
pub fn from_json<T: DeserializeOwned>(input: &[u8]) -> Result<T, CodecError> {
    if input.len() > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge);
    }
    let text = std::str::from_utf8(input).map_err(|_| CodecError::InvalidUtf8)?;
    let strict: StrictValue =
        serde_json::from_str(text).map_err(|e| classify(&e, CodecError::InvalidJson))?;
    serde_json::from_value(strict.0).map_err(|e| classify(&e, CodecError::Schema))
}

/// std::io::Writeはメモリ内の有界出力先としてのみ使用し、OSのI/Oは行わない。
struct BoundedBuffer {
    bytes: Vec<u8>,
    overflow: bool,
}
impl io::Write for BoundedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_FRAME_BYTES - self.bytes.len() {
            self.overflow = true;
            return Err(io::Error::other("frame limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// JSON本体のbufferは上限を越えて伸ばさず、送信可能な一件のbyte列を返す。
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let mut body = BoundedBuffer {
        bytes: Vec::new(),
        overflow: false,
    };
    if serde_json::to_writer(&mut body, value).is_err() {
        return Err(if body.overflow {
            CodecError::FrameTooLarge
        } else {
            CodecError::Schema
        });
    }
    // 型のSerialize実装からnullなどの不正wireを送り出すことも防ぐ。
    from_json::<StrictValue>(&body.bytes)?;
    let mut frame = Vec::with_capacity(4 + body.bytes.len());
    frame.extend_from_slice(&(body.bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body.bytes);
    Ok(frame)
}

// Valueへ変換する前に各objectのキーを確認する。escape後に同名になるキーも拒否。
pub(crate) struct StrictValue(pub Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct StrictVisitor;
        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("non-null JSON without duplicate keys")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Err(E::custom("null is not permitted"))
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Err(E::custom("null is not permitted"))
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Bool(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Number(v.into())))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Number(v.into())))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                Number::from_f64(v)
                    .map(|n| StrictValue(Value::Number(n)))
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::String(v.into())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::String(v)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element::<StrictValue>()? {
                    values.push(value.0);
                }
                Ok(StrictValue(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom("duplicate JSON key"));
                    }
                    let value = map.next_value::<StrictValue>()?;
                    values.insert(key, value.0);
                }
                Ok(StrictValue(Value::Object(values)))
            }
        }
        d.deserialize_any(StrictVisitor)
    }
}
