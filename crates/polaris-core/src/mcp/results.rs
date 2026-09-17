//! Bounded, nonpersistent result bytes. Handles are host-generated and usable
//! only in their originating run/attempt; reading never performs remote work.

use super::Error;
use polaris_mcp::content::{Part, Reply, Status};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
    time::{Duration, Instant},
};

pub const MAX_RESULT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_VISIBLE_BYTES: usize = 32 * 1024;
const MAX_HANDLES: usize = 16;
const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(15 * 60);
const MAX_REPLY_PARTS: usize = MAX_VISIBLE_BYTES / 16;
const MAX_JSON_DEPTH: usize = 64;
const MAX_JSON_NODES: usize = 16_384;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunBinding {
    pub session: String,
    pub run: String,
    pub attempt: String,
}

/// These are source identities provided by the host, never model arguments.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Source {
    pub server: String,
    #[serde(flatten)]
    pub origin: SourceOrigin,
    pub result_generation: String,
}

/// A host selection is provenance for untrusted content, never a model tool
/// call or execution grant. Flattening preserves the existing model-call wire.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SourceOrigin {
    ModelCall { model_call: String },
    HostSelection { selection_id: String },
}

impl SourceOrigin {
    fn id(&self) -> &str {
        match self {
            Self::ModelCall { model_call } => model_call,
            Self::HostSelection { selection_id } => selection_id,
        }
    }
}

pub enum Payload {
    Text(String),
    Binary { bytes: Vec<u8>, mime: String },
}

struct Entry {
    // The originating call and result generation never change. A later model
    // call in the same run may read this exact entry through its opaque handle.
    source: Source,
    payload: Payload,
    content_type: String,
    created: Instant,
    bytes: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Chunk {
    pub handle: String,
    pub source: Source,
    pub content_type: String,
    pub bytes: usize,
    pub offset: usize,
    pub next_offset: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
}

/// The single, bounded representation copied into the next model request.
/// Every part keeps the MCP reply order and is either inline or a reference to
/// a run-local handle. Source is host-provided provenance for the whole reply.
#[derive(Debug, Serialize)]
pub struct ReplyProjection {
    pub source: Source,
    pub status: &'static str,
    pub parts: Vec<ProjectedPart>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectedPart {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
    },
    Structured {
        value: serde_json::Value,
    },
    ResourceLink {
        uri: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
    },
    InputRequired {
        request: serde_json::Value,
    },
    Unsupported {
        unsupported: String,
    },
    Retained {
        original_kind: &'static str,
        handle: String,
        content_type: String,
        bytes: usize,
        excerpt: String,
        next_offset: usize,
        truncated: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
    },
    Binary {
        mime: String,
        bytes: usize,
        handle: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
        status: &'static str,
    },
}

enum RetainedPayload {
    Text(String),
    Binary { bytes: Vec<u8>, mime: String },
}

struct Retained {
    original_kind: &'static str,
    payload: RetainedPayload,
    content_type: String,
    uri: Option<String>,
    mime: Option<String>,
}

struct DraftPart {
    visible: ProjectedPart,
    retained: Option<Retained>,
    selected: bool,
}

impl Retained {
    fn bytes(&self) -> usize {
        match &self.payload {
            RetainedPayload::Text(text) => text.len(),
            RetainedPayload::Binary { bytes, .. } => bytes.len(),
        }
    }

    fn visible(&self, handle: String) -> ProjectedPart {
        if self.original_kind == "binary" {
            return ProjectedPart::Binary {
                mime: self
                    .mime
                    .clone()
                    .unwrap_or_else(|| self.content_type.clone()),
                bytes: self.bytes(),
                handle,
                uri: self.uri.clone(),
                status: "unsupported_modality",
            };
        }
        ProjectedPart::Retained {
            original_kind: self.original_kind,
            handle,
            content_type: self.content_type.clone(),
            bytes: self.bytes(),
            excerpt: String::new(),
            next_offset: 0,
            truncated: self.bytes() > 0,
            uri: self.uri.clone(),
            mime: self.mime.clone(),
        }
    }

