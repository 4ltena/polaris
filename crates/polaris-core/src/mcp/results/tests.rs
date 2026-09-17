//! Regression checks for bounded MCP result projection, handles, and expiry.

use super::*;

fn binding() -> RunBinding {
    RunBinding {
        session: "s".into(),
        run: "r".into(),
        attempt: "a".into(),
    }
}
fn source() -> Source {
    Source {
        server: "server".into(),
        origin: SourceOrigin::ModelCall {
            model_call: "call".into(),
        },
        result_generation: "g".into(),
    }
}

#[test]
fn escaped_utf8_chunks_reconstruct_exact_bytes_and_bound_the_full_envelope() {
    let binding = binding();
    let mut store = ResultStore::new(binding.clone()).unwrap();
    let now = Instant::now();
    let raw = "日本語\n\"\\\u{0001}🌟".repeat(12_000);
    let mut chunk = store
        .insert(&binding, source(), Payload::Text(raw.clone()), now)
        .unwrap();
    let mut rebuilt = String::new();
    let mut count = 0;
    loop {
        assert!(serde_json::to_vec(&chunk).unwrap().len() <= MAX_VISIBLE_BYTES);
        assert!(raw.is_char_boundary(chunk.next_offset));
        rebuilt.push_str(chunk.text.as_ref().unwrap());
        count += 1;
        if !chunk.truncated {
            break;
        }
        assert!(chunk.next_offset > chunk.offset);
        chunk = store
            .chunk(
                &binding,
                &chunk.handle,
                chunk.next_offset,
                MAX_VISIBLE_BYTES,
                now,
            )
            .unwrap();
    }
    assert_eq!(rebuilt, raw);
    assert!(count > 1);
    assert!(store.chunk(&binding, &chunk.handle, 1, 10, now).is_err());
    assert!(store.chunk(&binding, &chunk.handle, 0, 1, now).is_err());
    assert!(
        store
            .chunk(&binding, &chunk.handle, usize::MAX, 1, now)
            .is_err()
    );
    assert!(
        store
            .chunk(&binding, &chunk.handle, 0, MAX_VISIBLE_BYTES + 1, now)
            .is_err()
    );
}

#[test]
fn run_attempt_expiry_restart_and_finish_never_retrieve_or_recreate() {
    let binding = binding();
    let now = Instant::now();
    let mut store = ResultStore::new(binding.clone()).unwrap();
    let first = store
        .insert(&binding, source(), Payload::Text("retained".into()), now)
        .unwrap();
    let second = store
        .insert(&binding, source(), Payload::Text("other".into()), now)
        .unwrap();
    assert_ne!(first.handle, second.handle);
    for other in [
        RunBinding {
            session: "fork".into(),
            ..binding.clone()
        },
        RunBinding {
            run: "other".into(),
            ..binding.clone()
        },
        RunBinding {
            attempt: "retry".into(),
            ..binding.clone()
        },
    ] {
        assert!(store.chunk(&other, &first.handle, 0, 32, now).is_err());
    }
    let mut restarted = ResultStore::new(binding.clone()).unwrap();
    assert!(
        restarted
            .chunk(&binding, &first.handle, 0, 32, now)
            .is_err()
    );
    assert!(
        store
            .chunk(
                &binding,
                &first.handle,
                0,
                32,
                now + TTL - Duration::from_nanos(1)
            )
            .is_ok()
    );
    assert!(
        store
            .chunk(&binding, &first.handle, 0, 32, now + TTL)
            .is_err()
    );
    store.finish();
    assert!(
        store
            .insert(&binding, source(), Payload::Text("new".into()), now)
            .is_err()
    );
}

