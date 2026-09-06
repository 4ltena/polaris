//! Persistence, isolation, retrieval budgets and explicit embedding behavior.
use polaris_memory::{
    Embedding, Error, MAX_EXCERPT_BYTES, MAX_RESULTS, MemoryStore, Record, SearchMode,
    SearchRequest,
};

fn record(project: &str, session: &str, id: &str, text: &str) -> Record {
    Record {
        project_id: project.into(),
        session_id: session.into(),
        id: id.into(),
        role: "user".into(),
        timestamp: "2026-09-05T00:00:00Z".into(),
        source: "/synthetic/archive.jsonl:1".into(),
        text: text.into(),
    }
}

fn embedding(model: &str, values: &[f32]) -> Embedding {
    Embedding {
        model: model.into(),
        values: values.into(),
    }
}

fn semantic<'a>(project: &'a str, vector: &'a Embedding) -> SearchRequest<'a> {
    SearchRequest {
        mode: SearchMode::Semantic(vector),
        ..SearchRequest::lexical(project, "unused")
    }
}

#[test]
fn japanese_identifiers_and_literal_query_syntax() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    let original = record(
        "project",
        "session",
        "id",
        "設計を確認する。日本語の長期記憶。HTTP_Client::send %_needle'",
    );
    store.upsert(&original, None).unwrap();
    for query in [
        "長期記憶",
        "http_client::SEND",
        "%_needle'",
        "日本語 HTTP_Client",
    ] {
        let hits = store
            .search(&SearchRequest::lexical("project", query))
            .unwrap();
        assert_eq!(hits.len(), 1, "query: {query}");
        assert_eq!(hits[0].source, original.source);
    }
    assert!(
        store
            .search(&SearchRequest::lexical("project", "長期記憶 missing"))
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .search(&SearchRequest::lexical("project", "' OR 1=1 --"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.get("project", "session", "id").unwrap(),
        Some(original)
    );
}

#[test]
fn project_filter_precedes_lexical_and_semantic_ranking() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    store
        .upsert(
            &record("a", "s", "same", "一致"),
            Some(&embedding("m", &[0.0, 1.0])),
        )
        .unwrap();
    store
        .upsert(
            &record("b", "s", "same", "一致 一致 一致"),
            Some(&embedding("m", &[1.0, 0.0])),
        )
        .unwrap();
    let vector = embedding("m", &[1.0, 0.0]);
    for mut request in [SearchRequest::lexical("a", "一致"), semantic("a", &vector)] {
        request.limit = 1;
        let hits = store.search(&request).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].project_id, "a");
    }
    assert_eq!(store.get_by_id("a", "same").unwrap().unwrap().text, "一致");
    assert!(store.get("unknown", "s", "same").unwrap().is_none());
}

#[test]
fn session_filter_and_ambiguous_ids_are_explicit() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    for session in ["s1", "s2"] {
        store
            .upsert(&record("p", session, "same", "一致"), None)
            .unwrap();
    }
    assert!(matches!(
        store.get_by_id("p", "same"),
        Err(Error::AmbiguousRecordId)
    ));
    let mut query = SearchRequest::lexical("p", "一致");
    query.session_id = Some("s2");
    let hits = store.search(&query).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, "s2");
}