    fn into_entry(self, source: Source, created: Instant) -> Entry {
        let bytes = self.bytes();
        let payload = match self.payload {
            RetainedPayload::Text(text) => Payload::Text(text),
            RetainedPayload::Binary { bytes, mime } => Payload::Binary { bytes, mime },
        };
        Entry {
            source,
            payload,
            content_type: self.content_type,
            created,
            bytes,
        }
    }
}

/// The execution owner drops or clears the store on completion. No Clone,
/// serialization, resume or fork restoration is provided.
pub struct ResultStore {
    binding: RunBinding,
    entries: BTreeMap<String, Entry>,
    closed: bool,
}

fn identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

impl ResultStore {
    pub fn new(binding: RunBinding) -> Result<Self, Error> {
        if ![&binding.session, &binding.run, &binding.attempt]
            .into_iter()
            .all(|s| identifier(s))
        {
            return Err(Error::ResultUnavailable);
        }
        Ok(Self {
            binding,
            entries: BTreeMap::new(),
            closed: false,
        })
    }

    pub fn insert(
        &mut self,
        binding: &RunBinding,
        source: Source,
        payload: Payload,
        now: Instant,
    ) -> Result<Chunk, Error> {
        self.check_binding(binding)?;
        validate_source(&source)?;
        let bytes = match &payload {
            Payload::Text(text) => text.len(),
            Payload::Binary { bytes, mime } => {
                if !identifier(mime) {
                    return Err(Error::ResultUnavailable);
                }
                bytes.len()
            }
        };
        if bytes > MAX_RESULT_BYTES {
            return Err(Error::ContextLimit);
        }
        self.expire(now);
        if self.entries.len() >= MAX_HANDLES
            || self
                .entries
                .values()
                .map(|entry| entry.bytes)
                .sum::<usize>()
                + bytes
                > MAX_RETAINED_BYTES
        {
            return Err(Error::ContextLimit);
        }
        let mut random = [0u8; 24];
        getrandom::fill(&mut random).map_err(|_| Error::ResultUnavailable)?;
        let handle: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        if self.entries.contains_key(&handle) {
            return Err(Error::ResultUnavailable);
        }
        let entry = Entry {
            source,
            payload,
            content_type: "text/plain; charset=utf-8".into(),
            created: now,
            bytes,
        };
        // Project before storing, so malformed framing cannot leave an unseen
        // handle consuming the capacity of this run.
        let chunk = project(&handle, &entry, 0, MAX_VISIBLE_BYTES)?;
        self.entries.insert(handle, entry);
        Ok(chunk)
    }

    /// Projects an SDK-independent reply into one provider-safe envelope.
    /// Large or binary parts are retained only after the complete envelope and
    /// all capacity checks succeed, so a rejected reply leaves no new handles.
    pub fn project_reply(
        &mut self,
        binding: &RunBinding,
        source: Source,
        reply: Reply,
        now: Instant,
    ) -> Result<ReplyProjection, Error> {
        self.project_reply_bounded(binding, source, reply, now, MAX_VISIBLE_BYTES)
    }

