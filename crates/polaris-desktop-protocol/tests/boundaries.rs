//! ID・u64の値境界、有限frameの全分割とEOF、不正UTF-8・JSON・深さ上限を検査する。

use polaris_desktop_protocol::{
    codec::{self, CodecError, Decode, MAX_FRAME_BYTES},
    ids::*,
    request::Request,
};
use serde_json::Value;

#[test]
fn all_id_types_enforce_utf8_byte_limits() {
    macro_rules! check {
        ($($id:ident),+ $(,)?) => {$ (
            assert!($id::new("").is_err());
            for value in ["a".to_owned(), "a".repeat(127), "a".repeat(128), "日".repeat(42), format!("{}ab", "日".repeat(42))] {
                let id = $id::new(value.clone()).unwrap();
                assert_eq!(id.as_str(), value);
                assert_eq!(serde_json::to_value(&id).unwrap(), Value::String(value));
                assert_eq!(serde_json::from_str::<$id>(&serde_json::to_string(&id).unwrap()).unwrap(), id);
            }
            assert!($id::new("a".repeat(129)).is_err());
            assert!($id::new("日".repeat(43)).is_err());
            assert!(serde_json::from_str::<$id>("3").is_err());
            assert!(serde_json::from_str::<$id>("null").is_err());
        )+};
    }
    check!(
        ProjectId,
        SessionId,
        ClientId,
        RequestId,
        EngineEpoch,
        RunId,
        AttemptId,
        TaskId,
        SubscriptionId,
        ApprovalId,
        MessageId,
        AttachmentId,
        SnapshotId,
        HistoryCursor,
        OperationId,
        ResultId
    );
}

#[test]
fn decimal_u64_retains_precision_and_rejects_noncanonical_input() {
    for value in [0, 1, (1 << 53) - 1, 1 << 53, (1 << 53) + 1, u64::MAX] {
        let expected = format!("\"{value}\"");
        let number = DecimalU64::new(value);
        assert_eq!(serde_json::to_string(&number).unwrap(), expected);
        assert_eq!(
            codec::from_json::<DecimalU64>(expected.as_bytes())
                .unwrap()
                .get(),
            value
        );
    }
    for wire in [
        r#""""#,
        r#""00""#,
        r#""01""#,
        r#""-1""#,
        r#""+1""#,
        r#"" 1""#,
        r#""1 ""#,
        r#""1.0""#,
        r#""1e3""#,
        r#""１""#,
        r#""18446744073709551616""#,
        r#""99999999999999999999999999999999999999""#,
        "0",
        "1.0",
        "1e3",
        "-1",
        "null",
        "true",
    ] {
        assert!(
            codec::from_json::<DecimalU64>(wire.as_bytes()).is_err(),
            "accepted {wire}"
        );
    }
    assert_eq!(
        DecimalU64::new(u64::MAX).checked_add(1),
        Err(ValueError::Overflow)
    );
    assert_eq!(
        DecimalU64::new(u64::MAX - 1).checked_add(1).unwrap().get(),
        u64::MAX
    );
}

fn frame(body: &[u8]) -> Vec<u8> {
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(body);
    frame
}

#[test]
fn header_and_body_every_split_need_more_then_decode_once() {
    let request = br#"{"protocol_version":1,"kind":"request","client_id":"c","request_id":"r","method":"hello","params":{}}"#;
    let frame = frame(request);
    for split in 0..frame.len() {
        assert_eq!(
            codec::decode::<Request>(&frame[..split]),
            Decode::NeedMore,
            "split={split}"
        );
        if split == 0 {
            assert_eq!(codec::finish::<Request>(&frame[..split]), Ok(None));
        } else {
            assert_eq!(
                codec::finish::<Request>(&frame[..split]),
                Err(CodecError::UnexpectedEof)
            );
        }
    }
    assert!(
        matches!(codec::decode::<Request>(&frame), Decode::Decoded { consumed, .. } if consumed == frame.len())
    );
    assert!(codec::finish::<Request>(&frame).unwrap().is_some());
    let mut joined = frame.clone();
    joined.extend_from_slice(&frame);
    joined.extend_from_slice(&[0, 0]);
    let Decode::Decoded {
        consumed: first, ..
    } = codec::decode::<Request>(&joined)
    else {
        panic!("first")
    };
    let Decode::Decoded {
        consumed: second, ..
    } = codec::decode::<Request>(&joined[first..])
    else {
        panic!("second")
    };
    assert_eq!((first, second), (frame.len(), frame.len()));
    assert_eq!(
        codec::finish::<Request>(&joined[first + second..]),
        Err(CodecError::UnexpectedEof)
    );
}

#[test]
fn frame_zero_one_exact_limit_and_over_limit() {
    assert_eq!(
        codec::decode::<Value>(&frame(b"")),
        Decode::Invalid(CodecError::InvalidJson)
    );
    assert_eq!(
        codec::decode::<Value>(&frame(b"1")),
        Decode::Decoded {
            value: Value::from(1),
            consumed: 5
        }
    );
    assert_eq!(
        codec::decode::<Value>(&frame(b"{")),
        Decode::Invalid(CodecError::InvalidJson)
    );
    let text = "a".repeat(MAX_FRAME_BYTES - 2);
    let encoded = codec::encode(&text).unwrap();
    assert_eq!(encoded.len(), MAX_FRAME_BYTES + 4);
    assert_eq!(&encoded[..4], &(MAX_FRAME_BYTES as u32).to_be_bytes());
    assert_eq!(
        codec::decode::<String>(&encoded),
        Decode::Decoded {
            value: text,
            consumed: encoded.len()
        }
    );
    for size in [MAX_FRAME_BYTES as u32 + 1, u32::MAX] {
        let header = size.to_be_bytes();
        assert_eq!(
            codec::decode::<Value>(&header),
            Decode::Invalid(CodecError::FrameTooLarge)
        );
        assert_eq!(
            codec::finish::<Value>(&header),
            Err(CodecError::FrameTooLarge)
        );
    }
    assert_eq!(
        codec::encode(&"a".repeat(MAX_FRAME_BYTES - 1)),
        Err(CodecError::FrameTooLarge)
    );
    assert_eq!(
        codec::from_json::<Value>(&vec![b' '; MAX_FRAME_BYTES + 1]),
        Err(CodecError::FrameTooLarge)
    );
}

#[test]
fn invalid_utf8_json_trailing_content_and_depth_are_rejected() {
    let japanese = frame("\"日本語\"".as_bytes());
    for split in 4..japanese.len() {
        assert_eq!(
            codec::decode::<String>(&japanese[..split]),
            Decode::NeedMore
        );
    }
    assert!(
        matches!(codec::decode::<String>(&japanese), Decode::Decoded { value, .. } if value == "日本語")
    );
    for invalid in [&[0xff][..], &[b'"', 0xe6, 0x97, b'"'][..]] {
        assert_eq!(
            codec::decode::<Value>(&frame(invalid)),
            Decode::Invalid(CodecError::InvalidUtf8)
        );
    }
    for invalid in ["{", "{}{}", "{} trailing", "[1,]", "NaN", r#""\uD800""#] {
        assert_eq!(
            codec::from_json::<Value>(invalid.as_bytes()),
            Err(CodecError::InvalidJson),
            "{invalid}"
        );
    }
    let deep = format!("{}0{}", "[".repeat(160), "]".repeat(160));
    assert_eq!(
        codec::from_json::<Value>(deep.as_bytes()),
        Err(CodecError::InvalidJson)
    );
    assert_eq!(codec::encode(&Value::Null), Err(CodecError::Null));
}
