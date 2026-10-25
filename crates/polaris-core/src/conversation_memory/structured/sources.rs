//! Raw evidence resolution after exact structured-publication authorization.

use crate::conversation_state::{ConversationStateV2, ConversationStore, HistoryMode, RawEventV2};
use polaris_memory::{
    MemoryStore,
    structured::{
        PublicationScope, StructuredAncestorPublicationRange, StructuredPublication,
        StructuredPublicationView,
    },
};
use std::{io, path::Path};

/// Host-verified raw evidence. This is not an execution grant or a current-value
/// assertion; the retrieval packer still checks coverage and fact states.
#[derive(Debug, Clone)]
pub struct ResolvedStructuredSource {
    publication: StructuredPublication,
    source_id: String,
    event: RawEventV2,
}
impl ResolvedStructuredSource {
    /// The raw owner calls this inside `with_structured_source`, after binding
    /// the publication to its frozen receipt. No path or grant is accepted here.
    #[cfg(test)]
    pub(crate) fn from_raw_prefix(
        publication: StructuredPublication,
        source_id: &str,
        bytes: &[u8],
    ) -> io::Result<Self> {
        polaris_memory::structured::validate_publication(&publication).map_err(io::Error::other)?;
        let fail = || io::Error::other("Structured source text or range mismatch");
        if !bytes.ends_with(b"\n")
            || crate::conversation_state::content_hash(bytes) != publication.raw_hash
        {
            return Err(fail());
        }
        let mut previous = None;
        let mut found = None;
        for line in bytes[..bytes.len() - 1].split(|b| *b == b'\n') {
            let event: RawEventV2 = serde_json::from_slice(line)?;
            crate::conversation_state::validate_event(&event, previous.as_ref())?;
            if serde_json::to_vec(&event)? != line
                || i64::try_from(event.epoch)
                    .ok()
                    .is_none_or(|e| e > publication.scope.epoch)
            {
                return Err(fail());
            }
            let id = format!(
                "source-v3:{}:{}:{}:{}",
                publication.scope.project_id,
                publication.scope.session_id,
                event.epoch,
                event.sequence
            );
            if id == source_id
                && (i64::try_from(event.epoch).ok() != Some(publication.scope.epoch)
                    || found.replace(event.clone()).is_some())
            {
                return Err(fail());
            }
            previous = Some(event);
        }
        Self::from_verified_event(publication, source_id, found.ok_or_else(fail)?)
    }

    /// The raw owner has checked the entire frozen prefix and the index has
    /// confirmed this source's membership in the exact saved publication.
    /// This crate-internal constructor only binds the resulting event identity.
    pub(crate) fn from_verified_event(
        publication: StructuredPublication,
        source_id: &str,
        event: RawEventV2,
    ) -> io::Result<Self> {
        polaris_memory::structured::validate_publication(&publication).map_err(io::Error::other)?;
        let expected = format!(
            "source-v3:{}:{}:{}:{}",
            publication.scope.project_id, publication.scope.session_id, event.epoch, event.sequence
        );
        if event.schema_version != 2
            || event.sequence == 0
            || event.turn_id == 0
            || i64::try_from(event.epoch).ok() != Some(publication.scope.epoch)
            || source_id != expected
        {
            return Err(io::Error::other(
                "Structured source text identifier mismatch",
            ));
        }
        Ok(Self {
            publication,
            source_id: source_id.into(),
            event,
        })
    }

    pub fn publication(&self) -> &StructuredPublication {
        &self.publication
    }
    pub fn source_id(&self) -> &str {
        &self.source_id
    }
    pub fn event(&self) -> &RawEventV2 {
        &self.event
    }
}

pub fn publication_view(state: &ConversationStateV2) -> io::Result<StructuredPublicationView> {
    state.validate()?;
    owner_publication_view(state)
}

/// Shared projection for already validated owners, including opaque v3 IDs.
pub(crate) fn owner_publication_view(
    state: &ConversationStateV2,
) -> io::Result<StructuredPublicationView> {
    if state.history_mode != HistoryMode::Structured10 {
        return Err(io::Error::other("Structured memory is disabled"));
    }
    let number = |n: u64| {
        i64::try_from(n)
            .map_err(|_| io::Error::other("Structured memory storage generation limit reached"))
    };
    Ok(StructuredPublicationView {
        scope: PublicationScope {
            project_id: state.project_id.clone(),
            session_id: state.session_id.clone(),
            epoch: number(state.epoch)?,
            generation: number(state.generation)?,
        },
        visible_publication_ids: state.visible_structured_publication_ids.clone(),
        ancestors: state
            .ancestors
            .iter()
            .map(|a| {
                Ok(StructuredAncestorPublicationRange {
                    session_id: a.session_id.clone(),
                    epoch: number(a.epoch)?,
                    generation: number(a.generation)?,
                    visible_publication_ids: a.visible_structured_publication_ids.clone(),
                })
            })
            .collect::<io::Result<_>>()?,
    })
}