    /// The trusted publisher reserves its proof framing before this projection.
    pub(crate) fn project_reply_bounded(
        &mut self,
        binding: &RunBinding,
        source: Source,
        reply: Reply,
        now: Instant,
        visible_limit: usize,
    ) -> Result<ReplyProjection, Error> {
        self.check_binding(binding)?;
        if !(1..=MAX_VISIBLE_BYTES).contains(&visible_limit) {
            return Err(Error::ContextLimit);
        }
        validate_source(&source)?;
        self.expire(now);
        if reply.parts.len() > MAX_REPLY_PARTS {
            return Err(Error::ContextLimit);
        }
        if reply_payload_bytes(&reply)? > MAX_RESULT_BYTES {
            return Err(Error::ContextLimit);
        }

        let mut parts = reply
            .parts
            .into_iter()
            .map(draft_part)
            .collect::<Result<Vec<_>, Error>>()?;
        let status = reply_status(reply.status);
        let placeholder = "0".repeat(48);
        let mut projection = ReplyProjection {
            source: source.clone(),
            status,
            parts: parts.iter().map(|part| part.visible.clone()).collect(),
        };
        let remaining_handles = MAX_HANDLES
            .checked_sub(self.entries.len())
            .ok_or(Error::ContextLimit)?;
        let mut selected_handles = parts.iter().filter(|part| part.selected).count();
        if selected_handles > remaining_handles {
            return Err(Error::ContextLimit);
        }

        // Binary data always remains out of the String-only provider message.
        // If inline data overflows the envelope, move the largest retainable
        // part first; this reaches a bounded representation without dropping a
        // variant or silently altering its contents.
        loop {
            if serialized_len(&projection)? <= visible_limit {
                break;
            }
            if selected_handles >= remaining_handles {
                return Err(Error::ContextLimit);
            }
            let index = parts
                .iter()
                .enumerate()
                .filter(|(_, part)| !part.selected && part.retained.is_some())
                .max_by_key(|(_, part)| serialized_len(&part.visible).unwrap_or(usize::MAX))
                .map(|(index, _)| index)
                .ok_or(Error::ContextLimit)?;
            let retained = parts[index].retained.as_ref().expect("selected above");
            parts[index].visible = retained.visible(placeholder.clone());
            parts[index].selected = true;
            selected_handles += 1;
            projection.parts[index] = parts[index].visible.clone();
        }

        let needed = parts
            .iter()
            .filter(|part| part.selected)
            .filter_map(|part| part.retained.as_ref())
            .collect::<Vec<_>>();
        let bytes = needed.iter().try_fold(0usize, |total, retained| {
            total
                .checked_add(retained.bytes())
                .ok_or(Error::ContextLimit)
        })?;
        let retained_bytes = self.entries.values().try_fold(0usize, |total, entry| {
            total.checked_add(entry.bytes).ok_or(Error::ContextLimit)
        })?;
        if self.entries.len() + needed.len() > MAX_HANDLES
            || retained_bytes
                .checked_add(bytes)
                .is_none_or(|total| total > MAX_RETAINED_BYTES)
        {
            return Err(Error::ContextLimit);
        }

        let mut handles = BTreeSet::new();
        for _ in 0..needed.len() {
            let handle = new_handle()?;
            if self.entries.contains_key(&handle) || !handles.insert(handle) {
                return Err(Error::ResultUnavailable);
            }
        }
        let mut handles = handles.into_iter();
        let mut entries = Vec::new();
        for (index, part) in parts.iter_mut().enumerate() {
            if !part.selected {
                continue;
            }
            let Some(retained) = part.retained.take() else {
                continue;
            };
            let handle = handles.next().expect("one handle per retained part");
            part.visible = retained.visible(handle.clone());
            projection.parts[index] = part.visible.clone();
            let entry = retained.into_entry(source.clone(), now);
            // A retained part must be locally readable before it can consume a
            // handle. This also keeps hostile MIME or provenance framing from
            // creating a handle that can never produce a bounded chunk.
            project(&handle, &entry, 0, MAX_VISIBLE_BYTES)?;
            entries.push((index, handle, entry));
        }
        add_excerpts(&mut projection, &entries, visible_limit)?;
        if serialized_len(&projection)? > visible_limit {
            return Err(Error::ContextLimit);
        }
        for (_, handle, entry) in entries {
            self.entries.insert(handle, entry);
        }
        Ok(projection)
    }

    pub fn chunk(
        &mut self,
        binding: &RunBinding,
        handle: &str,
        offset: usize,
        limit: usize,
        now: Instant,
    ) -> Result<Chunk, Error> {
        self.check_binding(binding)?;
        self.expire(now);
        let entry = self.entries.get(handle).ok_or(Error::ResultUnavailable)?;
        project(handle, entry, offset, limit)
    }

    /// Host UI access to retained binary bytes. This is not the String-only
    /// provider projection, and must not be converted to a base64 model message.
    pub fn binary_chunk(
        &mut self,
        binding: &RunBinding,
        handle: &str,
        offset: usize,
        limit: usize,
        now: Instant,
    ) -> Result<&[u8], Error> {
        self.check_binding(binding)?;
        self.expire(now);
        let entry = self.entries.get(handle).ok_or(Error::ResultUnavailable)?;
        if limit == 0 || limit > MAX_VISIBLE_BYTES || offset > entry.bytes {
            return Err(Error::ResultUnavailable);
        }
        match &entry.payload {
            Payload::Binary { bytes, .. } => {
                Ok(&bytes[offset..offset.saturating_add(limit).min(bytes.len())])
            }
            _ => Err(Error::ResultUnavailable),
        }
    }

    pub fn finish(&mut self) {
        self.closed = true;
        self.entries.clear();
    }

    pub(crate) fn check_binding(&self, binding: &RunBinding) -> Result<(), Error> {
        if self.closed || &self.binding != binding {
            Err(Error::ResultUnavailable)
        } else {
            Ok(())
        }
    }