#[test]
fn upsert_deduplicates_replaces_provenance_and_clears_stale_vectors() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    let vector = embedding("m", &[1.0]);
    let mut original = record("p", "s", "id", "old text");
    store.upsert(&original, Some(&vector)).unwrap();
    store.upsert(&original, Some(&vector)).unwrap();
    assert_eq!(store.search(&semantic("p", &vector)).unwrap().len(), 1);
    original.text = "new text".into();
    original.source = "updated-source".into();
    store.upsert(&original, None).unwrap();
    assert_eq!(store.get("p", "s", "id").unwrap(), Some(original));
    assert!(
        store
            .search(&SearchRequest::lexical("p", "old"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .search(&SearchRequest::lexical("p", "new"))
            .unwrap()
            .len(),
        1
    );
    assert!(store.search(&semantic("p", &vector)).unwrap().is_empty());
}

#[test]
fn model_dimension_and_missing_embeddings_never_mix() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    for (id, vector) in [
        ("matched", Some(embedding("m", &[1.0, 0.0]))),
        ("opposite", Some(embedding("m", &[-1.0, 0.0]))),
        ("other_model", Some(embedding("other", &[1.0, 0.0]))),
        ("other_dimension", Some(embedding("m", &[1.0]))),
        ("lexical_only", None),
    ] {
        store
            .upsert(&record("p", "s", id, "記録"), vector.as_ref())
            .unwrap();
    }
    let hits = store
        .search(&semantic("p", &embedding("m", &[1.0, 0.0])))
        .unwrap();
    assert_eq!(
        hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
        ["matched", "opposite"]
    );
    assert!((hits[0].score - 1.0).abs() < 1e-12);
    assert!((hits[1].score + 1.0).abs() < 1e-12);
    assert!(
        store
            .search(&semantic("p", &embedding("unknown", &[1.0, 0.0])))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .search(&SearchRequest::lexical("p", "記録"))
            .unwrap()
            .len(),
        5
    );
}

#[test]
fn forgetting_removes_vectors_and_persists_tombstones_across_connections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.sqlite3");
    let mut store = MemoryStore::open(&path).unwrap();
    let mut importer = MemoryStore::open(&path).unwrap();
    let vector = embedding("m", &[1.0]);
    for (project, session) in [("p", "s"), ("p", "keep"), ("other", "s")] {
        store
            .upsert(&record(project, session, "id", "記録"), Some(&vector))
            .unwrap();
    }
    assert!(!importer.is_session_forgotten("p", "s").unwrap());
    assert_eq!(store.delete_session("p", "s").unwrap(), 1);
    assert_eq!(store.delete_session("p", "s").unwrap(), 0);
    assert!(importer.is_session_forgotten("p", "s").unwrap());
    assert!(matches!(
        importer.upsert(&record("p", "s", "new-id", "復活"), Some(&vector)),
        Err(Error::SessionForgotten)
    ));
    drop(store);
    let store = MemoryStore::open(&path).unwrap();
    assert!(store.get("p", "s", "id").unwrap().is_none());
    assert!(store.is_session_forgotten("p", "s").unwrap());
    assert!(!store.is_session_forgotten("other", "s").unwrap());
    for request in [SearchRequest::lexical("p", "記録"), semantic("p", &vector)] {
        let hits = store.search(&request).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "keep");
    }
    let connection = rusqlite::Connection::open(&path).unwrap();
    let remaining: i64 = connection
        .query_row("SELECT count(*) FROM memory_vectors", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 2);
    assert_eq!(store.search(&semantic("other", &vector)).unwrap().len(), 1);
}

#[test]
fn forgetting_an_unimported_session_also_prevents_resurrection() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    assert_eq!(store.delete_session("p", "s").unwrap(), 0);
    assert!(matches!(
        store.upsert(&record("p", "s", "id", "記録"), None),
        Err(Error::SessionForgotten)
    ));
}

#[test]
fn result_and_excerpt_budgets_preserve_utf8_and_full_text() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    let body = format!("{}一致{}", "前文。".repeat(1000), "後文。".repeat(1000));
    for n in 0..MAX_RESULTS + 7 {
        store
            .upsert(&record("p", "s", &format!("{n:03}"), &body), None)
            .unwrap();
    }
    let mut query = SearchRequest::lexical("p", "一致");
    query.limit = usize::MAX;
    query.excerpt_bytes = usize::MAX;
    let hits = store.search(&query).unwrap();
    assert_eq!(hits.len(), MAX_RESULTS);
    assert!(
        hits.iter()
            .all(|h| h.excerpt.len() <= MAX_EXCERPT_BYTES && h.excerpt.contains("一致"))
    );
    assert_eq!(hits[0].id, "000");
    query.limit = 1;
    for budget in 0..30 {
        query.excerpt_bytes = budget;
        let hits = store.search(&query).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].excerpt.len() <= budget);
    }
    query.limit = 0;
    assert!(store.search(&query).unwrap().is_empty());
    assert_eq!(store.get("p", "s", "000").unwrap().unwrap().text, body);
}

