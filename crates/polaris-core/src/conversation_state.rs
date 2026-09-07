//! Versioned session storage, separate from the legacy Message-only JSONL reader.
//!
//! A lock is held only for local I/O. Summary and embedding work use the returned
//! immutable snapshot and must reacquire/compare its generation before publishing.
//! SQLite tombstones are owned by the memory layer: its transaction must enclose
//! `publish` when publishing indexed summaries. This module does not authorize reads.

use crate::workflow::{WORKFLOW_SCHEMA_VERSION, WorkflowGatesV1, WorkflowStateV1};
use polaris_provider::{Message, Role};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

pub const SCHEMA_VERSION: u32 = 2;
pub const REAL_TURN_WINDOW: usize = 10;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryMode {
    #[default]
    Legacy,
    Strict10,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AncestorRange {
    pub session_id: String,
    pub epoch: u64,
    pub generation: u64,
    pub visible_summary_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationStateV2 {
    pub schema_version: u32,
    pub project_id: String,
    pub session_id: String,
    pub epoch: u64,
    pub generation: u64,
    pub raw_commit_offset: u64,
    pub raw_commit_hash: String,
    #[serde(default)]
    pub history_mode: HistoryMode,
    pub recent_turn_ids: Vec<u64>,
    pub visible_summary_ids: Vec<String>,
    pub workflow: WorkflowStateV1,
    pub gates: WorkflowGatesV1,
    pub ancestors: Vec<AncestorRange>,
}

impl ConversationStateV2 {
    pub fn new(project_id: String, session_id: String) -> io::Result<Self> {
        let state = Self {
            schema_version: SCHEMA_VERSION,
            project_id,
            session_id,
            epoch: 0,
            generation: 0,
            raw_commit_offset: 0,
            raw_commit_hash: content_hash(&[]),
            history_mode: HistoryMode::Legacy,
            recent_turn_ids: Vec::new(),
            visible_summary_ids: Vec::new(),
            workflow: WorkflowStateV1::default(),
            gates: WorkflowGatesV1::default(),
            ancestors: Vec::new(),
        };
        state.validate()?;
        Ok(state)
    }
    pub fn validate(&self) -> io::Result<()> {
        if self.schema_version != SCHEMA_VERSION
            || self.workflow.schema_version != WORKFLOW_SCHEMA_VERSION
            || self.gates.schema_version != WORKFLOW_SCHEMA_VERSION
        {
            return Err(invalid("未対応のセッション保存形式です"));
        }
        if self.project_id.is_empty()
            || !valid_session_id(&self.session_id)
            || !valid_hash(&self.raw_commit_hash)
            || self.recent_turn_ids.len() > REAL_TURN_WINDOW
            || self.recent_turn_ids.windows(2).any(|ids| ids[0] >= ids[1])
            || self.visible_summary_ids.iter().any(|id| id.is_empty())
            || self
                .visible_summary_ids
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                != self.visible_summary_ids.len()
            || self.ancestors.iter().any(|ancestor| {
                !valid_session_id(&ancestor.session_id) || ancestor.session_id == self.session_id
            })
        {
            return Err(invalid("セッション状態の識別子・範囲が不正です"));
        }
        Ok(())
    }
}

/// `starts_turn` is set only for actual user input, not few-shot or retrieved data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawEventV2 {
    pub schema_version: u32,
    pub sequence: u64,
    pub epoch: u64,
    pub turn_id: u64,
    pub starts_turn: bool,
    pub message: Message,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationSnapshot {
    pub state: ConversationStateV2,
    pub raw_hash: String,
    pub raw_offset: u64,
    pub events: Vec<RawEventV2>,
}
impl ConversationSnapshot {
    pub fn active_turns(&self) -> Vec<u64> {
        self.events
            .iter()
            .filter(|event| event.epoch == self.state.epoch && event.starts_turn)
            .map(|event| event.turn_id)
            .collect()
    }
    pub fn expired_turns(&self) -> Vec<u64> {
        let mut turns = self.active_turns();
        turns.truncate(turns.len().saturating_sub(REAL_TURN_WINDOW));
        turns
    }
    pub fn recent_messages(&self) -> Vec<Message> {
        let turns = self.active_turns();
        let first = turns.get(turns.len().saturating_sub(REAL_TURN_WINDOW));
        self.events
            .iter()
            .filter(|event| {
                event.epoch == self.state.epoch
                    && first.is_some_and(|first| event.turn_id >= *first)
            })
            .map(|event| event.message.clone())
            .collect()
    }
    /// Expired turns must not split a function call/result group.
    pub fn validate_complete_turn(&self, turn_id: u64) -> io::Result<()> {
        let events: Vec<_> = self
            .events
            .iter()
            .filter(|event| event.epoch == self.state.epoch && event.turn_id == turn_id)
            .collect();
        if !events.first().is_some_and(|event| event.starts_turn) {
            return Err(invalid("原文ターンがありません"));
        }
        let mut pending = BTreeSet::new();
        let mut seen = BTreeSet::new();
        for event in events {
            let msg = &event.message;
            if !pending.is_empty() && msg.role != Role::Tool {
                return Err(invalid("tool結果が未完了です"));
            }
            for call in &msg.tool_calls {
                if msg.role != Role::Assistant
                    || call.id.is_empty()
                    || !seen.insert(call.id.clone())
                {
                    return Err(invalid("tool呼出しIDが不正です"));
                }
                pending.insert(call.id.clone());
            }
            if msg.role == Role::Tool {
                if !msg
                    .tool_call_id
                    .as_ref()
                    .is_some_and(|id| pending.remove(id))
                {
                    return Err(invalid("対応しないtool結果です"));
                }
            } else if msg.tool_call_id.is_some() {
                return Err(invalid("tool以外に結果IDがあります"));
            }
        }
        if !pending.is_empty() {
            return Err(invalid("tool結果が未完了です"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ConversationStore {
    directory: PathBuf,
    project_id: String,
    session_id: String,
}

impl ConversationStore {
    pub fn create(data_root: &Path, state: ConversationStateV2) -> io::Result<Self> {
        state.validate()?;
        if state.generation != 0
            || state.raw_commit_offset != 0
            || state.raw_commit_hash != content_hash(&[])
            || !state.recent_turn_ids.is_empty()
            || !state.visible_summary_ids.is_empty()
        {
            return Err(invalid("新規セッションの状態が不正です"));
        }
        let root = data_root.join("sessions-v2");
        fs::create_dir_all(&root)?;
        reject_symlink(&root)?;
        let directory = root.join(&state.session_id);
        // Never overwrite an existing session, even after an interrupted creation.
        private_directory(&directory)?;
        private_directory(&directory.join("snapshots"))?;
        let raw = private_file(&directory.join("raw.jsonl"), true)?;
        raw.sync_all()?;
        let lock = private_file(&directory.join("writer.lock"), true)?;
        lock.sync_all()?;
        atomic_write(&directory.join("state.json"), &serde_json::to_vec(&state)?)?;
        File::open(&root)?.sync_all()?;
        Ok(Self {
            directory,
            project_id: state.project_id,
            session_id: state.session_id,
        })
    }
    pub fn open(data_root: &Path, project_id: &str, session_id: &str) -> io::Result<Self> {
        if !valid_session_id(session_id) {
            return Err(invalid("セッションIDが不正です"));
        }
        let directory = data_root.join("sessions-v2").join(session_id);
        reject_symlink(&data_root.join("sessions-v2"))?;
        reject_symlink(&directory)?;
        let store = Self {
            directory,
            project_id: project_id.into(),
            session_id: session_id.into(),
        };
        store.lock()?.snapshot()?;
        Ok(store)
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    /// Reads the immutable raw snapshot named by an indexed conversation source.
    /// The caller supplies only a generation and hash already verified by the
    /// memory index; this method never accepts an arbitrary filesystem path.
    pub fn load_snapshot(
        &self,
        generation: u64,
        raw_hash: &str,
    ) -> io::Result<ConversationSnapshot> {
        if !valid_hash(raw_hash) {
            return Err(invalid("原文snapshot hashが不正です"));
        }
        let path = self
            .directory
            .join("snapshots")
            .join(format!("{generation}-{raw_hash}.json"));
        reject_symlink(&path)?;
        let snapshot: ConversationSnapshot = serde_json::from_slice(&fs::read(path)?)?;
        if snapshot.state.project_id != self.project_id
            || snapshot.state.session_id != self.session_id
            || snapshot.state.generation != generation
            || snapshot.raw_hash != raw_hash
            || snapshot.raw_offset > u64::try_from(usize::MAX).unwrap_or(u64::MAX)
        {
            return Err(invalid("原文snapshotの所属・世代・hashが一致しません"));
        }
        let mut raw = Vec::new();
        for event in &snapshot.events {
            serde_json::to_writer(&mut raw, event)?;
            raw.push(b'\n');
        }
        if raw.len() as u64 != snapshot.raw_offset || content_hash(&raw) != snapshot.raw_hash {
            return Err(invalid("原文snapshot本文のhash・長さが一致しません"));
        }
        snapshot.state.validate()?;
        for (index, event) in snapshot.events.iter().enumerate() {
            validate_event(
                event,
                index.checked_sub(1).and_then(|i| snapshot.events.get(i)),
            )?;
            if event.epoch > snapshot.state.epoch {
                return Err(invalid("原文snapshotのepochが不正です"));
            }
        }
        Ok(snapshot)
    }
    /// The v2 root inferred from this session's fixed directory layout.
    pub fn data_root(&self) -> io::Result<&Path> {
        self.directory
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| invalid("セッション保存先が不正です"))
    }
    pub fn lock(&self) -> io::Result<LockedConversation<'_>> {
        reject_symlink(&self.directory.join("writer.lock"))?;
        let file = private_file(&self.directory.join("writer.lock"), false)?;
        file.try_lock().map_err(io::Error::other)?;
        Ok(LockedConversation {
            store: self,
            _lock: file,
        })
    }
}

/// Dropping the guard explicitly releases the OS lock, including cancellation.
/// Do not retain this across provider calls.
pub struct LockedConversation<'a> {
    store: &'a ConversationStore,
    _lock: File,
}
impl Drop for LockedConversation<'_> {
    fn drop(&mut self) {
        // Closing this descriptor alone is insufficient while a concurrently
        // forked process retains the same open-file description before exec.
        // An explicit unlock ends our critical section without waiting for it.
        let _ = self._lock.unlock();
    }
}
impl LockedConversation<'_> {
    /// Copies only the active real-turn window into a fresh namespace. Ancestor
    /// reads remain bounded at the parent's committed generation; the memory
    /// layer must still check its latest tombstone on every such lookup.
    pub fn fork(
        &self,
        data_root: &Path,
        new_id: &str,
        artifact_hash: &str,
    ) -> io::Result<ConversationStore> {
        let snapshot = self.snapshot()?;
        let turns = snapshot.active_turns();
        let first = turns.get(turns.len().saturating_sub(REAL_TURN_WINDOW));
        for turn in turns.iter().rev().take(REAL_TURN_WINDOW) {
            snapshot.validate_complete_turn(*turn)?;
        }
        let mut next = ConversationStateV2::new(snapshot.state.project_id.clone(), new_id.into())?;
        next.history_mode = snapshot.state.history_mode;
        next.workflow = snapshot.state.workflow.clone();
        next.gates = snapshot.state.gates.for_fork(artifact_hash);
        next.ancestors = snapshot.state.ancestors.clone();
        next.ancestors.push(AncestorRange {
            session_id: snapshot.state.session_id.clone(),
            epoch: snapshot.state.epoch,
            generation: snapshot.state.generation,
            visible_summary_ids: snapshot.state.visible_summary_ids.clone(),
        });
        let child = ConversationStore::create(data_root, next)?;
        let mut lock = child.lock()?;
        for event in snapshot.events.iter().filter(|event| {
            event.epoch == snapshot.state.epoch
                && first.is_some_and(|first| event.turn_id >= *first)
        }) {
            lock.append(event.message.clone(), event.starts_turn)?;
        }
        let copied = lock.snapshot()?;
        lock.publish(&copied, copied.state.clone())?;
        drop(lock);
        Ok(child)
    }

    pub fn snapshot(&self) -> io::Result<ConversationSnapshot> {
        let state_path = self.store.directory.join("state.json");
        reject_symlink(&state_path)?;
        let state: ConversationStateV2 = serde_json::from_slice(&fs::read(state_path)?)?;
        state.validate()?;
        if state.project_id != self.store.project_id || state.session_id != self.store.session_id {
            return Err(invalid("セッションの所属が一致しません"));
        }
        let raw_path = self.store.directory.join("raw.jsonl");
        reject_symlink(&raw_path)?;
        let bytes = fs::read(raw_path)?;
        let committed =
            usize::try_from(state.raw_commit_offset).map_err(|_| invalid("原文範囲が不正です"))?;
        if committed > bytes.len()
            || content_hash(&bytes[..committed]) != state.raw_commit_hash
            || (committed > 0 && bytes[committed - 1] != b'\n')
        {
            return Err(invalid("保存済み原文のhashが一致しません"));
        }
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            return Err(invalid(
                "原文の末尾が未完了です。原文を保持して停止しました",
            ));
        }
        let mut events = Vec::<RawEventV2>::new();
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let event: RawEventV2 = serde_json::from_slice(line)?;
            validate_event(&event, events.last())?;
            if event.epoch > state.epoch {
                return Err(invalid("未公開epochの原文があります"));
            }
            events.push(event);
        }
        Ok(ConversationSnapshot {
            state,
            raw_hash: content_hash(&bytes),
            raw_offset: bytes.len() as u64,
            events,
        })
    }
    pub fn append(
        &mut self,
        message: Message,
        starts_turn: bool,
    ) -> io::Result<ConversationSnapshot> {
        let snapshot = self.snapshot()?;
        let prior = snapshot.events.last();
        let sequence = prior.map_or(Ok(1), |event| {
            event
                .sequence
                .checked_add(1)
                .ok_or_else(|| invalid("イベント番号が上限です"))
        })?;
        let turn_id = if starts_turn {
            prior.map_or(Ok(1), |event| {
                event
                    .turn_id
                    .checked_add(1)
                    .ok_or_else(|| invalid("ターン番号が上限です"))
            })?
        } else {
            prior
                .filter(|event| event.epoch == snapshot.state.epoch)
                .ok_or_else(|| invalid("ユーザー入力より前の応答です"))?
                .turn_id
        };
        let event = RawEventV2 {
            schema_version: SCHEMA_VERSION,
            sequence,
            epoch: snapshot.state.epoch,
            turn_id,
            starts_turn,
            message,
        };
        validate_event(&event, prior)?;
        if starts_turn
            && let Some(previous) = prior.filter(|event| event.epoch == snapshot.state.epoch)
        {
            snapshot.validate_complete_turn(previous.turn_id)?;
        }
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        let path = self.store.directory.join("raw.jsonl");
        reject_symlink(&path)?;
        let mut file = OpenOptions::new().append(true).open(path)?;
        file.write_all(&line)?;
        fail_point("raw-sync")?;
        file.sync_all()?;
        self.snapshot()
    }
    pub fn save_snapshot(&self, expected: &ConversationSnapshot) -> io::Result<PathBuf> {
        self.check_snapshot(expected)?;
        fail_point("snapshot")?;
        let directory = self.store.directory.join("snapshots");
        reject_symlink(&directory)?;
        let path = directory.join(format!(
            "{}-{}.json",
            expected.state.generation, expected.raw_hash
        ));
        let body = serde_json::to_vec(expected)?;
        if path.exists() {
            reject_symlink(&path)?;
            if fs::read(&path)? != body {
                return Err(invalid("同じsnapshot IDの内容が異なります"));
            }
        } else {
            atomic_write(&path, &body)?;
        }
        Ok(path)
    }
    pub fn check_snapshot(&self, expected: &ConversationSnapshot) -> io::Result<()> {
        let actual = self.snapshot()?;
        if actual.state != expected.state
            || actual.raw_hash != expected.raw_hash
            || actual.raw_offset != expected.raw_offset
            || serde_json::to_vec(&actual.events)? != serde_json::to_vec(&expected.events)?
        {
            return Err(invalid("要約処理中にセッションが変更されました"));
        }
        Ok(())
    }
    /// The caller must hold the memory transaction which checks forgotten-session
    /// tombstones and verifies all visible summary IDs before invoking this method.
    /// All workflow and gate evidence is atomically published with the raw marker.
    pub fn publish(
        &mut self,
        expected: &ConversationSnapshot,
        mut next: ConversationStateV2,
    ) -> io::Result<()> {
        self.check_snapshot(expected)?;
        if next.project_id != expected.state.project_id
            || next.session_id != expected.state.session_id
            || next.epoch != expected.state.epoch
            || next.generation != expected.state.generation
        {
            return Err(invalid("公開対象の世代・所属が一致しません"));
        }
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("保存世代が上限です"))?;
        next.raw_commit_offset = expected.raw_offset;
        next.raw_commit_hash = expected.raw_hash.clone();
        let turns = expected.active_turns();
        next.recent_turn_ids = turns.into_iter().rev().take(REAL_TURN_WINDOW).collect();
        next.recent_turn_ids.reverse();
        next.validate()?;
        fail_point("marker")?;
        atomic_write(
            &self.store.directory.join("state.json"),
            &serde_json::to_vec(&next)?,
        )
    }
    /// Clear starts an empty epoch; raw history is retained for explicit recovery.
    pub fn clear_epoch(&mut self) -> io::Result<()> {
        let snapshot = self.snapshot()?;
        let mut next = snapshot.state.clone();
        next.epoch = next
            .epoch
            .checked_add(1)
            .ok_or_else(|| invalid("epochが上限です"))?;
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("保存世代が上限です"))?;
        next.raw_commit_hash = snapshot.raw_hash;
        next.raw_commit_offset = snapshot.raw_offset;
        next.recent_turn_ids.clear();
        next.visible_summary_ids.clear();
        next.ancestors.clear();
        atomic_write(
            &self.store.directory.join("state.json"),
            &serde_json::to_vec(&next)?,
        )
    }
}

