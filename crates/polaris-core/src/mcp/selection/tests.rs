//! Regression checks for MCP selection budgets, schemas, and stale catalogs.

use super::*;
use serde_json::json;

fn tool(server: &str, name: &str, description: &str) -> Item {
    Item {
        id: ItemId {
            server_id: server.into(),
            kind: Kind::Tool,
            key: name.into(),
        },
        name: name.into(),
        description: description.into(),
        metadata: json!({}),
        input_schema: Some(
            json!({"type":"object","properties":{"value":{"type":"integer"}},"required":["value"],"additionalProperties":false}),
        ),
        output_schema: None,
        schema_hash: Some(format!("schema-{server}-{name}")),
        alias: Some(format!("mcp_{server}_{name}")),
    }
}

fn catalog(items: Vec<Item>, generation: &str) -> Catalog {
    Catalog {
        generation: generation.into(),
        items,
        encoded_bytes: 1024,
        rejected_items: 0,
    }
}

#[test]
fn kind_filter_precedes_candidate_limit() {
    let mut items = (0..12)
        .map(|i| tool("s", &format!("tool-{i}"), ""))
        .collect::<Vec<_>>();
    let mut resource = tool("s", "note", "");
    resource.id.kind = Kind::Resource;
    items.push(resource);
    let catalog = catalog(items, "g");
    let view = CatalogView::checked(vec![("s", &catalog)]).unwrap();
    let found = view
        .candidates_matching("", Some("s"), "", Some(Kind::Resource))
        .unwrap();
    assert_eq!(found.candidates.len(), 1);
    assert_eq!(found.candidates[0].key, "note");
    assert!(!found.truncated);
}

#[test]
fn search_distinguishes_exact_count_from_count_and_token_truncation() {
    for count in [12, 13] {
        let catalog = catalog(
            (0..count).map(|i| tool("s", &i.to_string(), "")).collect(),
            "g",
        );
        let view = CatalogView::checked(vec![("s", &catalog)]).unwrap();
        let found = view.candidates_matching("", Some("s"), "", None).unwrap();
        assert_eq!(found.candidates.len(), 12);
        assert_eq!(found.truncated, count == 13);
    }
    let catalog = catalog(
        (0..12)
            .map(|i| tool("s", &i.to_string(), &"資料".repeat(80)))
            .collect(),
        "g",
    );
    let view = CatalogView::checked(vec![("s", &catalog)]).unwrap();
    let found = view.candidates_matching("", Some("s"), "", None).unwrap();
    assert!(!found.candidates.is_empty());
    assert!(found.candidates.len() < 12);
    assert!(found.truncated);
    assert!(
        count_tokens(&serde_json::to_string(&found.candidates).unwrap()) <= MCP_CANDIDATE_LIMIT
    );
}

#[test]
fn selection_is_next_request_only_and_does_not_change_current_dispatch() {
    let catalog = catalog(
        vec![
            tool("a", "edit", "edit scene"),
            tool("a", "read", "read scene"),
        ],
        "g1",
    );
    let view = CatalogView::checked(vec![("a", &catalog)]).unwrap();
    let mut pending = PendingSelection::default();
    pending.select(&view, &catalog.items[0].id, "g1").unwrap();
    let first = pending.freeze(&view).unwrap();
    assert_eq!(first.specs().len(), 1);
    assert_eq!(
        first
            .resolve(&view, "mcp_a_edit", &json!({"value":3}))
            .unwrap()
            .id
            .key,
        "edit"
    );
    assert_eq!(
        first.resolve(&view, "mcp_a_edit", &json!({"value":"3"})),
        Err(Error::InvalidArguments)
    );
    pending.select(&view, &catalog.items[1].id, "g1").unwrap();
    assert_eq!(
        first.resolve(&view, "mcp_a_read", &json!({"value":3})),
        Err(Error::InvalidSelection)
    );
    let second = pending.freeze(&view).unwrap();
    assert_eq!(second.specs()[0].name, "mcp_a_read");
    assert!(pending.freeze(&view).unwrap().specs().is_empty());
    assert_eq!(
        second.resolve(&view, "mcp_a_edit", &json!({"value":3})),
        Err(Error::InvalidSelection)
    );
}

#[test]
fn generation_schema_server_and_item_removal_invalidate_selections() {
    let original = catalog(vec![tool("a", "edit", "scene")], "g1");
    let view = CatalogView::checked(vec![("a", &original)]).unwrap();
    let mut pending = PendingSelection::default();
    pending.select(&view, &original.items[0].id, "g1").unwrap();
    let frozen = pending.freeze(&view).unwrap();
    for changed in [
        catalog(original.items.clone(), "g2"),
        catalog(Vec::new(), "g1"),
        {
            let mut item = original.items[0].clone();
            item.schema_hash = Some("new".into());
            catalog(vec![item], "g1")
        },
    ] {
        let fresh = CatalogView::checked(vec![("a", &changed)]).unwrap();
        assert!(
            frozen
                .resolve(&fresh, "mcp_a_edit", &json!({"value":1}))
                .is_err()
        );
    }
    let other = catalog(vec![tool("b", "edit", "scene")], "g1");
    let fresh = CatalogView::checked(vec![("b", &other)]).unwrap();
    assert_eq!(
        frozen.resolve(&fresh, "mcp_a_edit", &json!({"value":1})),
        Err(Error::SelectionStale)
    );
    pending.select(&view, &original.items[0].id, "g1").unwrap();
    assert!(pending.freeze(&fresh).is_err());
    assert!(
        pending.freeze(&view).unwrap().specs().is_empty(),
        "stale pending selection must be consumed"
    );
}

