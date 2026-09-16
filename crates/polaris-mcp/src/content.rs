//! SDK-independent result variants. Binary content is never serialized into a
//! model message; the host retains it in a bounded run-local handle instead.

use crate::Error;
use base64::Engine;
use serde_json::Value;

const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Complete,
    ToolError,
    InputRequired,
    UnsupportedContent,
}

pub enum Part {
    Text {
        text: String,
        uri: Option<String>,
        mime: Option<String>,
    },
    Structured(Value),
    ResourceLink {
        uri: String,
        name: String,
        mime: Option<String>,
    },
    Binary {
        bytes: Vec<u8>,
        mime: String,
        uri: Option<String>,
    },
    /// Retain an unsupported input request as data, with no automatic reply.
    InputRequired(Value),
    Unsupported {
        kind: String,
    },
}

pub struct Reply {
    pub status: Status,
    pub parts: Vec<Part>,
}

/// Kept private to the SDK adapter. `wire` was already bounded/redacted by the
/// transport, and is checked again before conversion and binary allocation.
pub(crate) fn decode(wire: Value, kind: crate::catalog::Kind) -> Result<Reply, Error> {
    let encoded = crate::registration::bounded_json(&wire, MAX_BYTES)?;
    let mut remaining = MAX_BYTES;
    let mut reply = Reply {
        status: Status::Complete,
        parts: Vec::new(),
    };
    let object = wire.as_object().ok_or(Error::Protocol)?;
    match object.get("resultType").and_then(Value::as_str) {
        Some("input_required") => {
            reply.status = Status::InputRequired;
            reply.parts.push(Part::InputRequired(wire));
            return Ok(reply);
        }
        None | Some("complete") => {}
        Some(_) => {
            reply.status = Status::UnsupportedContent;
            reply.parts.push(Part::Unsupported {
                kind: "result_type".into(),
            });
            return Ok(reply);
        }
    }
    // Decode the known envelope through the SDK, retaining its content union.
    match kind {
        crate::catalog::Kind::Tool => {
            let result: rmcp::model::CallToolResult =
                serde_json::from_slice(&encoded).map_err(|_| Error::Protocol)?;
            if result.is_error == Some(true) {
                reply.status = Status::ToolError;
            }
            for content in result.content {
                append(content, &mut reply, &mut remaining)?;
            }
            if let Some(value) = result.structured_content {
                charge(
                    &mut remaining,
                    serde_json::to_vec(&value)
                        .map_err(|_| Error::Protocol)?
                        .len(),
                )?;
                reply.parts.push(Part::Structured(value));
            }
        }
        crate::catalog::Kind::Resource | crate::catalog::Kind::ResourceTemplate => {
            let result: rmcp::model::ReadResourceResult =
                serde_json::from_slice(&encoded).map_err(|_| Error::Protocol)?;
            for content in result.contents {
                append_resource(content, &mut reply, &mut remaining)?;
            }
        }
        crate::catalog::Kind::Prompt => {
            let result: rmcp::model::GetPromptResult =
                serde_json::from_slice(&encoded).map_err(|_| Error::Protocol)?;
            if let Some(description) = result.description {
                charge(&mut remaining, description.len())?;
                reply.parts.push(Part::Text {
                    text: description,
                    uri: None,
                    mime: None,
                });
            }
            for message in result.messages {
                // A prompt's server-provided role is data, never a system or
                // assistant role in provider history. Host marks the whole
                // reply as untrusted user-selected content.
                append(message.content, &mut reply, &mut remaining)?;
            }
        }
    }
    Ok(reply)
}

fn charge(remaining: &mut usize, size: usize) -> Result<(), Error> {
    *remaining = remaining.checked_sub(size).ok_or(Error::Limit)?;
    Ok(())
}

fn append(
    content: rmcp::model::ContentBlock,
    reply: &mut Reply,
    remaining: &mut usize,
) -> Result<(), Error> {
    use rmcp::model::ContentBlock;
    match content {
        ContentBlock::Text(content) => {
            charge(remaining, content.text.len())?;
            reply.parts.push(Part::Text {
                text: content.text,
                uri: None,
                mime: None,
            });
        }
        ContentBlock::Image(content) => {
            binary(content.data, content.mime_type, None, reply, remaining)?
        }
        ContentBlock::Audio(content) => {
            binary(content.data, content.mime_type, None, reply, remaining)?
        }
        ContentBlock::Resource(content) => append_resource(content.resource, reply, remaining)?,
        ContentBlock::ResourceLink(content) => {
            charge(
                remaining,
                content.uri.len()
                    + content.name.len()
                    + content.mime_type.as_ref().map_or(0, String::len),
            )?;
            reply.parts.push(Part::ResourceLink {
                uri: content.uri,
                name: content.name,
                mime: content.mime_type,
            });
        }
        _ => {
            reply.status = Status::UnsupportedContent;
            reply.parts.push(Part::Unsupported {
                kind: "content".into(),
            });
        }
    }
    Ok(())
}

