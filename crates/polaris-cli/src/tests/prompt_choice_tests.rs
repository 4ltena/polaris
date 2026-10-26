
use super::mcp_prompt_choice;

#[test]
fn a_choice_names_one_server_and_one_prompt() {
    let held = mcp_prompt_choice("files/review", &["path=src/lib.rs".into(), "深さ=2".into()])
        .expect("a well-formed choice");
    assert_eq!(held.server, "files");
    assert_eq!(held.prompt, "review");
    assert_eq!(held.arguments["path"], "src/lib.rs");
    // Values stay strings. Parsing them as JSON would make an argument
    // change type depending on what it looked like, and the server
    // would be told something other than what was typed.
    assert_eq!(held.arguments["深さ"], "2");
    assert!(held.arguments["深さ"].is_string());
}

#[test]
fn a_malformed_choice_is_refused_before_anything_connects() {
    for refused in ["files", "/review", "files/", ""] {
        assert!(
            mcp_prompt_choice(refused, &[]).is_err(),
            "{refused:?} does not name a server and a prompt"
        );
    }
    for refused in ["nokey", "=value"] {
        assert!(
            mcp_prompt_choice("files/review", &[refused.into()]).is_err(),
            "{refused:?} is not key=value"
        );
    }
    // A repeated key would silently keep one of the two values.
    assert!(
        mcp_prompt_choice("files/review", &["a=1".into(), "a=2".into()]).is_err(),
        "a duplicate key has no obvious winner"
    );
    // A value may contain `=`; only the first one separates.
    let held = mcp_prompt_choice("files/review", &["q=a=b".into()]).expect("one separator");
    assert_eq!(held.arguments["q"], "a=b");
}
