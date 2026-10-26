
use super::*;

#[test]
fn paste_inserts_without_submitting_and_preserves_secret_redaction() {
    let mut buffer = "before ".to_owned();
    let mut cursor = buffer.len();
    let mut history = input::History::new();
    assert!(!apply_composer_paste(&mut buffer, &mut cursor, &mut history, "日本語\ntext").unwrap());
    assert_eq!(buffer, "before 日本語\ntext");
    assert_eq!(cursor, buffer.len());
    buffer = "/mcp secret set dummy ".into();
    cursor = buffer.len();
    let before = buffer.clone();
    assert!(
        apply_composer_paste(
            &mut buffer,
            &mut cursor,
            &mut history,
            "dummy-only-not-a-credential"
        )
        .is_err()
    );
    assert!(!buffer.contains("dummy-only-not-a-credential"));
    assert_eq!(buffer, before);
    assert_eq!(cursor, buffer.len());
    for (prefix, paste, expected) in [
        ("/mcp sec", "ret set", true),
        ("/mcp sec", "ret set dummy-value", false),
        ("", "/mcp secret set", true),
        ("", "/mcp secret delete reference", true),
    ] {
        assert_eq!(
            composer_paste_is_allowed(prefix, prefix.len(), paste),
            expected
        );
    }
}