/// Resolve through the current owner's frozen marker grants, never through a
/// model-supplied path or scope. Desktop's separate raw owner uses the same
/// index callback but must map its own published-prefix receipt.
pub fn resolve_source(
    store: &ConversationStore,
    database: &Path,
    publication_id: &str,
    source_id: &str,
) -> io::Result<ResolvedStructuredSource> {
    let lock = store.lock()?;
    let current = lock.snapshot()?;
    let memory = MemoryStore::open(database).map_err(io::Error::other)?;
    crate::conversation_memory::structured_owner::with_verified_receipts(
        store,
        &current,
        &memory,
        |_| resolve_source_from_snapshot(store, &current, &memory, publication_id, source_id),
    )
}

/// Reuses the caller's frozen snapshot and index connection. Shared-history
/// preparation additionally verifies receipts and keeps the raw owner locked.
pub(crate) fn resolve_source_from_snapshot(
    store: &ConversationStore,
    current: &crate::conversation_state::ConversationSnapshot,
    memory: &MemoryStore,
    publication_id: &str,
    source_id: &str,
) -> io::Result<ResolvedStructuredSource> {
    let view = publication_view(&current.state)?;
    memory
        .with_structured_source(&view, publication_id, source_id, |publication| {
            // Reopening the current owner would try to acquire its existing lock.
            let parent;
            let source_store = if publication.scope.session_id == current.state.session_id {
                store
            } else {
                parent = ConversationStore::open(
                    store.data_root().map_err(polaris_memory::Error::Io)?,
                    &publication.scope.project_id,
                    &publication.scope.session_id,
                )
                .map_err(polaris_memory::Error::Io)?;
                &parent
            };
            let generation = u64::try_from(publication.scope.generation)
                .ok()
                .and_then(|g| g.checked_sub(1))
                .ok_or(polaris_memory::Error::InvalidInput(
                    "structured source generation invalid",
                ))?;
            // load_snapshot reconstructs raw JSONL bytes and checks their hash,
            // length, event sequence, state identity and stored coverage boundary.
            let snapshot = source_store
                .load_snapshot(generation, &publication.raw_hash)
                .map_err(polaris_memory::Error::Io)?;
            if i64::try_from(snapshot.state.epoch).ok() != Some(publication.scope.epoch) {
                return Err(polaris_memory::Error::InvalidInput(
                    "structured source epoch mismatch",
                ));
            }
            let mut matches = snapshot.events.iter().filter(|event| {
                event.epoch == snapshot.state.epoch
                    && super::source_id(&snapshot, event) == source_id
            });
            let event = matches
                .next()
                .cloned()
                .ok_or(polaris_memory::Error::InvalidInput(
                    "structured source absent from raw snapshot",
                ))?;
            if matches.next().is_some() {
                return Err(polaris_memory::Error::InvalidInput(
                    "structured source sequence ambiguous",
                ));
            }
            Ok(ResolvedStructuredSource {
                publication,
                source_id: source_id.into(),
                event,
            })
        })
        .map_err(io::Error::other)
}

const URI_PREFIX: &str = "conversation://structured/";

pub fn source_uri(publication_id: &str, source_id: &str) -> io::Result<String> {
    valid_component(publication_id)?;
    valid_component(source_id)?;
    Ok(uri_at(publication_id, source_id, 0))
}