#[test]
fn schema_budget_is_enforced_before_compile_and_invalid_schemas_never_select() {
    let mut invalid = tool("a", "invalid", "bad ref");
    invalid.input_schema = Some(json!({"$ref":"https://invalid.example/private"}));
    let mut oversized = tool("a", "large", "large schema");
    oversized.input_schema = Some(
        json!({"type":"string","enum":(0..10_000).map(|n|format!("indivisible_value_{n}")).collect::<Vec<_>>()}),
    );
    let mut items = vec![invalid, oversized];
    items.extend((0..5).map(|n| tool("a", &format!("ok{n}"), "small")));
    let catalog = catalog(items, "g1");
    let view = CatalogView::checked(vec![("a", &catalog)]).unwrap();
    let mut pending = PendingSelection::default();
    assert_eq!(
        pending.select(&view, &catalog.items[0].id, "g1"),
        Err(Error::InvalidSchema)
    );
    assert_eq!(
        pending.select(&view, &catalog.items[1].id, "g1"),
        Err(Error::ContextLimit)
    );
    for item in &catalog.items[2..6] {
        pending.select(&view, &item.id, "g1").unwrap();
    }
    assert_eq!(
        pending.select(&view, &catalog.items[6].id, "g1"),
        Err(Error::ContextLimit)
    );
    assert_eq!(pending.freeze(&view).unwrap().specs().len(), 4);
}

#[test]
fn candidates_are_deterministic_scoped_bounded_and_do_not_fill_irrelevant_slots() {
    let a = catalog(
        vec![
            tool("a", "read", "read scene verify"),
            tool("a", "delete", "delete scene"),
            tool("a", "識別", "日本語の名前を識別する"),
        ],
        "ga",
    );
    let b = catalog(vec![tool("b", "read", "read image")], "gb");
    let view = CatalogView::checked(vec![("b", &b), ("a", &a)]).unwrap();
    let reverse = CatalogView::checked(vec![("a", &a), ("b", &b)]).unwrap();
    assert_eq!(
        serde_json::to_value(view.candidates("read", None, "verify").unwrap()).unwrap(),
        serde_json::to_value(reverse.candidates("read", None, "verify").unwrap()).unwrap()
    );
    let hits = view.candidates("read", None, "verify").unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].server_id, "a");
    assert!(
        view.candidates("missing", None, "verify")
            .unwrap()
            .is_empty()
    );
    assert_eq!(view.candidates("日本語", None, "").unwrap().len(), 1);
    assert_eq!(view.candidates("", Some("b"), "").unwrap().len(), 1);
    let large = catalog(
        (0..200)
            .map(|n| tool("a", &format!("read{n}"), &"資料の説明".repeat(100)))
            .collect(),
        &"a".repeat(64),
    );
    let hits = CatalogView::checked(vec![("a", &large)])
        .unwrap()
        .candidates("read", None, "")
        .unwrap();
    assert!(!hits.is_empty());
    assert!(hits.len() <= 12);
    assert!(count_tokens(&serde_json::to_string(&hits).unwrap()) <= MCP_CANDIDATE_LIMIT);
    for phase in crate::workflow::Phase::ALL {
        let hits = view.candidates("read", None, phase.as_str()).unwrap();
        assert!(count_tokens(&serde_json::to_string(&hits).unwrap()) <= MCP_CANDIDATE_LIMIT);
    }
}

#[test]
fn run_catalog_limits_and_partial_counts_are_not_hidden() {
    let mut catalog = catalog(Vec::new(), "g1");
    catalog.rejected_items = 3;
    catalog.encoded_bytes = 1024 * 1024;
    assert_eq!(
        CatalogView::checked(vec![("a", &catalog)])
            .unwrap()
            .rejected_items()
            .unwrap(),
        3
    );
    assert!(CatalogView::checked(vec![("a", &catalog), ("a", &catalog)]).is_err());
    assert!(
        CatalogView::checked(vec![
            ("a", &catalog),
            ("b", &catalog),
            ("c", &catalog),
            ("d", &catalog)
        ])
        .is_ok()
    );
    assert!(
        CatalogView::checked(vec![
            ("a", &catalog),
            ("b", &catalog),
            ("c", &catalog),
            ("d", &catalog),
            ("e", &catalog)
        ])
        .is_err()
    );
    catalog.encoded_bytes = MAX_RUN_CATALOG_BYTES + 1;
    assert!(CatalogView::checked(vec![("a", &catalog)]).is_err());
}

#[test]
fn holding_the_same_view_cannot_bypass_connection_invalidation() {
    let catalog = catalog(vec![tool("a", "read", "scene")], "g1");
    let valid = std::cell::Cell::new(true);
    let view = CatalogView {
        sources: vec![CatalogSource::Fixture("a", &catalog, Some(&valid))],
    };
    let mut pending = PendingSelection::default();
    pending.select(&view, &catalog.items[0].id, "g1").unwrap();
    let frozen = pending.freeze(&view).unwrap();
    assert!(
        frozen
            .resolve(&view, "mcp_a_read", &json!({"value":1}))
            .is_ok()
    );
    assert_eq!(view.candidates("read", None, "").unwrap().len(), 1);
    pending.select(&view, &catalog.items[0].id, "g1").unwrap();
    valid.set(false);
    assert_eq!(
        frozen.resolve(&view, "mcp_a_read", &json!({"value":1})),
        Err(Error::CatalogUnavailable)
    );
    assert!(view.candidates("read", None, "").is_err());
    assert!(view.rejected_items().is_err());
    assert!(pending.freeze(&view).is_err());
    valid.set(true);
    assert!(pending.freeze(&view).unwrap().specs().is_empty());
}
