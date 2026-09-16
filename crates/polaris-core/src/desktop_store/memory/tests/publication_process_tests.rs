//! A failed publication marker must recover across an actual process boundary.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn structured_marker_failure_recovers_saved_response_and_embedding_in_new_process() {
    let (root, mut writer, target) = setup_mode(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
    );
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (work, pending, response) = structured_pending(&mut writer, &target).await;
    super::super::super::disk::FAIL.with(|flag| flag.set(Some("marker_rename")));
    let failed = writer.publish_structured_memory(&target, &work, &pending, &response, &database);
    super::super::super::disk::FAIL.with(|flag| flag.set(None));
    assert!(failed.is_err());
    writer.recover().unwrap();
    let before = writer.snapshot().unwrap();
    assert!(
        before
            .state
            .conversation_memory
            .as_ref()
            .unwrap()
            .sources
            .is_empty()
    );
    let stored = MemoryStore::open(&database).unwrap();
    stored.verify_structured_delta(&response.delta).unwrap();
    drop(stored);
    let metadata = directory.path().join("process.json");
    std::fs::write(
        &metadata,
        serde_json::to_vec(&serde_json::json!({
            "root":writer.test_directory().parent().unwrap(),"database":database,
            "project":before.marker.project_id,"session":before.marker.session_id,
            "parent_pid":std::process::id()
        }))
        .unwrap(),
    )
    .unwrap();
    drop(writer);
    let output = tokio::time::timeout(std::time::Duration::from_secs(120),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "desktop_store::memory::tests::publication_process_tests::structured_marker_recovery_child", "--ignored", "--nocapture"])
            .env("POLARIS_STRUCTURED_MARKER_FIXTURE", &metadata).kill_on_drop(true).output()
    ).await.expect("owned synthetic marker recovery child timeout").unwrap();
    assert!(
        output.status.success(),
        "child stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("structured_marker_recovery "));
    print!("{stdout}");
    let writer = root
        .open(&before.marker.project_id, &before.marker.session_id)
        .unwrap();
    let after = writer.snapshot().unwrap();
    assert_eq!(
        serde_json::to_vec(&after.raw[..before.raw.len()]).unwrap(),
        serde_json::to_vec(&before.raw).unwrap()
    );
    assert_eq!(
        after.raw.iter().filter(|event| event.starts_turn).count(),
        12
    );
    let memory = after.state.conversation_memory.as_ref().unwrap();
    assert_eq!(memory.sources.len(), 2);
    assert_eq!(
        memory.attempts[0],
        before.state.conversation_memory.as_ref().unwrap().attempts[0]
    );
    assert!(
        memory
            .sources
            .iter()
            .any(|source| source.structured_publication_id.as_deref()
                == Some(&response.delta.publication.publication_id))
    );
    assert_eq!(
        after
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
    MemoryStore::open(&database)
        .unwrap()
        .verify_structured_delta(&response.delta)
        .unwrap();
}

struct NewSummary(AtomicUsize);
impl StrictSummaryProvider for NewSummary {
    fn summarize<'a>(&'a self, request: SummaryRequest) -> FutureResult<'a, String> {
        assert_eq!(
            request.source_turn_id, 2,
            "the stored turn 1 response must not be resent"
        );
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(serde_json::json!({
            "summary":{"facts":["Second fixture summary"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[2]},
            "processed_source_ids":request.structured_sources.unwrap().extraction_source_ids,
            "entity_candidates":[],"fact_deltas":[],"entity_relations":[]
        }).to_string())
        })
    }
}

struct CountedEmbed {
    inner: Embed,
    passages: AtomicUsize,
}
impl StrictEmbedder for CountedEmbed {
    fn metadata(&self) -> &EmbeddingModel {
        self.inner.metadata()
    }
    fn embed_passage<'a>(&'a self, text: &'a str) -> FutureResult<'a, Vec<f32>> {
        self.passages.fetch_add(1, Ordering::SeqCst);
        self.inner.embed_passage(text)
    }
    fn embed_query<'a>(&'a self, text: &'a str) -> FutureResult<'a, Vec<Vec<f32>>> {
        self.inner.embed_query(text)
    }
}

#[tokio::test]
#[ignore = "親テストがmarker保存失敗後の一時保存先を指定するfixture"]
async fn structured_marker_recovery_child() {
    let path =
        std::env::var_os("POLARIS_STRUCTURED_MARKER_FIXTURE").expect("parent-owned fixture path");
    let input: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let parent_pid = input["parent_pid"].as_u64().unwrap();
    assert_ne!(u64::from(std::process::id()), parent_pid);
    let database = std::path::PathBuf::from(input["database"].as_str().unwrap());
    let root_path = std::path::PathBuf::from(input["root"].as_str().unwrap());
    assert!(
        database.is_absolute()
            && database.is_file()
            && root_path.is_absolute()
            && root_path.is_dir()
    );
    let root = DesktopRoot::open_owned(&root_path).unwrap();
    let mut writer = root
        .open(
            &ProjectId::new(input["project"].as_str().unwrap()).unwrap(),
            &SessionId::new(input["session"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
    let before = writer.snapshot().unwrap();
    let old = before.state.conversation_memory.as_ref().unwrap();
    assert!(old.sources.is_empty());
    assert_eq!(old.attempts.len(), 1);
    assert!(
        old.attempts[0]
            .structured
            .as_ref()
            .unwrap()
            .response
            .is_some()
    );
    let target = accept(&mut writer, 12);
    writer
        .prepare_structured_memory_index(&target, &database, &resources())
        .unwrap();
    writer.bind_memory(&target, resources()).unwrap();
    let raw_hash = writer.snapshot().unwrap().marker.raw_hash;
    let summary = Arc::new(NewSummary(AtomicUsize::new(0)));
    let embed = Arc::new(CountedEmbed {
        inner: Embed(EmbeddingModel {
            model: "fixture".into(),
            revision: "fixed".into(),
            dimension: 2,
        }),
        passages: AtomicUsize::new(0),
    });
    let (runtime, mut owner) = crate::desktop_memory::DesktopMemory::pair(
        target.clone(),
        Arc::new(StrictHistory::new(summary.clone(), embed.clone())),
        database,
        resources(),
    )
    .unwrap();
    let task = tokio::spawn(async move { runtime.prepare().await });
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        while !task.is_finished() {
            owner.poll(Some(&mut writer), false);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(summary.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        embed.passages.load(Ordering::SeqCst),
        1,
        "the existing pending embedding must be reused"
    );
    let state = writer.snapshot().unwrap();
    let current = writer.memory_snapshot(&target).unwrap();
    assert_eq!(state.marker.raw_hash, raw_hash);
    assert_eq!(current.conversation.state.visible_summary_ids.len(), 2);
    assert_eq!(
        current
            .conversation
            .state
            .visible_structured_publication_ids
            .len(),
        2
    );
    assert_eq!(
        state.state.conversation_memory.as_ref().unwrap().attempts[0],
        old.attempts[0]
    );
    assert_eq!(
        state
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
    finish(&mut writer, &target, 12);
    println!(
        "structured_marker_recovery {}",
        serde_json::json!({"child_pid":std::process::id(),"parent_pid":parent_pid,"summary_calls":1,"passage_calls":1,"raw_user_turns":12,"live_provider":false})
    );
}
