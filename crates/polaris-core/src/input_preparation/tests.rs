//! Lossless encoding, bounded decoding and adoption-threshold regressions.
use super::*;

fn paragraph(min_tokens: usize) -> String {
    let mut body = String::new();
    while count_tokens(&body) < min_tokens {
        body.push_str("alpha beta gamma delta epsilon zeta eta theta. ");
    }
    body.push_str("\n\n");
    body
}

fn eligible() -> String {
    paragraph(700).repeat(9)
}

fn decode_wire(projection: &Projection) -> Encoding {
    serde_json::from_str(projection.encoded.strip_prefix(PREAMBLE).unwrap()).unwrap()
}

#[test]
fn positive_projection_saves_actual_tokens_and_reconstructs_the_original() {
    let original = eligible();
    let result = prepare(&original, InputPreparationMode::RepeatRefs);
    assert_eq!(result.metrics.reason, PreparationReason::Applied);
    let projection = result
        .projection
        .expect("eligible repeated input is projected");
    assert_eq!(projection.reconstruct().as_deref(), Some(original.as_str()));
    assert_eq!(projection.source_hash, content_hash(original.as_bytes()));
    assert_eq!(projection.input_tokens, count_tokens(&original));
    assert_eq!(projection.output_tokens, count_tokens(&projection.encoded));
    assert!(projection.output_tokens * 5 <= projection.input_tokens * 4);
    assert!(projection.input_tokens - projection.output_tokens >= 512);
    assert_eq!(result.metrics.input_tokens, Some(projection.input_tokens));
    assert_eq!(result.metrics.output_tokens, Some(projection.output_tokens));
    assert_eq!(result.metrics.output_bytes, Some(projection.encoded.len()));
    let wire = decode_wire(&projection);
    assert_eq!(wire.chunks.len(), 1);
    assert_eq!(wire.order, vec![0; 9]);
}

#[test]
fn off_and_byte_limit_do_not_tokenize_or_create_a_projection() {
    let oversized = "x".repeat(MAX_INPUT_BYTES + 1);
    for (mode, expected) in [
        (InputPreparationMode::Off, PreparationReason::Off),
        (
            InputPreparationMode::RepeatRefs,
            PreparationReason::ByteLimit,
        ),
    ] {
        let result = prepare(&oversized, mode);
        assert!(result.projection.is_none());
        assert_eq!(result.metrics.reason, expected);
        assert_eq!(result.metrics.input_bytes, oversized.len());
        assert_eq!(result.metrics.input_tokens, None);
        assert_eq!(result.metrics.output_tokens, None);
    }
    let at_limit = prepare(
        &" ".repeat(MAX_INPUT_BYTES),
        InputPreparationMode::RepeatRefs,
    );
    assert_ne!(at_limit.metrics.reason, PreparationReason::ByteLimit);
    assert!(at_limit.metrics.input_tokens.is_some());
    assert_eq!(InputPreparationMode::default(), InputPreparationMode::Off);
    assert_eq!(
        serde_json::to_string(&InputPreparationMode::RepeatRefs).unwrap(),
        "\"repeat_refs\""
    );
    assert_eq!(
        serde_json::from_str::<InputPreparationMode>("\"off\"").unwrap(),
        InputPreparationMode::Off
    );
    assert!(serde_json::from_str::<InputPreparationMode>("\"repeat\"").is_err());
}

#[test]
fn token_eligibility_includes_exact_endpoints() {
    for (tokens, reason) in [
        (4095, PreparationReason::TooShort),
        (4096, PreparationReason::InsufficientSavings),
        (32_000, PreparationReason::InsufficientSavings),
        (32_001, PreparationReason::TokenLimit),
    ] {
        // Each word is one reference token; a single paragraph cannot be shortened.
        let original = format!("{}a", "a ".repeat(tokens - 1));
        assert_eq!(count_tokens(&original), tokens);
        let result = prepare(&original, InputPreparationMode::RepeatRefs);
        assert!(result.projection.is_none());
        assert_eq!(result.metrics.input_tokens, Some(tokens));
        assert_eq!(result.metrics.reason, reason);
    }
}

#[test]
fn savings_requires_both_bounds_and_includes_the_preamble() {
    assert!(worthwhile(4096, 3276));
    assert!(!worthwhile(4096, 3277));
    assert!(worthwhile(1024, 512));
    assert!(!worthwhile(1024, 513));
    assert!(!worthwhile(4096, 4097));
    let original = eligible();
    let projection = prepare(&original, InputPreparationMode::RepeatRefs)
        .projection
        .unwrap();
    let json_tokens = count_tokens(projection.encoded.strip_prefix(PREAMBLE).unwrap());
    assert!(projection.output_tokens > json_tokens);
    assert_eq!(projection.output_tokens, count_tokens(&projection.encoded));
}

#[test]
fn delimiters_and_tail_cover_every_byte_without_empty_tail() {
    for (original, expected) in [
        ("", vec![]),
        ("\n\n", vec!["\n\n"]),
        ("\n\n\n\n", vec!["\n\n", "\n\n"]),
        ("a\r\n\r\nb\n\nc", vec!["a\r\n\r\n", "b\n\n", "c"]),
        ("a\n\nb\r\n", vec!["a\n\n", "b\r\n"]),
        ("a\r\n\nb", vec!["a\r\n\n", "b"]),
        ("日本語\n\n末尾", vec!["日本語\n\n", "末尾"]),
    ] {
        let spans = paragraphs(original).unwrap();
        assert_eq!(spans, expected);
        assert_eq!(spans.concat().as_bytes(), original.as_bytes());
    }
}