#[test]
fn count_byte_limits_and_nontext_projection_keep_positive_controls() {
    let binding = binding();
    let now = Instant::now();
    let mut store = ResultStore::new(binding.clone()).unwrap();
    let binary = store
        .insert(
            &binding,
            source(),
            Payload::Binary {
                bytes: vec![0, 255, 2, 3],
                mime: "image/png".into(),
            },
            now,
        )
        .unwrap();
    assert_eq!(binary.status, Some("unsupported_modality"));
    assert!(binary.text.is_none());
    assert_eq!(
        store
            .binary_chunk(&binding, &binary.handle, 1, 2, now)
            .unwrap(),
        &[255, 2]
    );
    for _ in 1..16 {
        store
            .insert(&binding, source(), Payload::Text(String::new()), now)
            .unwrap();
    }
    assert_eq!(
        store
            .insert(&binding, source(), Payload::Text("overflow".into()), now)
            .unwrap_err(),
        Error::ContextLimit
    );
    assert!(
        store.chunk(&binding, &binary.handle, 0, 10, now).is_ok(),
        "capacity refusal does not discard old handles"
    );
    let mut store = ResultStore::new(binding.clone()).unwrap();
    for _ in 0..4 {
        store
            .insert(
                &binding,
                source(),
                Payload::Binary {
                    bytes: vec![7; MAX_RESULT_BYTES],
                    mime: "application/octet-stream".into(),
                },
                now,
            )
            .unwrap();
    }
    assert_eq!(
        store
            .insert(
                &binding,
                source(),
                Payload::Text("one byte too many".into()),
                now
            )
            .unwrap_err(),
        Error::ContextLimit
    );
    assert!(
        store
            .insert(
                &binding,
                source(),
                Payload::Text("fresh after expiry".into()),
                now + TTL
            )
            .is_ok()
    );
    assert_eq!(
        store
            .insert(
                &binding,
                source(),
                Payload::Text("x".repeat(MAX_RESULT_BYTES + 1)),
                now + TTL
            )
            .unwrap_err(),
        Error::ContextLimit
    );
}

#[test]
fn later_chunk_calls_keep_each_original_source_and_bytes_bound_to_its_handle() {
    let binding = binding();
    let now = Instant::now();
    let mut store = ResultStore::new(binding.clone()).unwrap();
    let first_source = source();
    let second_source = Source {
        origin: SourceOrigin::ModelCall {
            model_call: "second-call".into(),
        },
        result_generation: "second-generation".into(),
        ..source()
    };
    let first = store
        .insert(
            &binding,
            first_source.clone(),
            Payload::Text("first result".into()),
            now,
        )
        .unwrap();
    let second = store
        .insert(
            &binding,
            second_source.clone(),
            Payload::Text("second result".into()),
            now,
        )
        .unwrap();
    assert_ne!(first.handle, second.handle);
    for _ in 0..2 {
        let first = store.chunk(&binding, &first.handle, 0, 100, now).unwrap();
        let second = store.chunk(&binding, &second.handle, 0, 100, now).unwrap();
        assert_eq!(first.source, first_source);
        assert_eq!(first.text.as_deref(), Some("first result"));
        assert_eq!(second.source, second_source);
        assert_eq!(second.text.as_deref(), Some("second result"));
    }
}

#[test]
fn many_small_replies_stay_inline_and_do_not_consume_handles() {
    let binding = binding();
    let now = Instant::now();
    let mut store = ResultStore::new(binding.clone()).unwrap();
    for _ in 0..32 {
        let projection = store
            .project_reply(
                &binding,
                source(),
                polaris_mcp::content::Reply {
                    status: polaris_mcp::content::Status::Complete,
                    parts: vec![polaris_mcp::content::Part::Text {
                        text: "small".into(),
                        uri: None,
                        mime: None,
                    }],
                },
                now,
            )
            .unwrap();
        assert!(matches!(
            projection.parts.as_slice(),
            [ProjectedPart::Text { .. }]
        ));
    }
    let binary = store
        .insert(
            &binding,
            source(),
            Payload::Binary {
                bytes: vec![1],
                mime: "application/octet-stream".into(),
            },
            now,
        )
        .unwrap();
    for _ in 1..16 {
        store
            .insert(&binding, source(), Payload::Text(String::new()), now)
            .unwrap();
    }
    assert!(store.chunk(&binding, &binary.handle, 0, 1, now).is_ok());
}