#[test]
fn case_folding_excerpts_map_back_to_original_unicode() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    store
        .upsert(
            &record("p", "s", "id", &format!("{}目標", "İ".repeat(500))),
            None,
        )
        .unwrap();
    let mut query = SearchRequest::lexical("p", "目標");
    query.excerpt_bytes = 18;
    let hits = store.search(&query).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].excerpt.contains("目標"));
    assert!(hits[0].excerpt.len() <= 18);
}

#[test]
fn invalid_embeddings_do_not_partially_replace_a_record() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    let original = record("p", "s", "id", "original");
    let valid = embedding("m", &[f32::MAX, f32::MAX]);
    store.upsert(&original, Some(&valid)).unwrap();
    for invalid in [vec![], vec![0.0], vec![f32::NAN], vec![f32::INFINITY]] {
        let vector = embedding("m", &invalid);
        assert!(
            store
                .upsert(&record("p", "s", "id", "changed"), Some(&vector))
                .is_err()
        );
        assert!(store.search(&semantic("p", &vector)).is_err());
    }
    assert_eq!(store.get("p", "s", "id").unwrap(), Some(original));
    assert!((store.search(&semantic("p", &valid)).unwrap()[0].score - 1.0).abs() < 1e-12);
}

#[test]
fn reopen_preserves_text_provenance_vectors_and_ranking() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.sqlite3");
    let original = record("p", "s", "id", "保存された記録");
    let vector = embedding("model-v1", &[3.0, 4.0]);
    {
        let mut store = MemoryStore::open(&path).unwrap();
        store.upsert(&original, Some(&vector)).unwrap();
    }
    let store = MemoryStore::open(&path).unwrap();
    assert_eq!(store.get_by_id("p", "id").unwrap(), Some(original));
    assert_eq!(
        store
            .search(&SearchRequest::lexical("p", "保存"))
            .unwrap()
            .len(),
        1
    );
    assert!((store.search(&semantic("p", &vector)).unwrap()[0].score - 1.0).abs() < 1e-12);
}

#[test]
fn invalid_metadata_and_empty_or_oversized_queries_are_rejected() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    assert!(store.upsert(&record("", "s", "id", "text"), None).is_err());
    assert!(
        store
            .upsert(&record("p", "s", &"x".repeat(1025), "text"), None)
            .is_err()
    );
    for query in ["   ", &"x".repeat(4097)] {
        assert!(store.search(&SearchRequest::lexical("p", query)).is_err());
    }
}

#[test]
fn hybrid_fuses_a_bounded_filtered_union_with_lexical_excerpts() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    let vector = embedding("m", &[1.0, 0.0]);
    for (id, text, supplied) in [
        ("both", "関連 設計", Some(&vector)),
        ("lexical", "関連 関連 関連", None),
        ("semantic", "異なる表現", Some(&vector)),
    ] {
        store.upsert(&record("p", "s", id, text), supplied).unwrap();
    }
    store
        .upsert(
            &record("other", "s", "outsider", "関連 関連"),
            Some(&vector),
        )
        .unwrap();
    store
        .upsert(
            &record("p", "other", "outsider", "関連 関連"),
            Some(&vector),
        )
        .unwrap();
    let mut query = SearchRequest::lexical("p", "関連");
    query.mode = SearchMode::Hybrid {
        query: "関連",
        embedding: &vector,
    };
    query.session_id = Some("s");
    query.excerpt_bytes = 9;
    let hits = store.search(&query).unwrap();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].id, "both");
    assert!((hits[0].score - (1.0 / 62.0 + 1.0 / 61.0)).abs() < 1e-12);
    assert!(
        hits.iter()
            .all(|h| h.project_id == "p" && h.session_id == "s" && h.excerpt.len() <= 9)
    );
    query.limit = 1;
    assert_eq!(store.search(&query).unwrap().len(), 1);
    query.limit = 0;
    assert!(store.search(&query).unwrap().is_empty());
}