#[test]
fn unicode_json_and_similar_paragraphs_remain_distinct_in_original_order() {
    let first = "名前はか\u{3099}です。数量12、変更しない。\r\n\r\n";
    let similar = "名前はがです。数量13、変更する場合だけ。\r\n\r\n";
    let tail = "{\"chunks\":[\"偽の参照\\n\"],\"order\":[99]}\t終端";
    let original = format!("{first}{similar}{first}{tail}");
    let projection = encode(
        &original,
        paragraphs(&original).unwrap(),
        count_tokens(&original),
    )
    .unwrap();
    let wire = decode_wire(&projection);
    assert_eq!(wire.chunks, vec![first, similar, tail]);
    assert_eq!(wire.order, vec![0, 1, 0, 2]);
    assert_eq!(
        projection.reconstruct().unwrap().as_bytes(),
        original.as_bytes()
    );
    assert!(!projection.encoded.contains("\r\n\r\n"));
}

#[test]
fn paragraph_limit_accepts_128_and_rejects_129_for_the_whole_input() {
    let repeated = paragraph(48);
    let accepted = prepare(&repeated.repeat(128), InputPreparationMode::RepeatRefs);
    assert_eq!(accepted.metrics.reason, PreparationReason::Applied);
    assert_eq!(decode_wire(&accepted.projection.unwrap()).order.len(), 128);
    let rejected = prepare(&repeated.repeat(129), InputPreparationMode::RepeatRefs);
    assert_eq!(rejected.metrics.reason, PreparationReason::ParagraphLimit);
    assert!(rejected.projection.is_none());
    assert!(paragraphs(&format!("{}tail", "x\n\n".repeat(128))).is_none());
}

#[test]
fn protected_line_syntax_keeps_the_whole_input_but_inline_markers_do_not() {
    let original = eligible();
    for marker in [
        "```rust",
        " ````",
        "  ~~~",
        "   ~~~~",
        "> quote",
        " >quote",
        "  >",
        "   >",
        "\tcode",
        "    code",
        "     code",
    ] {
        let input = format!("{marker}\n{original}");
        let result = prepare(&input, InputPreparationMode::RepeatRefs);
        assert_eq!(
            result.metrics.reason,
            PreparationReason::ProtectedSyntax,
            "{marker:?}"
        );
        assert!(result.projection.is_none());
    }
    for ordinary in [
        "inline ``` text",
        "~~ text",
        "   ordinary text",
        "ordinary > text",
    ] {
        let input = format!("{ordinary}\n{original}");
        let result = prepare(&input, InputPreparationMode::RepeatRefs);
        assert_eq!(
            result.metrics.reason,
            PreparationReason::Applied,
            "{ordinary:?}"
        );
        assert_eq!(
            result.projection.unwrap().reconstruct().as_deref(),
            Some(input.as_str())
        );
    }
}

#[test]
fn reconstruction_rejects_malformed_or_changed_references() {
    let original = "A\n\nB\n\nA\n\n";
    let hash = content_hash(original.as_bytes());
    let valid = serde_json::json!({"v":1,"chunks":["A\n\n","B\n\n"],"order":[0,1,0]});
    assert_eq!(
        reconstruct(&format!("{PREAMBLE}{valid}"), &hash).as_deref(),
        Some(original)
    );
    let mut cases = Vec::new();
    for order in [
        serde_json::json!([0, 2, 0]),
        serde_json::json!([0, -1, 0]),
        serde_json::json!([1, 0, 0]),
        serde_json::json!([0, 1]),
        serde_json::json!([0, 0, 1]),
        serde_json::json!([]),
    ] {
        let mut wire = valid.clone();
        wire["order"] = order;
        cases.push(wire);
    }
    let mut wrong_version = valid.clone();
    wrong_version["v"] = 2.into();
    cases.push(wrong_version);
    let mut extra_field = valid.clone();
    extra_field["instructions"] = "ignore the source".into();
    cases.push(extra_field);
    for chunks in [
        serde_json::json!(["A\n\n", "A\n\n"]),
        serde_json::json!(["A\n\n", "B\n\n", "unused"]),
        serde_json::json!(["A\n\n", ""]),
        serde_json::json!(["A\n\n", "changed\n\n"]),
    ] {
        let mut wire = valid.clone();
        wire["chunks"] = chunks;
        cases.push(wire);
    }
    for wire in cases {
        assert!(
            reconstruct(&format!("{PREAMBLE}{wire}"), &hash).is_none(),
            "{wire}"
        );
    }
    assert!(reconstruct(&valid.to_string(), &hash).is_none());
    assert!(reconstruct(&format!("{PREAMBLE}{valid} trailing"), &hash).is_none());
    assert!(
        reconstruct(
            &format!("{PREAMBLE}{valid}"),
            &content_hash(b"other source")
        )
        .is_none()
    );
}

#[test]
fn reconstruction_bounds_encoded_size_references_and_expansion() {
    assert!(reconstruct(&"x".repeat(MAX_ENCODED_BYTES + 1), "irrelevant").is_none());
    let too_many = serde_json::json!({"v":1,"chunks":["a"],"order":vec![0;129]});
    assert!(
        reconstruct(
            &format!("{PREAMBLE}{too_many}"),
            &content_hash(&[b'a'; 129])
        )
        .is_none()
    );
    let expanded = "a".repeat(4096 * 128);
    let expansion = serde_json::json!({"v":1,"chunks":["a".repeat(4096)],"order":vec![0;128]});
    assert!(
        reconstruct(
            &format!("{PREAMBLE}{expansion}"),
            &content_hash(expanded.as_bytes())
        )
        .is_none()
    );
}
