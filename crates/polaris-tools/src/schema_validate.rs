//! subagent の結果を、型が宣言した JSON Schema に照合するだけの薄い
//! ラッパー。バリデータ自体のエラー表現には立ち入らず、モデルへ
//! そのまま返せる1つの文字列へ畳む。

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
}