#[test]
fn setting_embeddings_preserves_records_and_respects_forgetting() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    let original = record("p", "s", "id", "記録");
    store.upsert(&original, None).unwrap();
    let vector = embedding("m", &[1.0]);
    store.set_embedding("p", "s", "id", &vector).unwrap();
    assert_eq!(store.get("p", "s", "id").unwrap(), Some(original));
    assert_eq!(store.search(&semantic("p", &vector)).unwrap().len(), 1);
    assert!(matches!(
        store.set_embedding("other", "s", "id", &vector),
        Err(Error::RecordNotFound)
    ));
    let replacement = embedding("new", &[-1.0]);
    store.set_embedding("p", "s", "id", &replacement).unwrap();
    assert!(store.search(&semantic("p", &vector)).unwrap().is_empty());
    assert_eq!(store.search(&semantic("p", &replacement)).unwrap().len(), 1);
    store.delete_session("p", "s").unwrap();
    assert!(matches!(
        store.set_embedding("p", "s", "id", &vector),
        Err(Error::SessionForgotten)
    ));
}

#[test]
fn read_only_does_not_create_initialize_mutate_or_change_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.sqlite3");
    assert!(MemoryStore::open_read_only(&missing).is_err());
    assert!(!missing.exists());
    let empty = dir.path().join("empty.sqlite3");
    std::fs::write(&empty, []).unwrap();
    let empty_store = MemoryStore::open_read_only(&empty).unwrap();
    assert!(
        empty_store
            .search(&SearchRequest::lexical("p", "text"))
            .is_err()
    );
    assert_eq!(std::fs::metadata(&empty).unwrap().len(), 0);
    let path = dir.path().join("memory.sqlite3");
    let original = record("p", "s", "id", "text");
    {
        let mut store = MemoryStore::open(&path).unwrap();
        store.upsert(&original, None).unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    }
    let mut readonly = MemoryStore::open_read_only(&path).unwrap();
    assert_eq!(
        readonly.get_by_id("p", "id").unwrap(),
        Some(original.clone())
    );
    assert_eq!(
        readonly
            .search(&SearchRequest::lexical("p", "text"))
            .unwrap()
            .len(),
        1
    );
    let range = readonly.get_range("p", "s", "id", 1, 2).unwrap().unwrap();
    assert_eq!(range.record.text, "ex");
    assert_eq!(range.total_bytes, 4);
    assert!(readonly.upsert(&original, None).is_err());
    assert!(readonly.delete_session("p", "s").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}

#[test]
fn records_page_is_scoped_ordered_bounded_and_complete() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    for id in ["c", "a", "b"] {
        store
            .upsert(&record("p", "s", id, "full text"), None)
            .unwrap();
        store
            .upsert(&record("other", "s", id, "not visible"), None)
            .unwrap();
        store
            .upsert(&record("p", "other", id, "not visible"), None)
            .unwrap();
    }
    let page = store.records_page("p", "s", None, 2).unwrap();
    assert_eq!(
        page.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
    let next = store.records_page("p", "s", Some(&page[1].id), 2).unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].id, "c");
    assert_eq!(next[0].text, "full text");
    assert!(
        store
            .records_page("p", "s", Some("c"), 2)
            .unwrap()
            .is_empty()
    );
    assert!(store.records_page("p", "s", None, 0).unwrap().is_empty());
    for n in 0..MAX_RESULTS + 5 {
        store
            .upsert(&record("p", "s", &format!("{n:03}"), "text"), None)
            .unwrap();
    }
    assert_eq!(
        store
            .records_page("p", "s", None, usize::MAX)
            .unwrap()
            .len(),
        MAX_RESULTS
    );
}