    fn expire(&mut self, now: Instant) {
        self.entries.retain(|_, entry| {
            now.checked_duration_since(entry.created)
                .is_some_and(|age| age < TTL)
        });
    }
}

fn validate_source(source: &Source) -> Result<(), Error> {
    if [
        source.server.as_str(),
        source.origin.id(),
        source.result_generation.as_str(),
    ]
    .into_iter()
    .all(identifier)
    {
        Ok(())
    } else {
        Err(Error::ResultUnavailable)
    }
}

fn reply_status(status: Status) -> &'static str {
    match status {
        Status::Complete => "complete",
        Status::ToolError => "tool_error",
        Status::InputRequired => "input_required",
        Status::UnsupportedContent => "unsupported_content",
    }
}

/// `polaris-mcp` already bounds decoded transport replies, but this public
/// host entry also accepts synthetic replies. Charge every retained or inline
/// input field before cloning or serializing the projection.
fn reply_payload_bytes(reply: &Reply) -> Result<usize, Error> {
    reply.parts.iter().try_fold(0usize, |total, part| {
        let bytes = match part {
            Part::Text { text, uri, mime } => text
                .len()
                .checked_add(uri.as_ref().map_or(0, String::len))
                .and_then(|value| value.checked_add(mime.as_ref().map_or(0, String::len))),
            Part::Structured(value) | Part::InputRequired(value) => Some(bounded_json_len(value)?),
            Part::ResourceLink { uri, name, mime } => uri
                .len()
                .checked_add(name.len())
                .and_then(|value| value.checked_add(mime.as_ref().map_or(0, String::len))),
            Part::Binary { bytes, mime, uri } => bytes
                .len()
                .checked_add(mime.len())
                .and_then(|value| value.checked_add(uri.as_ref().map_or(0, String::len))),
            Part::Unsupported { kind } => Some(kind.len()),
        }
        .ok_or(Error::ContextLimit)?;
        total.checked_add(bytes).ok_or(Error::ContextLimit)
    })
}

fn serialized_len(value: &impl Serialize) -> Result<usize, Error> {
    serde_json::to_vec(value)
        .map(|value| value.len())
        .map_err(|_| Error::ResultUnavailable)
}