fn valid_component(value: &str) -> io::Result<()> {
    if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        return Err(io::Error::other("Invalid structured source URI identifier"));
    }
    Ok(())
}
fn encode_component(value: &str) -> String {
    use std::fmt::Write;
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            write!(&mut encoded, "%{byte:02X}").expect("String formatting");
        }
    }
    encoded
}
fn decode_component(value: &str) -> io::Result<String> {
    let invalid = || io::Error::other("Invalid structured source URI");
    let bytes = value.as_bytes();
    let mut decoded = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hi = char::from(*bytes.get(at + 1).ok_or_else(invalid)?)
                .to_digit(16)
                .ok_or_else(invalid)?;
            let lo = char::from(*bytes.get(at + 2).ok_or_else(invalid)?)
                .to_digit(16)
                .ok_or_else(invalid)?;
            decoded.push((hi * 16 + lo) as u8);
            at += 3;
        } else {
            decoded.push(bytes[at]);
            at += 1;
        }
    }
    let decoded = String::from_utf8(decoded).map_err(|_| invalid())?;
    valid_component(&decoded)?;
    if encode_component(&decoded) != value {
        return Err(invalid());
    }
    Ok(decoded)
}
fn uri_at(publication: &str, source: &str, offset: usize) -> String {
    format!(
        "{URI_PREFIX}{}?source={}&offset={offset}",
        encode_component(publication),
        encode_component(source)
    )
}

pub(crate) fn parse_source_uri(uri: &str) -> io::Result<(String, String, usize)> {
    let invalid = || io::Error::other("Invalid structured source URI");
    if uri.len() > 8192 {
        return Err(invalid());
    }
    let (publication, rest) = uri
        .strip_prefix(URI_PREFIX)
        .and_then(|s| s.split_once("?source="))
        .ok_or_else(invalid)?;
    let (source, offset) = rest.split_once("&offset=").ok_or_else(invalid)?;
    let publication = decode_component(publication)?;
    let source = decode_component(source)?;
    let offset = offset.parse::<usize>().map_err(|_| invalid())?;
    if uri_at(&publication, &source, offset) != uri {
        return Err(invalid());
    }
    Ok((publication, source, offset))
}