#[test]
fn offline_excerpt_eval_compares_baseline_bytes_and_evidence_recall() {
    // Fixed synthetic evidence; vectors are supplied fixtures, not model output.
    // Recall here means the answer-bearing span survives retrieval, not answer quality.
    let mut store = MemoryStore::open(":memory:").unwrap();
    let vector = embedding("offline-fixture", &[1.0, 0.0]);
    let fixtures = [
        (
            "ja",
            "採用 色",
            format!(
                "採用の議論。{}採用する色は青色。{}",
                "背景の長い説明。".repeat(200),
                "補足。".repeat(100)
            ),
            "採用する色は青色。",
            false,
        ),
        (
            "identifier",
            "cache_key TTL",
            format!(
                "cache_key introduction. {}cache_key TTL=300; {}",
                "background ".repeat(200),
                "details ".repeat(100)
            ),
            "cache_key TTL=300",
            false,
        ),
        (
            "partial",
            "決定 未出現語",
            format!(
                "{}決定はSQLiteを使う。{}",
                "長い経緯。".repeat(200),
                "補足。".repeat(100)
            ),
            "決定はSQLiteを使う。",
            true,
        ),
    ];
    let (mut baseline_bytes, mut improved_bytes, mut baseline_recall, mut improved_recall) =
        (0, 0, 0, 0);
    for (id, query, body, evidence, hybrid) in &fixtures {
        store
            .upsert(&record("p", id, id, body), Some(&vector))
            .unwrap();
        let mut request = SearchRequest::lexical("p", query);
        request.session_id = Some(id);
        request.excerpt_bytes = 96;
        if *hybrid {
            request.mode = SearchMode::Hybrid {
                query,
                embedding: &vector,
            };
        }
        let hits = store.search(&request).unwrap();
        assert_eq!(hits.len(), 1);
        let improved = &hits[0].excerpt;
        // Previous lexical algorithm picked the first query occurrence; a
        // semantic-only candidate in hybrid search always started at byte zero.
        let offset = if *hybrid {
            0
        } else {
            query
                .split_whitespace()
                .filter_map(|q| body.find(q))
                .min()
                .unwrap()
        };
        let mut start = offset.saturating_sub(96 / 4);
        while !body.is_char_boundary(start) {
            start += 1;
        }
        let mut end = (start + 96).min(body.len());
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        let baseline = &body[start..end];
        baseline_bytes += baseline.len();
        improved_bytes += improved.len();
        baseline_recall += usize::from(baseline.contains(evidence));
        improved_recall += usize::from(improved.contains(evidence));
        assert!(body.contains(improved));
        assert!(improved.len() <= 96);
    }
    assert_eq!(baseline_recall, 0);
    assert_eq!(improved_recall, fixtures.len());
    assert!(improved_bytes <= baseline_bytes);
    eprintln!(
        "offline excerpt fixture: baseline bytes={baseline_bytes}, evidence recall={baseline_recall}/3; improved bytes={improved_bytes}, evidence recall={improved_recall}/3"
    );
}

#[test]
fn semantic_excerpt_uses_only_available_lexical_clues() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    let vector = embedding("m", &[1.0]);
    let body = format!(
        "{}目標は達成。{}",
        "İ前置き".repeat(100),
        "後文。".repeat(50)
    );
    store
        .upsert(&record("p", "s", "id", &body), Some(&vector))
        .unwrap();
    for budget in 0..40 {
        let mut request = semantic("p", &vector);
        request.excerpt_bytes = budget;
        let pure = store.search(&request).unwrap();
        assert!(body.starts_with(&pure[0].excerpt));
        assert!(pure[0].excerpt.len() <= budget);
        request.mode = SearchMode::Hybrid {
            query: "目標 不在",
            embedding: &vector,
        };
        let hybrid = store.search(&request).unwrap();
        assert!(hybrid[0].excerpt.len() <= budget);
        assert!(body.contains(&hybrid[0].excerpt));
        if budget >= "目標".len() {
            assert!(hybrid[0].excerpt.contains("目標"));
        }
        request.mode = SearchMode::Hybrid {
            query: "不在",
            embedding: &vector,
        };
        assert_eq!(store.search(&request).unwrap()[0].excerpt, pure[0].excerpt);
    }
}