fn append_resource(
    content: rmcp::model::ResourceContents,
    reply: &mut Reply,
    remaining: &mut usize,
) -> Result<(), Error> {
    match content {
        rmcp::model::ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            ..
        } => {
            charge(
                remaining,
                uri.len() + mime_type.as_ref().map_or(0, String::len) + text.len(),
            )?;
            reply.parts.push(Part::Text {
                text,
                uri: Some(uri),
                mime: mime_type,
            });
        }
        rmcp::model::ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            ..
        } => {
            binary(
                blob,
                mime_type.unwrap_or_else(|| "application/octet-stream".into()),
                Some(uri),
                reply,
                remaining,
            )?;
        }
        _ => {
            reply.status = Status::UnsupportedContent;
            reply.parts.push(Part::Unsupported {
                kind: "resource".into(),
            });
        }
    }
    Ok(())
}

fn binary(
    data: String,
    mime: String,
    uri: Option<String>,
    reply: &mut Reply,
    remaining: &mut usize,
) -> Result<(), Error> {
    let estimated = data.len().checked_add(3).ok_or(Error::Limit)? / 4 * 3;
    if estimated > MAX_BYTES || data.len() > MAX_BYTES {
        return Err(Error::Limit);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| Error::Protocol)?;
    charge(
        remaining,
        bytes.len() + mime.len() + uri.as_ref().map_or(0, String::len),
    )?;
    reply.parts.push(Part::Binary { bytes, mime, uri });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Kind;
    use serde_json::json;

    #[test]
    fn text_json_links_and_nontext_keep_their_types_without_fetching() {
        let reply = decode(
            json!({"content":[
            {"type":"text","text":"日本語"},
            {"type":"image","data":"AP8=","mimeType":"image/png"},
            {"type":"audio","data":"AQI=","mimeType":"audio/wav"},
            {"type":"resource","resource":{"uri":"custom://raw","text":"original"}},
            {"type":"resource_link","uri":"https://invalid.example/no-fetch","name":"link"}
        ],"structuredContent":{"count":3}}),
            Kind::Tool,
        )
        .unwrap();
        assert_eq!(reply.status, Status::Complete);
        assert_eq!(reply.parts.len(), 6);
        assert!(matches!(&reply.parts[0], Part::Text {text,..} if text == "日本語"));
        assert!(
            matches!(&reply.parts[1], Part::Binary {bytes,mime,..} if bytes == &[0,255] && mime == "image/png")
        );
        assert!(
            matches!(&reply.parts[2], Part::Binary {bytes,mime,..} if bytes == &[1,2] && mime == "audio/wav")
        );
        assert!(
            matches!(&reply.parts[3], Part::Text {uri:Some(uri),text,..} if uri == "custom://raw" && text == "original")
        );
        assert!(
            matches!(&reply.parts[4], Part::ResourceLink {uri,..} if uri == "https://invalid.example/no-fetch")
        );
        assert!(matches!(&reply.parts[5], Part::Structured(value) if value["count"] == 3));
    }

    #[test]
    fn incomplete_error_unknown_and_invalid_binary_never_become_complete() {
        for kind in [Kind::Tool, Kind::Resource, Kind::Prompt] {
            let reply = decode(
                json!({"resultType":"input_required","inputRequests":{"answer":{}}}),
                kind,
            )
            .unwrap();
            assert_eq!(reply.status, Status::InputRequired);
            assert!(matches!(&reply.parts[0], Part::InputRequired(_)));
        }
        assert_eq!(
            decode(json!({"content":[],"isError":true}), Kind::Tool)
                .unwrap()
                .status,
            Status::ToolError
        );
        assert_eq!(
            decode(json!({"resultType":"task"}), Kind::Tool)
                .unwrap()
                .status,
            Status::UnsupportedContent
        );
        assert!(
            decode(
                json!({"content":[{"type":"image","data":"!invalid!","mimeType":"image/png"}]}),
                Kind::Tool
            )
            .is_err()
        );
        assert!(
            decode(
                json!({"content":[{"type":"text","text":"x".repeat(MAX_BYTES)}]}),
                Kind::Tool
            )
            .is_err()
        );
    }
}