pub(crate) fn resolve_uri(
    store: &ConversationStore,
    database: &Path,
    uri: &str,
) -> io::Result<crate::conversation_memory::ResolvedConversationSource> {
    let (publication, source, offset) = parse_source_uri(uri)?;
    let resolved = resolve_source(store, database, &publication, &source)?;
    Ok(crate::conversation_memory::ResolvedConversationSource {
        text: resolved.page(offset)?,
        source_id: source,
        start_turn: resolved.event.turn_id,
        end_turn: resolved.event.turn_id,
    })
}
impl ResolvedStructuredSource {
    pub(crate) fn page(&self, offset: usize) -> io::Result<String> {
        valid_component(&self.publication.publication_id)?;
        valid_component(&self.source_id)?;
        let payload = serde_json::to_string(&self.event)?;
        let source_label = serde_json::to_string(&self.source_id)?;
        super::super::strict_history::evidence_page(&payload, offset, |stop| {
            let next = if stop < payload.len() {
                uri_at(&self.publication.publication_id, &self.source_id, stop)
            } else {
                "none".into()
            };
            format!(
                "historical evidence source_id={source_label} turn={} raw_hash={} bytes={offset}-{stop}/{} next={next}",
                self.event.turn_id,
                self.publication.raw_hash,
                payload.len()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These fixtures seed only the index and archived raw source. Public readers
    // require owner receipts; this helper isolates index/raw validation below it.
    fn resolve_index_source(
        store: &ConversationStore,
        database: &Path,
        publication: &str,
        source: &str,
    ) -> io::Result<ResolvedStructuredSource> {
        let lock = store.lock()?;
        let snapshot = lock.snapshot()?;
        let memory = MemoryStore::open(database).map_err(io::Error::other)?;
        resolve_source_from_snapshot(store, &snapshot, &memory, publication, source)
    }

    use polaris_memory::structured::*;
    use polaris_provider::Message;

    #[test]
    fn owned_raw_prefix_requires_canonical_hash_sequence_and_exact_source() {
        let publication = StructuredPublication {
            scope: PublicationScope {
                project_id: "p".into(),
                session_id: "opaque-session".into(),
                epoch: 0,
                generation: 1,
            },
            publication_id: "fixture".into(),
            raw_hash: "a".repeat(64),
            delta_hash: "b".repeat(64),
            prompt_version: "fixture".into(),
        };
        let event = RawEventV2 {
            schema_version: 2,
            sequence: 1,
            epoch: 0,
            turn_id: 1,
            starts_turn: true,
            message: Message::user("原文🌟"),
        };
        let encode = |event: &RawEventV2| {
            let mut bytes = serde_json::to_vec(event).unwrap();
            bytes.push(b'\n');
            bytes
        };
        let raw = encode(&event);
        let source_id = "source-v3:p:opaque-session:0:1";
        let resolve = |raw: &[u8], id: &str| {
            let mut publication = publication.clone();
            publication.raw_hash = crate::conversation_state::content_hash(raw);
            ResolvedStructuredSource::from_raw_prefix(publication, id, raw)
        };
        assert_eq!(encode(resolve(&raw, source_id).unwrap().event()), raw);
        assert!(
            ResolvedStructuredSource::from_raw_prefix(publication.clone(), source_id, &raw)
                .is_err()
        );
        for id in [
            "source-v3:p:other-session:0:1",
            "source-v3:p:opaque-session:0:2",
            "source-v3:p:opaque-session:0:01",
        ] {
            assert!(resolve(&raw, id).is_err());
        }
        let mut future = event.clone();
        future.epoch = 1;
        let mut out_of_order = event.clone();
        out_of_order.sequence = 2;
        let mut pretty = serde_json::to_vec_pretty(&event).unwrap();
        pretty.push(b'\n');
        for invalid in [
            Vec::new(),
            raw[..raw.len() - 1].to_vec(),
            [raw.as_slice(), b"\n"].concat(),
            [raw.as_slice(), raw.as_slice()].concat(),
            encode(&future),
            encode(&out_of_order),
            pretty,
        ] {
            assert!(resolve(&invalid, source_id).is_err());
        }
    }

    #[test]
    fn source_uri_escapes_values_and_rejects_ambiguous_or_pathlike_forms() {
        let publication = "公開/a?b&c";
        let source = "source-v3:project:s:0:1";
        let uri = source_uri(publication, source).unwrap();
        assert_eq!(
            parse_source_uri(&uri).unwrap(),
            (publication.into(), source.into(), 0)
        );
        for invalid in [
            "conversation:///tmp/raw.jsonl",
            "conversation://structured/p?source=s&offset=0&offset=1",
            "conversation://structured/p?source=s&offset=01",
            "conversation://structured/p?source=%FF&offset=0",
            "conversation://structured/p?source=%0A&offset=0",
            "conversation://structured/p?source=%73&offset=0",
            "conversation://structured/p?source=s&offset=-1",
            "conversation://structured/p?source=%&offset=0",
        ] {
            assert!(parse_source_uri(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn structured_raw_read_verifies_scope_source_and_snapshot_bytes() {
        let root = tempfile::tempdir().unwrap();
        let session = "00000000-0000-4000-8000-000000000091";
        let mut state = ConversationStateV2::new("p".into(), session.into()).unwrap();
        state.history_mode = HistoryMode::Structured10;
        let store = ConversationStore::create(root.path(), state).unwrap();
        let database = root.path().join("memory.sqlite3");
        let mut db = MemoryStore::open(&database).unwrap();
        db.enable_structured_memory().unwrap();
        let mut lock = store.lock().unwrap();
        let snapshot = lock
            .append(Message::user("Asterの本番再試行上限は3回。"), true)
            .unwrap();
        let source = super::super::source_id(&snapshot, &snapshot.events[0]);
        let origin = OriginScope {
            project_id: "p".into(),
            session_id: session.into(),
            epoch: 0,
        };
        let entity_id = stable_entity_id(&origin, &source, 0, EntityKind::Product).unwrap();
        let mut delta = StructuredDelta {
            publication: StructuredPublication {
                scope: PublicationScope {
                    project_id: "p".into(),
                    session_id: session.into(),
                    epoch: 0,
                    generation: 1,
                },
                publication_id: "structured10-1-fixture".into(),
                raw_hash: snapshot.raw_hash.clone(),
                delta_hash: "a".repeat(64),
                prompt_version: "fixture".into(),
            },
            entities: vec![StructuredEntity {
                entity_id: entity_id.clone(),
                origin,
                origin_source_id: source.clone(),
                origin_ordinal: 0,
                kind: EntityKind::Product,
            }],
            entity_revisions: vec![StructuredEntityRevision {
                entity_id,
                source_id: source.clone(),
                canonical_name: "Aster".into(),
                description: "Product".into(),
                subject_project: "p".into(),
            }],
            aliases: vec![],
            facts: vec![],
            dependencies: vec![],
            relations: vec![],
        };
        delta.publication.delta_hash = delta_hash(&delta).unwrap();
        db.insert_structured_delta(&delta, &publication_view(&snapshot.state).unwrap())
            .unwrap();
        let path = lock.save_snapshot(&snapshot).unwrap();
        let mut next = snapshot.state.clone();
        next.visible_structured_publication_ids
            .push(delta.publication.publication_id.clone());
        // Synthetic fixture publishes no concurrent writer/forget operation.
        lock.publish(&snapshot, next).unwrap();
        drop(lock);
        let source_read = || {
            resolve_index_source(
                &store,
                &database,
                &delta.publication.publication_id,
                &source,
            )
        };
        let resolved = source_read().unwrap();
        assert_eq!(
            resolved.event().message.content,
            "Asterの本番再試行上限は3回。"
        );
        assert_eq!(resolved.source_id(), source);
        assert_eq!(resolved.publication(), &delta.publication);
        let uri = source_uri(&delta.publication.publication_id, &source).unwrap();
        assert!(
            crate::conversation_memory::resolve_conversation_uri(&store, &database, &uri).is_err()
        );
        assert!(
            super::resolve_source(
                &store,
                &database,
                &delta.publication.publication_id,
                &source
            )
            .is_err()
        );
        let mut large = resolved.clone();
        large.event.message.content = "過去の原文🌟".repeat(1500);
        let expected = serde_json::to_string(large.event()).unwrap();
        let mut recovered = String::new();
        let mut offset = 0;
        loop {
            let page = large.page(offset).unwrap();
            assert!(page.len() <= 4096 && crate::budget::count_tokens(&page) <= 1024);
            let (header, body) = page.split_once('\n').unwrap();
            assert!(!body.is_empty());
            recovered.push_str(body);
            let next = header.rsplit_once(" next=").unwrap().1;
            if next == "none" {
                break;
            }
            let (_, _, next_offset) = parse_source_uri(next).unwrap();
            assert!(next_offset > offset);
            offset = next_offset;
        }
        assert_eq!(recovered, expected);
        assert!(resolve_index_source(&store, &database, "ungranted", &source).is_err());
        let child = store
            .lock()
            .unwrap()
            .fork(
                root.path(),
                "00000000-0000-4000-8000-000000000092",
                "fixture",
            )
            .unwrap();
        assert_eq!(
            resolve_index_source(
                &child,
                &database,
                &delta.publication.publication_id,
                &source
            )
            .unwrap()
            .event()
            .message
            .content,
            resolved.event().message.content
        );
        let mut parent_lock = store.lock().unwrap();
        let later_snapshot = parent_lock
            .append(Message::user("AsterをNovaへ改名。"), true)
            .unwrap();
        let later_source =
            super::super::source_id(&later_snapshot, later_snapshot.events.last().unwrap());
        let mut later = delta.clone();
        later.publication.scope.generation = 2;
        later.publication.publication_id = "structured10-2-later".into();
        later.publication.raw_hash = later_snapshot.raw_hash.clone();
        later.entities.clear();
        later.entity_revisions[0].source_id = later_source.clone();
        later.entity_revisions[0].canonical_name = "Nova".into();
        later.publication.delta_hash = delta_hash(&later).unwrap();
        db.insert_structured_delta(&later, &publication_view(&later_snapshot.state).unwrap())
            .unwrap();
        parent_lock.save_snapshot(&later_snapshot).unwrap();
        let mut next = later_snapshot.state.clone();
        next.visible_structured_publication_ids
            .push(later.publication.publication_id.clone());
        parent_lock.publish(&later_snapshot, next).unwrap();
        drop(parent_lock);
        assert_eq!(
            resolve_index_source(
                &store,
                &database,
                &later.publication.publication_id,
                &later_source
            )
            .unwrap()
            .event()
            .message
            .content,
            "AsterをNovaへ改名。"
        );
        assert!(
            resolve_index_source(
                &child,
                &database,
                &later.publication.publication_id,
                &later_source
            )
            .is_err()
        );
        assert!(
            resolve_index_source(
                &child,
                &database,
                &delta.publication.publication_id,
                &source
            )
            .is_ok()
        );
        let original = std::fs::read(&path).unwrap();
        let mut changed: serde_json::Value = serde_json::from_slice(&original).unwrap();
        changed["events"][0]["message"]["content"] = serde_json::json!("上限は99回。");
        std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(source_read().is_err());
        std::fs::write(&path, &original).unwrap();
        assert!(source_read().is_ok());
        db.delete_session("p", session).unwrap();
        assert!(source_read().is_err());
        assert!(
            resolve_index_source(
                &child,
                &database,
                &delta.publication.publication_id,
                &source
            )
            .is_err()
        );
    }
}