fn validate_event(event: &RawEventV2, previous: Option<&RawEventV2>) -> io::Result<()> {
    if event.schema_version != SCHEMA_VERSION
        || event.sequence != previous.map_or(1, |event| event.sequence.saturating_add(1))
        || event.turn_id == 0
        || (event.starts_turn && event.message.role != Role::User)
        || previous.is_none() && !event.starts_turn
    {
        return Err(invalid("原文イベントの形式・順序が不正です"));
    }
    if let Some(previous) = previous
        && (event.epoch < previous.epoch
            || (event.epoch != previous.epoch && !event.starts_turn)
            || (event.starts_turn && event.turn_id != previous.turn_id.saturating_add(1))
            || (!event.starts_turn && event.turn_id != previous.turn_id))
    {
        return Err(invalid("原文のターン境界が不正です"));
    }
    Ok(())
}
fn valid_session_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, byte)| {
            if [8, 13, 18, 23].contains(&i) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}
pub fn content_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
#[cfg(test)]
thread_local! { static FAIL_POINT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }
fn fail_point(_point: &str) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_POINT.with(|point| point.get() == Some(_point)) {
        return Err(io::Error::other("injected persistence failure"));
    }
    Ok(())
}
fn reject_symlink(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(invalid("セッション内のsymlinkは使用できません"));
    }
    Ok(())
}
fn private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}
fn private_file(path: &Path, create: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().ok_or_else(|| invalid("保存先が不正です"))?;
    let temporary = parent.join(format!(
        ".pending-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = private_file(&temporary, true)?;
    // On failure leave the private temporary file for recovery, never remove raw.
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "00000000-0000-4000-8000-000000000001";
    fn store(root: &Path) -> ConversationStore {
        ConversationStore::create(
            root,
            ConversationStateV2::new("project".into(), ID.into()).unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn roundtrip_publishes_gates_and_detects_stale_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let old = {
            let mut lock = store.lock().unwrap();
            let snapshot = lock.append(Message::user("question"), true).unwrap();
            lock.save_snapshot(&snapshot).unwrap();
            let mut next = snapshot.state.clone();
            next.workflow.phase = crate::workflow::Phase::Implement;
            next.gates.approved_spec_hash = Some("spec".into());
            lock.publish(&snapshot, next).unwrap();
            snapshot
        };
        let resumed = ConversationStore::open(dir.path(), "project", ID).unwrap();
        let mut lock = resumed.lock().unwrap();
        let snapshot = lock.snapshot().unwrap();
        assert_eq!(snapshot.state.generation, 1);
        assert_eq!(
            snapshot.state.gates.approved_spec_hash.as_deref(),
            Some("spec")
        );
        assert_eq!(
            snapshot.state.workflow.phase,
            crate::workflow::Phase::Implement
        );
        assert!(lock.publish(&old, old.state.clone()).is_err());
        assert!(!dir.path().join("sessions").exists());
    }
    #[test]
    fn ten_real_turns_exclude_examples_and_retain_raw() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let mut lock = store.lock().unwrap();
        for turn in 1..=12 {
            lock.append(Message::user(format!("turn {turn}")), true)
                .unwrap();
            lock.append(Message::user("example"), false).unwrap();
            lock.append(Message::assistant("answer"), false).unwrap();
        }
        let snapshot = lock.snapshot().unwrap();
        assert_eq!(snapshot.expired_turns(), vec![1, 2]);
        assert_eq!(snapshot.recent_messages().len(), 30);
        assert_eq!(snapshot.recent_messages()[0].content, "turn 3");
        assert_eq!(snapshot.events.len(), 36);
        lock.clear_epoch().unwrap();
        let cleared = lock.snapshot().unwrap();
        assert!(cleared.recent_messages().is_empty());
        assert_eq!(cleared.events.len(), 36);
        let new = lock.append(Message::user("new epoch"), true).unwrap();
        assert_eq!(new.active_turns(), vec![13]);
    }
    #[test]
    fn raw_corruption_is_never_repaired_by_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let mut lock = store.lock().unwrap();
        let snapshot = lock.append(Message::user("original"), true).unwrap();
        lock.publish(&snapshot, snapshot.state.clone()).unwrap();
        let path = store.directory().join("raw.jsonl");
        let bytes = fs::read(&path).unwrap();
        let changed = String::from_utf8(bytes)
            .unwrap()
            .replace("original", "modified");
        fs::write(&path, &changed).unwrap();
        assert!(lock.snapshot().is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), changed);
    }
    #[test]
    fn unpublished_raw_is_recoverable_but_incomplete_tail_stops() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        {
            store
                .lock()
                .unwrap()
                .append(Message::user("recover me"), true)
                .unwrap();
        }
        let snapshot = store.lock().unwrap().snapshot().unwrap();
        assert_eq!(snapshot.state.raw_commit_offset, 0);
        assert_eq!(snapshot.events[0].message.content, "recover me");
        let path = store.directory().join("raw.jsonl");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{partial")
            .unwrap();
        let before = fs::read(&path).unwrap();
        assert!(store.lock().unwrap().snapshot().is_err());
        assert_eq!(fs::read(path).unwrap(), before);
    }
    #[test]
    fn pending_tool_calls_cannot_cross_real_user_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let mut lock = store.lock().unwrap();
        lock.append(Message::user("inspect"), true).unwrap();
        lock.append(
            Message::assistant_with_tool_calls(
                "",
                vec![polaris_provider::ToolCall {
                    id: "call".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                }],
            ),
            false,
        )
        .unwrap();
        assert!(lock.append(Message::user("next"), true).is_err());
        lock.append(Message::tool_result("call", "result"), false)
            .unwrap();
        lock.append(Message::user("next"), true).unwrap();
    }
    #[test]
    fn wrong_scope_version_and_path_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        assert!(ConversationStore::open(dir.path(), "other", ID).is_err());
        assert!(ConversationStore::open(dir.path(), "project", "../escape").is_err());
        assert!(
            ConversationStore::create(
                dir.path(),
                ConversationStateV2::new("project".into(), ID.into()).unwrap()
            )
            .is_err()
        );
        let path = store.directory().join("state.json");
        let mut state: ConversationStateV2 =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        state.gates.schema_version = 9;
        fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(store.lock().unwrap().snapshot().is_err());
    }
    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let lock = store.lock().unwrap();
        assert!(store.lock().is_err());
        drop(lock);
        assert!(store.lock().is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn drop_unlocks_even_if_an_inherited_description_remains_open() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let lock = store.lock().unwrap();
        // dup models the shared open-file description temporarily inherited
        // by a concurrently forked child before exec closes CLOEXEC handles.
        let inherited = lock._lock.try_clone().unwrap();
        assert!(store.lock().is_err());
        drop(lock);
        let next_writer = store.lock().expect("owner released the critical section");
        drop(inherited);
        assert!(
            store.lock().is_err(),
            "a retained handle cannot release the next writer"
        );
        drop(next_writer);
        assert!(store.lock().is_ok());
    }
    #[test]
    fn failed_sync_snapshot_and_marker_never_publish_or_erase_raw() {
        for point in ["raw-sync", "snapshot", "marker"] {
            let dir = tempfile::tempdir().unwrap();
            let store = store(dir.path());
            let mut lock = store.lock().unwrap();
            FAIL_POINT.with(|failure| failure.set(Some(point)));
            let result = lock
                .append(Message::user("retain original"), true)
                .and_then(|snapshot| {
                    lock.save_snapshot(&snapshot)?;
                    lock.publish(&snapshot, snapshot.state.clone())
                });
            FAIL_POINT.with(|failure| failure.set(None));
            assert!(result.is_err(), "{point} must stop publication");
            drop(lock);
            let snapshot = store.lock().unwrap().snapshot().unwrap();
            assert_eq!(snapshot.state.generation, 0);
            assert_eq!(snapshot.state.raw_commit_offset, 0);
            assert_eq!(snapshot.events[0].message.content, "retain original");
        }
    }
    #[test]
    fn a_forged_snapshot_body_cannot_be_saved_under_a_valid_raw_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let mut lock = store.lock().unwrap();
        let mut snapshot = lock.append(Message::user("original"), true).unwrap();
        snapshot.events[0].message.content = "replacement".into();
        assert!(lock.save_snapshot(&snapshot).is_err());
        assert!(lock.publish(&snapshot, snapshot.state.clone()).is_err());
    }
    #[test]
    fn fork_copies_only_ten_turns_and_freezes_parent_generation_without_grants() {
        use crate::workflow::{ExternalGrant, VerificationEvidence};
        let dir = tempfile::tempdir().unwrap();
        let parent = store(dir.path());
        let mut lock = parent.lock().unwrap();
        for turn in 1..=12 {
            lock.append(Message::user(format!("turn {turn}")), true)
                .unwrap();
        }
        let snapshot = lock.snapshot().unwrap();
        let mut next = snapshot.state.clone();
        next.history_mode = HistoryMode::Strict10;
        next.gates.verification = Some(VerificationEvidence {
            artifact_hash: "old".into(),
            passed: true,
            stale: false,
        });
        next.gates.external_grants.push(ExternalGrant {
            operation: "push".into(),
            target: "main".into(),
            artifact_hash: "old".into(),
        });
        lock.publish(&snapshot, next).unwrap();
        let child = lock
            .fork(
                dir.path(),
                "00000000-0000-4000-8000-000000000002",
                "changed",
            )
            .unwrap();
        let fork = child.lock().unwrap().snapshot().unwrap();
        assert_eq!(fork.events.len(), 10);
        assert_eq!(fork.state.history_mode, HistoryMode::Strict10);
        assert_eq!(fork.events[0].message.content, "turn 3");
        assert!(fork.state.gates.external_grants.is_empty());
        assert!(fork.state.gates.verification.unwrap().stale);
        assert_eq!(fork.state.ancestors[0].generation, 1);
        lock.append(Message::user("parent future"), true).unwrap();
        assert_eq!(child.lock().unwrap().snapshot().unwrap().events.len(), 10);
        assert_eq!(lock.snapshot().unwrap().events.len(), 13);
    }
}