#[test]
fn freshness_breaks_exact_ties_only_and_is_stable_before_truncation() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    let vector = embedding("m", &[1.0, 0.0]);
    for (id, timestamp, body, values) in [
        ("a-old", "2026-09-01T00:00:00Z", "決定", [1.0, 0.0]),
        ("z-new", "2026-09-05T00:00:00Z", "決定", [1.0, 0.0]),
        ("b-new", "2026-09-05T00:00:00Z", "決定", [1.0, 0.0]),
        ("future-weak", "2099-01-01T00:00:00Z", "決定", [0.9, 0.1]),
        (
            "old-strong",
            "2020-01-01T00:00:00Z",
            "決定 決定",
            [-1.0, 0.0],
        ),
    ] {
        let mut item = record("p", "s", id, body);
        item.timestamp = timestamp.into();
        store.upsert(&item, Some(&embedding("m", &values))).unwrap();
    }
    let semantic_hits = store.search(&semantic("p", &vector)).unwrap();
    assert_eq!(
        semantic_hits
            .iter()
            .map(|h| h.id.as_str())
            .collect::<Vec<_>>(),
        ["b-new", "z-new", "a-old", "future-weak", "old-strong"]
    );
    let lexical_hits = store.search(&SearchRequest::lexical("p", "決定")).unwrap();
    assert_eq!(lexical_hits[0].id, "old-strong");
    assert_eq!(lexical_hits[1].id, "future-weak");
    for limit in [1, 2] {
        let mut request = semantic("p", &vector);
        request.limit = limit;
        assert_eq!(
            store
                .search(&request)
                .unwrap()
                .iter()
                .map(|h| h.id.as_str())
                .collect::<Vec<_>>(),
            semantic_hits[..limit]
                .iter()
                .map(|h| h.id.as_str())
                .collect::<Vec<_>>()
        );
    }
    // No lexical matches: hybrid retains the semantic ordering and excerpts.
    let mut request = semantic("p", &vector);
    request.mode = SearchMode::Hybrid {
        query: "不在",
        embedding: &vector,
    };
    assert_eq!(store.search(&request).unwrap()[0].id, "b-new");
}

#[test]
fn range_get_checks_unicode_bounds_budgets_scope_and_tombstones() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    let body = format!("A日本語🙂e\u{301}{}", "続き。".repeat(1000));
    let original = record("p", "s", "id", &body);
    store.upsert(&original, None).unwrap();
    for start in [0, 1, 4, 10, body.len(), body.len() + 1, usize::MAX] {
        for budget in [0, 1, 2, 3, 4, 512, usize::MAX] {
            let result = store.get_range("p", "s", "id", start, budget);
            if !body.is_char_boundary(start) {
                assert!(matches!(result, Err(Error::InvalidInput(_))));
                continue;
            }
            let range = result.unwrap().unwrap();
            assert_eq!(range.start, start);
            assert_eq!(range.total_bytes, body.len());
            assert_eq!(range.record.text, body[start..range.end]);
            assert!(range.record.text.len() <= budget.min(MAX_EXCERPT_BYTES));
            assert_eq!(range.record.source, original.source);
        }
    }
    for start in [2, 3, 11, 12, 13] {
        assert!(store.get_range("p", "s", "id", start, 20).is_err());
    }
    let mut cursor = 0;
    let mut reconstructed = String::new();
    while cursor < body.len() {
        let range = store
            .get_range("p", "s", "id", cursor, 37)
            .unwrap()
            .unwrap();
        assert!(range.end > cursor);
        cursor = range.end;
        reconstructed.push_str(&range.record.text);
    }
    assert_eq!(reconstructed, body);
    assert_eq!(store.get("p", "s", "id").unwrap(), Some(original));
    assert!(
        store
            .get_range("other", "s", "id", 0, 10)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_range("p", "other", "id", 0, 10)
            .unwrap()
            .is_none()
    );
    store.delete_session("p", "s").unwrap();
    assert!(store.get_range("p", "s", "id", 0, 10).unwrap().is_none());
}

#[test]
fn native_millisecond_freshness_is_numeric_across_digit_boundaries() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    for (id, timestamp) in [("old", "999"), ("new", "1000"), ("same", "01000")] {
        let mut item = record("p", "s", id, "決定");
        item.timestamp = timestamp.into();
        store.upsert(&item, None).unwrap();
    }
    let hits = store.search(&SearchRequest::lexical("p", "決定")).unwrap();
    assert_eq!(
        hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
        ["new", "same", "old"]
    );
}

#[test]
fn leading_context_does_not_displace_an_exact_budget_match() {
    let mut store = MemoryStore::open(":memory:").unwrap();
    let evidence = "目標についての決定";
    let body = format!(
        "{}{}{}",
        "前置き。".repeat(100),
        evidence,
        "後文。".repeat(100)
    );
    store.upsert(&record("p", "s", "id", &body), None).unwrap();
    let mut request = SearchRequest::lexical("p", "目標 決定");
    request.excerpt_bytes = evidence.len();
    assert_eq!(store.search(&request).unwrap()[0].excerpt, evidence);
}
