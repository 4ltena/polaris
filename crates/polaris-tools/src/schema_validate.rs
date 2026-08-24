//! subagent の結果を、型が宣言した JSON Schema に照合するだけの薄い
//! ラッパー。バリデータ自体のエラー表現には立ち入らず、モデルへ
//! そのまま返せる1つの文字列へ畳む。
//!
//! ここへ渡されるスキーマは `agents/<type>/references/*.schema.json`、
//! すなわち型の作者が書いたファイルであり、しかも検証はサンドボックス
//! 外の親プロセスで走る。`jsonschema` の既定機能は `$ref` の
//! `https://` / `file://` 解決を有効にするため、既定のままだと
//! 「スキーマを読むこと」自体が外向き通信や任意ファイル読み出しの
//! 副作用を持ちうる。ワークスペースの `Cargo.toml` で
//! `default-features = false` にしてその経路ごと落としてある——同一
//! ドキュメント内の `$ref`(`#/$defs/...`)は既定機能とは無関係に動く
//! ため、同梱スキーマの表現力は何も失われない。

/// `instance` を `schema` へ照合する。不一致なら、再試行時にモデルへ
/// そのまま見せられる1文へ畳んだ理由を返す。`jsonschema` クレート自体の
/// コンパイル済みバリデータの構築失敗(スキーマ自体が不正な JSON Schema
/// である場合)も同じ `Err(String)` として扱う——型定義の作者の誤りと
/// subagent の出力の誤りを、呼び出し側では区別する必要が無いため。
pub fn validate(schema: &serde_json::Value, instance: &serde_json::Value) -> Result<(), String> {
    let compiled = jsonschema::validator_for(schema)
        .map_err(|e| format!("the output schema itself is invalid: {e}"))?;
    let errors: Vec<String> = compiled
        .iter_errors(instance)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_matching_instance_passes() {
        let schema = json!({
            "type": "object",
            "required": ["summary"],
            "properties": { "summary": { "type": "string" } }
        });
        let instance = json!({ "summary": "ok" });
        assert!(validate(&schema, &instance).is_ok());
    }

    #[test]
    fn a_missing_required_field_fails_with_a_readable_message() {
        let schema = json!({
            "type": "object",
            "required": ["summary"],
            "properties": { "summary": { "type": "string" } }
        });
        let instance = json!({});
        let err = validate(&schema, &instance).unwrap_err();
        assert!(err.contains("summary"), "{err}");
    }

    #[test]
    fn an_in_document_ref_still_resolves() {
        // `default-features = false` turns off *external* reference
        // resolution only. A `$ref` pointing within the same document is
        // what the shipped schemas would actually use to factor out a
        // repeated shape, so it has to keep working — otherwise the
        // hardening above would have cost real expressiveness.
        let schema = json!({
            "type": "object",
            "required": ["a", "b"],
            "properties": {
                "a": { "$ref": "#/$defs/nonEmpty" },
                "b": { "$ref": "#/$defs/nonEmpty" }
            },
            "$defs": {
                "nonEmpty": { "type": "string", "minLength": 1 }
            }
        });
        assert!(validate(&schema, &json!({ "a": "x", "b": "y" })).is_ok());
        let err = validate(&schema, &json!({ "a": "x", "b": "" })).unwrap_err();
        assert!(
            err.contains("/b"),
            "the in-document $ref was not applied: {err}"
        );
    }

    #[test]
    fn a_remote_ref_is_refused_instead_of_being_fetched() {
        // The schema is authored by whoever wrote `agents/<type>/`, and
        // validation runs in the unsandboxed parent process. With
        // `jsonschema`'s default features on, building the validator for
        // this schema performs a real outbound request. It must instead
        // fail to compile, and the failure has to come back through the
        // same `Err(String)` any other bad schema does.
        let schema = json!({ "$ref": "https://example.invalid/schema.json" });
        let err = validate(&schema, &json!({})).unwrap_err();
        assert!(
            err.starts_with("the output schema itself is invalid"),
            "a remote $ref was not refused at build time: {err}"
        );
    }

    #[test]
    fn a_file_ref_is_refused_instead_of_being_read() {
        // The `file://` counterpart. Same reasoning: reading an arbitrary
        // local path must not be a side effect of validating a subagent's
        // output against a type-authored schema.
        let schema = json!({ "$ref": "file:///etc/passwd" });
        let err = validate(&schema, &json!({})).unwrap_err();
        assert!(
            err.starts_with("the output schema itself is invalid"),
            "a file:// $ref was not refused at build time: {err}"
        );
    }
}