fn new_handle() -> Result<String, Error> {
    let mut random = [0u8; 24];
    getrandom::fill(&mut random).map_err(|_| Error::ResultUnavailable)?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn bounded_json_len(value: &serde_json::Value) -> Result<usize, Error> {
    let mut pending = vec![(value, 0usize)];
    let mut nodes = 0usize;
    while let Some((value, depth)) = pending.pop() {
        nodes = nodes.checked_add(1).ok_or(Error::ContextLimit)?;
        if depth > MAX_JSON_DEPTH || nodes > MAX_JSON_NODES {
            return Err(Error::ContextLimit);
        }
        match value {
            serde_json::Value::Array(values) => {
                if values.len() > MAX_JSON_NODES - nodes {
                    return Err(Error::ContextLimit);
                }
                pending.extend(values.iter().map(|value| (value, depth + 1)))
            }
            serde_json::Value::Object(values) => {
                if values.len() > MAX_JSON_NODES - nodes {
                    return Err(Error::ContextLimit);
                }
                pending.extend(values.values().map(|value| (value, depth + 1)))
            }
            _ => {}
        }
    }
    let mut writer = CountingWriter { bytes: 0 };
    let mut serializer = serde_json::Serializer::new(&mut writer);
    serde::Serialize::serialize(value, &mut serializer).map_err(|_| Error::ContextLimit)?;
    Ok(writer.bytes)
}

struct CountingWriter {
    bytes: usize,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::other("json overflow"))?;
        if self.bytes > MAX_RESULT_BYTES {
            return Err(io::Error::other("json limit"));
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_value(value: &serde_json::Value) -> Result<String, Error> {
    bounded_json_len(value)?;
    serde_json::to_string(value).map_err(|_| Error::ResultUnavailable)
}

fn safe_uri(uri: String) -> String {
    match uri.find(['?', '#']) {
        Some(end) => uri[..end].into(),
        None => uri,
    }
}

fn draft_part(part: Part) -> Result<DraftPart, Error> {
    let (visible, retained) = match part {
        Part::Text { text, uri, mime } => (
            ProjectedPart::Text {
                text: text.clone(),
                uri: uri.clone().map(safe_uri),
                mime: mime.clone(),
            },
            Some(Retained {
                original_kind: "text",
                payload: RetainedPayload::Text(text),
                content_type: mime
                    .clone()
                    .unwrap_or_else(|| "text/plain; charset=utf-8".into()),
                uri: uri.map(safe_uri),
                mime,
            }),
        ),
        Part::Structured(value) => {
            let text = serialized_value(&value)?;
            (
                ProjectedPart::Structured { value },
                Some(Retained {
                    original_kind: "structured",
                    payload: RetainedPayload::Text(text),
                    content_type: "application/json".into(),
                    uri: None,
                    mime: Some("application/json".into()),
                }),
            )
        }
        Part::ResourceLink { uri, name, mime } => {
            let uri = safe_uri(uri);
            let text = serde_json::to_string(&serde_json::json!({
                "uri": uri.clone(),
                "name": name.clone(),
                "mime": mime.clone(),
            }))
            .map_err(|_| Error::ResultUnavailable)?;
            (
                ProjectedPart::ResourceLink { uri, name, mime },
                Some(Retained {
                    original_kind: "resource_link",
                    payload: RetainedPayload::Text(text),
                    content_type: "application/json".into(),
                    uri: None,
                    mime: Some("application/json".into()),
                }),
            )
        }
        Part::Binary { bytes, mime, uri } => {
            let uri = uri.map(safe_uri);
            let retained = Retained {
                original_kind: "binary",
                payload: RetainedPayload::Binary {
                    bytes,
                    mime: mime.clone(),
                },
                content_type: mime.clone(),
                uri: uri.clone(),
                mime: Some(mime.clone()),
            };
            (
                ProjectedPart::Binary {
                    mime,
                    bytes: retained.bytes(),
                    handle: "0".repeat(48),
                    uri,
                    status: "unsupported_modality",
                },
                Some(retained),
            )
        }
        Part::InputRequired(value) => {
            let text = serialized_value(&value)?;
            (
                ProjectedPart::InputRequired { request: value },
                Some(Retained {
                    original_kind: "input_required",
                    payload: RetainedPayload::Text(text),
                    content_type: "application/json".into(),
                    uri: None,
                    mime: Some("application/json".into()),
                }),
            )
        }
        Part::Unsupported { kind } => (ProjectedPart::Unsupported { unsupported: kind }, None),
    };
    if retained
        .as_ref()
        .is_some_and(|part| part.bytes() > MAX_RESULT_BYTES)
    {
        return Err(Error::ContextLimit);
    }
    let selected = matches!(&visible, ProjectedPart::Binary { .. });
    Ok(DraftPart {
        visible,
        retained,
        selected,
    })
}

/// Adds a first, UTF-8-safe excerpt for every retained text part. The empty
/// fields are already in the projection before this runs, so reserving one
/// character per nonempty part proves that every reference remains useful.
/// Remaining envelope bytes are then divided among the retained parts.
fn add_excerpts(
    projection: &mut ReplyProjection,
    entries: &[(usize, String, Entry)],
    visible_limit: usize,
) -> Result<(), Error> {
    let text_entries = entries
        .iter()
        .enumerate()
        .filter(|(_, (_, _, entry))| matches!(&entry.payload, Payload::Text(_)))
        .map(|(entry_index, _)| entry_index)
        .collect::<Vec<_>>();
    for entry_index in &text_entries {
        let (index, handle, entry) = &entries[*entry_index];
        let Payload::Text(text) = &entry.payload else {
            continue;
        };
        if text.is_empty() {
            set_excerpt(&mut projection.parts[*index], project(handle, entry, 0, 1)?)?;
            continue;
        }
        let first = text.chars().next().expect("nonempty text").len_utf8();
        set_excerpt(
            &mut projection.parts[*index],
            project(handle, entry, 0, first)?,
        )?;
        if serialized_len(projection)? > visible_limit {
            return Err(Error::ContextLimit);
        }
    }

    for (position, entry_index) in text_entries.iter().enumerate() {
        let (index, handle, entry) = &entries[*entry_index];
        let Payload::Text(text) = &entry.payload else {
            continue;
        };
        let current = retained_offset(&projection.parts[*index])?;
        if current >= text.len() {
            continue;
        }
        let remaining_parts = text_entries.len() - position;
        let remaining = visible_limit.saturating_sub(serialized_len(projection)?);
        let upper = current
            .saturating_add(remaining / remaining_parts)
            .min(text.len());
        if upper <= current {
            continue;
        }
        let chunk = largest_excerpt(
            projection,
            *index,
            handle,
            entry,
            current,
            upper,
            visible_limit,
        )?;
        set_excerpt(&mut projection.parts[*index], chunk)?;
    }
    Ok(())
}

fn largest_excerpt(
    projection: &ReplyProjection,
    index: usize,
    handle: &str,
    entry: &Entry,
    current: usize,
    upper: usize,
    visible_limit: usize,
) -> Result<Chunk, Error> {
    let mut low = current;
    let mut high = upper;
    let mut best = project(handle, entry, 0, current)?;
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        let candidate = project(handle, entry, 0, middle)?;
        let mut trial = ReplyProjection {
            source: projection.source.clone(),
            status: projection.status,
            parts: projection.parts.clone(),
        };
        set_excerpt(&mut trial.parts[index], candidate.clone())?;
        if serialized_len(&trial)? <= visible_limit {
            low = middle;
            best = candidate;
        } else {
            high = middle.saturating_sub(1);
        }
    }
    Ok(best)
}

fn retained_offset(part: &ProjectedPart) -> Result<usize, Error> {
    match part {
        ProjectedPart::Retained { next_offset, .. } => Ok(*next_offset),
        _ => Err(Error::ResultUnavailable),
    }
}

fn set_excerpt(part: &mut ProjectedPart, chunk: Chunk) -> Result<(), Error> {
    let ProjectedPart::Retained {
        excerpt,
        next_offset,
        truncated,
        ..
    } = part
    else {
        return Err(Error::ResultUnavailable);
    };
    *excerpt = chunk.text.ok_or(Error::ResultUnavailable)?;
    *next_offset = chunk.next_offset;
    *truncated = chunk.truncated;
    Ok(())
}

fn project(handle: &str, entry: &Entry, offset: usize, limit: usize) -> Result<Chunk, Error> {
    if limit == 0 || limit > MAX_VISIBLE_BYTES || offset > entry.bytes {
        return Err(Error::ResultUnavailable);
    }
    let mut chunk = Chunk {
        handle: handle.into(),
        source: entry.source.clone(),
        bytes: entry.bytes,
        offset,
        next_offset: offset,
        truncated: offset < entry.bytes,
        content_type: String::new(),
        text: None,
        status: None,
    };
    match &entry.payload {
        Payload::Text(text) => {
            if !text.is_char_boundary(offset) {
                return Err(Error::ResultUnavailable);
            }
            let mut end = offset.saturating_add(limit).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end == offset && offset < text.len() {
                return Err(Error::ResultUnavailable);
            }
            chunk.content_type = entry.content_type.clone();
            // Count the entire JSON envelope, including escaping and source.
            // A binary search avoids repeated large truncation copies.
            let mut low = offset;
            let mut high = end;
            while low < high {
                let mut middle = low + (high - low).div_ceil(2);
                while !text.is_char_boundary(middle) {
                    middle -= 1;
                }
                if middle == low {
                    middle = low + text[low..].chars().next().map_or(0, char::len_utf8);
                }
                chunk.text = Some(text[offset..middle].into());
                chunk.next_offset = middle;
                chunk.truncated = middle < text.len();
                let bytes = serde_json::to_vec(&chunk)
                    .map_err(|_| Error::ResultUnavailable)?
                    .len();
                if bytes <= MAX_VISIBLE_BYTES {
                    low = middle;
                } else {
                    high = middle - 1;
                    while !text.is_char_boundary(high) {
                        high -= 1;
                    }
                }
            }
            if low == offset && offset < text.len() {
                return Err(Error::ContextLimit);
            }
            chunk.text = Some(text[offset..low].into());
            chunk.next_offset = low;
            chunk.truncated = low < text.len();
        }
        Payload::Binary { mime, .. } => {
            chunk.content_type = mime.clone();
            chunk.status = Some("unsupported_modality");
            // No bytes were delivered to the model. The host can use the
            // separate binary API; do not pretend this advances its cursor.
        }
    }
    if serde_json::to_vec(&chunk)
        .map_err(|_| Error::ResultUnavailable)?
        .len()
        > MAX_VISIBLE_BYTES
    {
        return Err(Error::ContextLimit);
    }
    Ok(chunk)
}

#[cfg(test)]
mod tests;
