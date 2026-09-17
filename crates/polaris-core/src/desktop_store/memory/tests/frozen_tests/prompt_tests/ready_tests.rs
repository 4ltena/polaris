//! Ready must return the writer's projection, never the worker snapshot's raw.
use super::*;
use crate::desktop_memory::{DesktopMemory, PreparedDesktopHistory};

async fn prepare(stage: u8) -> (PrototypeRoot, Writer, io::Result<PreparedDesktopHistory>) {
    let (root, mut writer, _reader) = selected(stage);
    let saved = writer.snapshot().unwrap();
    let run = &saved.state.runs.last().unwrap().run;
    let target = RunTarget {
        run_id: run.run_id.clone(),
        attempt_id: run.attempt_id.clone(),
    };
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    MemoryStore::open(&database).unwrap();
    let (memory, mut owner) =
        DesktopMemory::pair(target, Arc::new(history()), database, resources()).unwrap();
    let task = tokio::spawn(async move { memory.prepare().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !task.is_finished() {
            owner.poll(Some(&mut writer), false);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        writer.snapshot().unwrap().marker.raw_hash,
        saved.marker.raw_hash
    );
    (root, writer, task.await.unwrap())
}

#[tokio::test]
async fn history_ready_joins_only_confirmed_prompt_and_keeps_durable_raw_separate() {
    let (_root, writer, prepared) = prepare(2).await;
    let prepared = prepared.unwrap();
    let saved = writer.snapshot().unwrap();
    let last = prepared.messages.last().unwrap();
    assert!(last.content.starts_with("real user input\n\n"));
    assert!(last.content.contains("untrusted server supplement"));
    assert_eq!(
        prepared
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .count(),
        2
    );
    assert_eq!(
        saved
            .raw
            .iter()
            .filter(|e| e.message.role == Role::User)
            .count(),
        3
    );
    assert_eq!(
        saved
            .state
            .runs
            .last()
            .unwrap()
            .memory
            .as_ref()
            .unwrap()
            .phase,
        MemoryPhase::Ready
    );
}

#[tokio::test]
async fn history_ready_rejects_unconfirmed_current_prompt_before_publishing_ready() {
    for stage in [0, 1, 3, 4, 5] {
        let (_root, writer, prepared) = prepare(stage).await;
        assert!(prepared.is_err(), "stage {stage}");
        let saved = writer.snapshot().unwrap();
        assert_ne!(
            saved
                .state
                .runs
                .last()
                .unwrap()
                .memory
                .as_ref()
                .unwrap()
                .phase,
            MemoryPhase::Ready
        );
    }
}