#[test]
fn reply_statuses_remain_distinct_without_retaining_small_input() {
    let binding = binding();
    let now = Instant::now();
    let mut store = ResultStore::new(binding.clone()).unwrap();
    for (status, expected) in [
        (polaris_mcp::content::Status::Complete, "complete"),
        (polaris_mcp::content::Status::ToolError, "tool_error"),
        (
            polaris_mcp::content::Status::InputRequired,
            "input_required",
        ),
        (
            polaris_mcp::content::Status::UnsupportedContent,
            "unsupported_content",
        ),
    ] {
        let projection = store
            .project_reply(
                &binding,
                source(),
                polaris_mcp::content::Reply {
                    status,
                    parts: vec![polaris_mcp::content::Part::Unsupported {
                        kind: "known".into(),
                    }],
                },
                now,
            )
            .unwrap();
        assert_eq!(projection.status, expected);
    }
    for _ in 0..16 {
        store
            .insert(&binding, source(), Payload::Text(String::new()), now)
            .unwrap();
    }
}

#[test]
fn mixed_reply_is_bounded_preserves_variants_and_retains_exact_text_with_provenance() {
    let binding = binding();
    let now = Instant::now();
    let mut store = ResultStore::new(binding.clone()).unwrap();
    let source = Source {
        server: "mixed-server".into(),
        origin: SourceOrigin::ModelCall {
            model_call: "mixed-call".into(),
        },
        result_generation: "mixed-generation".into(),
    };
    let text = "日本語\n\"\\🌟".repeat(8_000);
    let projection = store
        .project_reply(
            &binding,
            source.clone(),
            polaris_mcp::content::Reply {
                status: polaris_mcp::content::Status::ToolError,
                parts: vec![
                    polaris_mcp::content::Part::Text {
                        text: text.clone(),
                        uri: Some("mcp://result".into()),
                        mime: Some("text/plain".into()),
                    },
                    polaris_mcp::content::Part::Structured(serde_json::json!({"count": 3})),
                    polaris_mcp::content::Part::ResourceLink {
                        uri: "https://invalid.example/no-fetch".into(),
                        name: "link".into(),
                        mime: None,
                    },
                    polaris_mcp::content::Part::Binary {
                        bytes: vec![0, 255, 2],
                        mime: "image/png".into(),
                        uri: Some("mcp://image".into()),
                    },
                    polaris_mcp::content::Part::InputRequired(serde_json::json!({"answer": {}})),
                    polaris_mcp::content::Part::Unsupported {
                        kind: "future".into(),
                    },
                ],
            },
            now,
        )
        .unwrap();

    assert_eq!(projection.status, "tool_error");
    assert_eq!(projection.source, source);
    assert!(serde_json::to_vec(&projection).unwrap().len() <= MAX_VISIBLE_BYTES);
    let text_handle = projection
        .parts
        .iter()
        .find_map(|part| match part {
            ProjectedPart::Retained {
                original_kind: "text",
                handle,
                excerpt,
                next_offset,
                truncated,
                ..
            } => Some((handle, excerpt, *next_offset, *truncated)),
            _ => None,
        })
        .unwrap();
    assert!(!text_handle.1.is_empty());
    assert!(text_handle.3);
    assert_eq!(&text[..text_handle.2], text_handle.1.as_str());
    assert!(text.is_char_boundary(text_handle.2));
    let mut chunk = store
        .chunk(&binding, text_handle.0, 0, MAX_VISIBLE_BYTES, now)
        .unwrap();
    let mut rebuilt = String::new();
    loop {
        assert_eq!(chunk.source, source);
        rebuilt.push_str(chunk.text.as_deref().unwrap());
        if !chunk.truncated {
            break;
        }
        chunk = store
            .chunk(
                &binding,
                text_handle.0,
                chunk.next_offset,
                MAX_VISIBLE_BYTES,
                now,
            )
            .unwrap();
    }
    assert_eq!(rebuilt, text);
    assert!(
        projection
            .parts
            .iter()
            .any(|part| matches!(part, ProjectedPart::Structured { .. }))
    );
    assert!(
        projection
            .parts
            .iter()
            .any(|part| matches!(part, ProjectedPart::ResourceLink { .. }))
    );
    let binary = projection
        .parts
        .iter()
        .find_map(|part| match part {
            ProjectedPart::Binary {
                handle,
                mime,
                bytes,
                status,
                ..
            } => Some((handle, mime, *bytes, *status)),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (binary.1.as_str(), binary.2, binary.3),
        ("image/png", 3, "unsupported_modality")
    );
    assert_eq!(
        store.binary_chunk(&binding, binary.0, 0, 3, now).unwrap(),
        &[0, 255, 2]
    );
    let wire = String::from_utf8(serde_json::to_vec(&projection).unwrap()).unwrap();
    assert!(!wire.contains("AP8C"));
    assert!(
        projection
            .parts
            .iter()
            .any(|part| matches!(part, ProjectedPart::InputRequired { .. }))
    );
    assert!(
        projection
            .parts
            .iter()
            .any(|part| matches!(part, ProjectedPart::Unsupported { .. }))
    );
}

#[test]
fn rejected_mixed_reply_leaves_existing_handles_and_creates_no_partial_handles() {
    let binding = binding();
    let now = Instant::now();
    let mut store = ResultStore::new(binding.clone()).unwrap();
    let first = store
        .insert(&binding, source(), Payload::Text("old".into()), now)
        .unwrap();
    for _ in 1..15 {
        store
            .insert(&binding, source(), Payload::Text(String::new()), now)
            .unwrap();
    }
    let reply = || polaris_mcp::content::Reply {
        status: polaris_mcp::content::Status::Complete,
        parts: vec![
            polaris_mcp::content::Part::Binary {
                bytes: vec![1],
                mime: "image/png".into(),
                uri: None,
            },
            polaris_mcp::content::Part::Binary {
                bytes: vec![2],
                mime: "image/png".into(),
                uri: None,
            },
        ],
    };
    assert_eq!(
        store
            .project_reply(&binding, source(), reply(), now)
            .unwrap_err(),
        Error::ContextLimit
    );
    assert_eq!(
        store
            .chunk(&binding, &first.handle, 0, MAX_VISIBLE_BYTES, now)
            .unwrap()
            .text
            .as_deref(),
        Some("old")
    );
    let one_binary = polaris_mcp::content::Reply {
        status: polaris_mcp::content::Status::Complete,
        parts: vec![polaris_mcp::content::Part::Binary {
            bytes: vec![3],
            mime: "image/png".into(),
            uri: None,
        }],
    };
    store
        .project_reply(&binding, source(), one_binary, now)
        .unwrap();
    assert_eq!(
        store
            .project_reply(&binding, source(), reply(), now)
            .unwrap_err(),
        Error::ContextLimit
    );
}

#[test]
fn oversized_or_over_handle_reply_fails_before_retaining_any_part() {
    let binding = binding();
    let now = Instant::now();
    let mut store = ResultStore::new(binding.clone()).unwrap();
    let many_small = polaris_mcp::content::Reply {
        status: polaris_mcp::content::Status::Complete,
        parts: (0..100)
            .map(|_| polaris_mcp::content::Part::Text {
                text: "x".repeat(500),
                uri: None,
                mime: None,
            })
            .collect(),
    };
    assert_eq!(
        store
            .project_reply(&binding, source(), many_small, now)
            .unwrap_err(),
        Error::ContextLimit
    );
    assert!(store.entries.is_empty());

    let too_large = polaris_mcp::content::Reply {
        status: polaris_mcp::content::Status::Complete,
        parts: vec![
            polaris_mcp::content::Part::Text {
                text: "x".repeat(MAX_RESULT_BYTES / 2 + 1),
                uri: None,
                mime: None,
            },
            polaris_mcp::content::Part::Text {
                text: "y".repeat(MAX_RESULT_BYTES / 2 + 1),
                uri: None,
                mime: None,
            },
        ],
    };
    assert_eq!(
        store
            .project_reply(&binding, source(), too_large, now)
            .unwrap_err(),
        Error::ContextLimit
    );
    assert!(store.entries.is_empty());
}

#[test]
fn result_display_uris_drop_queries_and_fragments_without_rewriting_text() {
    let binding = binding();
    let now = Instant::now();
    let mut store = ResultStore::new(binding.clone()).unwrap();
    let projection = store
        .project_reply(
            &binding,
            source(),
            polaris_mcp::content::Reply {
                status: polaris_mcp::content::Status::Complete,
                parts: vec![
                    polaris_mcp::content::Part::Text {
                        text: "literal?keep=this".into(),
                        uri: Some("mcp://text/path?secret=text#fragment".into()),
                        mime: None,
                    },
                    polaris_mcp::content::Part::ResourceLink {
                        uri: "https://example.invalid/resource?secret=link#fragment".into(),
                        name: "x".repeat(MAX_VISIBLE_BYTES),
                        mime: None,
                    },
                    polaris_mcp::content::Part::Binary {
                        bytes: vec![1],
                        mime: "image/png".into(),
                        uri: Some("mcp://binary?secret=binary#fragment".into()),
                    },
                ],
            },
            now,
        )
        .unwrap();
    let wire = String::from_utf8(serde_json::to_vec(&projection).unwrap()).unwrap();
    assert!(wire.contains("literal?keep=this"));
    assert!(!wire.contains("secret="));
    assert!(!wire.contains("#fragment"));
    let link = projection
        .parts
        .iter()
        .find_map(|part| match part {
            ProjectedPart::Retained {
                original_kind: "resource_link",
                handle,
                ..
            } => Some(handle),
            _ => None,
        })
        .unwrap();
    let saved = store
        .chunk(&binding, link, 0, MAX_VISIBLE_BYTES, now)
        .unwrap();
    assert!(!saved.text.unwrap().contains("secret="));
}

#[test]
fn malformed_json_shape_and_unbounded_part_count_reject_before_old_handles_change() {
    let binding = binding();
    let now = Instant::now();
    let mut store = ResultStore::new(binding.clone()).unwrap();
    let old = store
        .insert(&binding, source(), Payload::Text("old".into()), now)
        .unwrap();
    let mut deep = serde_json::Value::Null;
    for _ in 0..=MAX_JSON_DEPTH {
        deep = serde_json::json!([deep]);
    }
    assert_eq!(
        store
            .project_reply(
                &binding,
                source(),
                polaris_mcp::content::Reply {
                    status: polaris_mcp::content::Status::Complete,
                    parts: vec![polaris_mcp::content::Part::Structured(deep)],
                },
                now,
            )
            .unwrap_err(),
        Error::ContextLimit
    );
    assert_eq!(
        store.chunk(&binding, &old.handle, 0, 10, now).unwrap().text,
        Some("old".into())
    );
    assert_eq!(
        store
            .project_reply(
                &binding,
                source(),
                polaris_mcp::content::Reply {
                    status: polaris_mcp::content::Status::Complete,
                    parts: vec![polaris_mcp::content::Part::InputRequired(
                        serde_json::Value::String("x".repeat(MAX_RESULT_BYTES)),
                    )],
                },
                now,
            )
            .unwrap_err(),
        Error::ContextLimit
    );
    assert_eq!(
        store
            .project_reply(
                &binding,
                source(),
                polaris_mcp::content::Reply {
                    status: polaris_mcp::content::Status::UnsupportedContent,
                    parts: (0..=MAX_REPLY_PARTS)
                        .map(|_| polaris_mcp::content::Part::Unsupported {
                            kind: String::new()
                        })
                        .collect(),
                },
                now,
            )
            .unwrap_err(),
        Error::ContextLimit
    );
    assert!(store.chunk(&binding, &old.handle, 0, 10, now).is_ok());
}

#[test]
fn reply_projection_reserves_host_framing_without_losing_escaped_text() {
    let binding = binding();
    let now = Instant::now();
    let raw = "日本語\n\"\\\u{0001}🌟".repeat(1000);
    let reply = || Reply {
        status: Status::Complete,
        parts: vec![Part::Text {
            text: raw.clone(),
            uri: None,
            mime: None,
        }],
    };
    let mut store = ResultStore::new(binding.clone()).unwrap();
    for limit in [0, 1, MAX_VISIBLE_BYTES + 1] {
        assert!(
            store
                .project_reply_bounded(&binding, source(), reply(), now, limit)
                .is_err()
        );
        assert!(store.entries.is_empty());
    }
    let projected = store
        .project_reply_bounded(&binding, source(), reply(), now, 1024)
        .unwrap();
    assert!(serde_json::to_vec(&projected).unwrap().len() <= 1024);
    let ProjectedPart::Retained {
        handle,
        excerpt,
        next_offset,
        truncated,
        ..
    } = &projected.parts[0]
    else {
        panic!("retained result");
    };
    assert!(*truncated && !excerpt.is_empty());
    assert!(raw.starts_with(excerpt));
    assert!(raw.is_char_boundary(*next_offset));
    let mut offset = 0;
    let mut rebuilt = String::new();
    loop {
        let chunk = store
            .chunk(&binding, handle, offset, MAX_VISIBLE_BYTES, now)
            .unwrap();
        rebuilt.push_str(chunk.text.as_deref().unwrap());
        if !chunk.truncated {
            break;
        }
        assert!(chunk.next_offset > offset);
        offset = chunk.next_offset;
    }
    assert_eq!(rebuilt, raw);
}

#[test]
fn host_prompt_source_preserves_selection_in_projection_and_retained_chunks() {
    assert_eq!(
        serde_json::to_string(&source()).unwrap(),
        r#"{"server":"server","model_call":"call","result_generation":"g"}"#,
    );
    let selected = Source {
        origin: SourceOrigin::HostSelection {
            selection_id: "selection-1".into(),
        },
        ..source()
    };
    let binding = binding();
    let now = Instant::now();
    let mut store = ResultStore::new(binding.clone()).unwrap();
    let raw = "原文🌟".repeat(8_000);
    let projection = store
        .project_reply(
            &binding,
            selected.clone(),
            Reply {
                status: Status::Complete,
                parts: vec![Part::Text {
                    text: raw.clone(),
                    uri: None,
                    mime: None,
                }],
            },
            now,
        )
        .unwrap();
    let wire = serde_json::to_value(&projection).unwrap();
    assert_eq!(wire["source"]["selection_id"], "selection-1");
    assert!(wire["source"].get("model_call").is_none());
    assert!(serde_json::to_vec(&projection).unwrap().len() <= MAX_VISIBLE_BYTES);
    let handle = match &projection.parts[0] {
        ProjectedPart::Retained { handle, .. } => handle,
        other => panic!("expected retained prompt text: {other:?}"),
    };
    let mut offset = 0;
    let mut rebuilt = String::new();
    loop {
        let chunk = store
            .chunk(&binding, handle, offset, MAX_VISIBLE_BYTES, now)
            .unwrap();
        assert_eq!(chunk.source, selected);
        let encoded = serde_json::to_value(&chunk).unwrap();
        assert_eq!(encoded["source"]["selection_id"], "selection-1");
        assert!(encoded["source"].get("model_call").is_none());
        rebuilt.push_str(chunk.text.as_deref().unwrap());
        if !chunk.truncated {
            break;
        }
        assert!(chunk.next_offset > offset);
        offset = chunk.next_offset;
    }
    assert_eq!(rebuilt, raw);
}

#[test]
fn host_prompt_invalid_selection_fails_before_result_retention() {
    let binding = binding();
    let now = Instant::now();
    let mut store = ResultStore::new(binding.clone()).unwrap();
    for selection_id in [String::new(), "control\n".into(), "x".repeat(257)] {
        let source = Source {
            origin: SourceOrigin::HostSelection { selection_id },
            ..source()
        };
        assert!(
            store
                .insert(&binding, source.clone(), Payload::Text("raw".into()), now)
                .is_err()
        );
        assert!(
            store
                .project_reply(
                    &binding,
                    source,
                    Reply {
                        status: Status::Complete,
                        parts: vec![Part::Text {
                            text: "raw".into(),
                            uri: None,
                            mime: None
                        }],
                    },
                    now
                )
                .is_err()
        );
        assert!(store.entries.is_empty());
    }
}
