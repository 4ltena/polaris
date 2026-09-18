//! Durable structured-summary receipts owned by the existing conversation lock.
//! Callers hold the memory tombstone guard for mutations; receipts grant no reads.

use super::*;
use crate::conversation_memory::structured::{
    STRUCTURED_PROMPT_VERSION, StructuredIntent, ValidatedStructuredResponse,
    structured_summary_intent, validate_saved_response,
};
use polaris_memory::structured::StructuredDelta;
use std::io::Read;

const RECEIPT_VERSION: u32 = 1;
const MAX_RECEIPT_BYTES: u64 = 1024 * 1024;
const MAX_ATTEMPTS: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedResponse {
    pub summary: String,
    pub delta: StructuredDelta,
    pub coverage: StructuredCoverage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StructuredAttemptReceipt {
    schema_version: u32,
    project_id: String,
    session_id: String,
    epoch: u64,
    generation: u64,
    raw_offset: u64,
    raw_hash: String,
    turn: u64,
    prompt_version: String,
    pub intent: StructuredIntent,
    pub response: Option<SavedResponse>,
}

impl StructuredAttemptReceipt {
    pub(crate) fn validate(&self, source: &ConversationSnapshot, turn: u64) -> io::Result<()> {
        if self.schema_version != RECEIPT_VERSION
            || self.project_id != source.state.project_id
            || self.session_id != source.state.session_id
            || self.epoch != source.state.epoch
            || self.generation != source.state.generation
            || self.raw_offset != source.raw_offset
            || self.raw_hash != source.raw_hash
            || self.turn != turn
            || self.prompt_version != STRUCTURED_PROMPT_VERSION
            || source.state.history_mode != HistoryMode::Structured10
            || !source.expired_turns().contains(&turn)
            || structured_summary_intent(source)?.as_ref() != Some(&self.intent)
        {
            return Err(invalid("構造化抽出記録の所属・世代・原文が一致しません"));
        }
        source.validate_complete_turn(turn)?;
        if let Some(response) = &self.response {
            validate_saved_response(
                source,
                turn,
                &self.intent,
                &response.summary,
                &response.delta,
                &response.coverage,
            )?;
        }
        Ok(())
    }
}

impl ConversationStore {
    pub(crate) fn bound_structured_attempt(
        &self,
        expected: &ConversationSnapshot,
        turn: u64,
    ) -> io::Result<Option<StructuredAttemptReceipt>> {
        let Some(receipt) = self.read_structured_attempt(expected.state.epoch, turn)? else {
            return Ok(None);
        };
        receipt.validate(expected, turn)?;
        let source = self.load_snapshot(receipt.generation, &receipt.raw_hash)?;
        if serde_json::to_vec(&source)? != serde_json::to_vec(expected)? {
            return Err(invalid("構造化抽出の保存source snapshotが一致しません"));
        }
        Ok(Some(receipt))
    }
    fn attempt_directory(&self, epoch: u64, create: bool) -> io::Result<Option<PathBuf>> {
        let mut parent = self.directory.clone();
        reject_symlink(&parent)?;
        for name in ["structured-attempts".to_string(), epoch.to_string()] {
            let directory = parent.join(name);
            match fs::symlink_metadata(&directory) {
                Ok(metadata) if metadata.file_type().is_dir() => {}
                Ok(_) => return Err(invalid("構造化抽出の保存先がdirectoryではありません")),
                Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                    private_directory(&directory)?;
                    File::open(&parent)?.sync_all()?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            }
            parent = directory;
        }
        Ok(Some(parent))
    }

    pub(crate) fn read_structured_attempt(
        &self,
        epoch: u64,
        turn: u64,
    ) -> io::Result<Option<StructuredAttemptReceipt>> {
        let Some(directory) = self.attempt_directory(epoch, false)? else {
            return Ok(None);
        };
        let path = directory.join(format!("{turn}.json"));
        let metadata = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !metadata.is_file() || metadata.len() > MAX_RECEIPT_BYTES {
            return Err(invalid("構造化抽出記録の形式・大きさが上限外です"));
        }
        let mut bytes = Vec::new();
        File::open(path)?
            .take(MAX_RECEIPT_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_RECEIPT_BYTES {
            return Err(invalid("構造化抽出記録が1MiBを超えました"));
        }
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    /// The caller supplies an exact ID from its frozen marker, not a DB row.
    pub(crate) fn structured_publication_receipt(
        &self,
        epoch: u64,
        maximum_generation: u64,
        id: &str,
    ) -> io::Result<StructuredDelta> {
        let (turn, _) = id
            .strip_prefix("structured10-")
            .and_then(|s| s.split_once('-'))
            .ok_or_else(|| invalid("構造化公開IDが不正です"))?;
        let turn = turn
            .parse::<u64>()
            .map_err(|_| invalid("構造化公開turnが不正です"))?;
        let receipt = self
            .read_structured_attempt(epoch, turn)?
            .ok_or_else(|| invalid("構造化公開の保存記録がありません"))?;
        let source = self.load_snapshot(receipt.generation, &receipt.raw_hash)?;
        receipt.validate(&source, turn)?;
        let response = receipt
            .response
            .ok_or_else(|| invalid("構造化公開の応答がありません"))?;
        if receipt.epoch != epoch
            || response.delta.publication.publication_id != id
            || u64::try_from(response.delta.publication.scope.generation)
                .ok()
                .is_none_or(|generation| generation > maximum_generation)
        {
            return Err(invalid("構造化公開が許可された世代・IDと一致しません"));
        }
        Ok(response.delta)
    }
}

impl LockedConversation<'_> {
    pub(crate) fn check_structured_turn(
        &self,
        expected: &ConversationSnapshot,
        turn: u64,
    ) -> io::Result<()> {
        self.check_snapshot(expected)?;
        let state = &expected.state;
        if state.history_mode != HistoryMode::Structured10
            || state.visible_summary_ids.len() != state.visible_structured_publication_ids.len()
            || expected.expired_turns().into_iter().find(|id| {
                !state
                    .visible_summary_ids
                    .iter()
                    .any(|summary| summary.starts_with(&format!("strict10-{id}-")))
            }) != Some(turn)
        {
            return Err(invalid("構造化抽出の対象が次の期限切れturnと一致しません"));
        }
        crate::conversation_memory::structured::sources::publication_view(state)?
            .check_receipt_capacity(Some(&format!(
                "structured10-{turn}-{}",
                &expected.raw_hash[..16]
            )))
            .map_err(io::Error::other)?;
        expected.validate_complete_turn(turn)
    }

    fn write_structured_attempt(&self, receipt: &StructuredAttemptReceipt) -> io::Result<()> {
        let bytes = serde_json::to_vec(receipt)?;
        if bytes.len() as u64 > MAX_RECEIPT_BYTES {
            return Err(invalid("構造化抽出記録が1MiBを超えました"));
        }
        let directory = self.store.attempt_directory(receipt.epoch, true)?.unwrap();
        let path = directory.join(format!("{}.json", receipt.turn));
        if path.exists() {
            reject_symlink(&path)?;
        }
        fail_point("structured_attempt_before_write")?;
        atomic_write(&path, &bytes)?;
        fail_point("structured_attempt_after_write")
    }

    /// Only the successful first durable write returns `true`. It is never serialized.
    pub(crate) fn begin_structured_attempt(
        &self,
        expected: &ConversationSnapshot,
        turn: u64,
    ) -> io::Result<(StructuredAttemptReceipt, bool)> {
        self.check_structured_turn(expected, turn)?;
        if let Some(receipt) = self.store.bound_structured_attempt(expected, turn)? {
            return Ok((receipt, false));
        }
        // Only the first unpublished turn can start, so all earlier attempts
        // are marker-visible. No directory scan or separate mutable counter is needed.
        if expected.state.visible_structured_publication_ids.len() >= MAX_ATTEMPTS {
            return Err(invalid("構造化抽出記録がepoch内の上限に達しました"));
        }
        let receipt = StructuredAttemptReceipt {
            schema_version: RECEIPT_VERSION,
            project_id: expected.state.project_id.clone(),
            session_id: expected.state.session_id.clone(),
            epoch: expected.state.epoch,
            generation: expected.state.generation,
            raw_offset: expected.raw_offset,
            raw_hash: expected.raw_hash.clone(),
            turn,
            prompt_version: STRUCTURED_PROMPT_VERSION.into(),
            intent: structured_summary_intent(expected)?
                .ok_or_else(|| invalid("構造化抽出sourceがありません"))?,
            response: None,
        };
        receipt.validate(expected, turn)?;
        self.save_snapshot(expected)?;
        self.write_structured_attempt(&receipt)?;
        Ok((receipt, true))
    }

    pub(crate) fn save_structured_attempt_response(
        &self,
        expected: &ConversationSnapshot,
        turn: u64,
        response: &ValidatedStructuredResponse,
    ) -> io::Result<()> {
        self.check_structured_turn(expected, turn)?;
        let mut receipt = self
            .store
            .bound_structured_attempt(expected, turn)?
            .ok_or_else(|| invalid("構造化抽出の開始記録がありません"))?;
        receipt.validate(expected, turn)?;
        let saved = SavedResponse {
            summary: response.summary.clone(),
            delta: response.delta.clone(),
            coverage: response.coverage.clone(),
        };
        if let Some(prior) = &receipt.response {
            return if prior == &saved {
                Ok(())
            } else {
                Err(invalid("保存済み構造化応答を置換できません"))
            };
        }
        receipt.response = Some(saved);
        receipt.validate(expected, turn)?;
        self.write_structured_attempt(&receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_owner_attempt_ceiling_keeps_raw_and_does_not_create_an_excess_receipt() {
        for count in [MAX_ATTEMPTS - 1, MAX_ATTEMPTS] {
            let temp = tempfile::tempdir().unwrap();
            let mut state = ConversationStateV2::new(
                "project".into(),
                "00000000-0000-4000-8000-000000000001".into(),
            )
            .unwrap();
            state.history_mode = HistoryMode::Structured10;
            let store = ConversationStore::create(temp.path(), state).unwrap();
            let mut lock = store.lock().unwrap();
            for turn in 1..=11 {
                lock.append(Message::user(format!("filler {turn}")), true)
                    .unwrap();
            }
            let raw = lock.snapshot().unwrap();
            let mut next = raw.state.clone();
            // Seed only the raw owner's marker shape to isolate its receipt
            // capacity. This does not stand in for SQLite grants or real sends.
            next.visible_summary_ids = (2..count + 2)
                .map(|turn| format!("strict10-{turn}-aaaaaaaaaaaaaaaa"))
                .collect();
            next.visible_structured_publication_ids = (2..count + 2)
                .map(|turn| format!("structured10-{turn}-aaaaaaaaaaaaaaaa"))
                .collect();
            lock.publish(&raw, next).unwrap();
            let expected = lock.snapshot().unwrap();
            let attempt = lock.begin_structured_attempt(&expected, 1);
            if count < MAX_ATTEMPTS {
                assert!(attempt.unwrap().1);
            } else {
                assert!(
                    attempt
                        .unwrap_err()
                        .to_string()
                        .contains("receipt grant limit")
                );
                assert!(store.read_structured_attempt(0, 1).unwrap().is_none());
            }
            assert_eq!(lock.snapshot().unwrap().raw_hash, raw.raw_hash);
            assert_eq!(lock.snapshot().unwrap().state, expected.state);
        }
    }
}
