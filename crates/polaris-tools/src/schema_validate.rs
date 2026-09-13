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

/// 外部 schema 用の、値や upstream 診断を反射しない失敗分類。
#[derive(Debug, Clone, Copy, Eq, PartialEq, thiserror::Error)]
pub enum McpSchemaError {
    #[error("mcp_schema_limit")]
    Limit,
    #[error("mcp_unsupported_schema")]
    Unsupported,
    #[error("mcp_invalid_schema")]
    Invalid,
    #[error("mcp_invalid_arguments ({errors} errors, capped at 8)")]
    Arguments { errors: usize },
}

/// 要求で選ばれた schema だけを compile し、同じ snapshot で再利用する。
/// Compile の同期処理に強制停止可能な時間上限があるとは扱わない。
pub struct McpSchema(jsonschema::Validator);

impl McpSchema {
    pub fn compile(schema: &serde_json::Value) -> Result<Self, McpSchemaError> {
        mcp_json_limit(schema)?;
        let draft = mcp_draft(schema)?;
        mcp_local_references(schema)?;
        jsonschema::options()
            .with_draft(draft)
            .build(schema)
            .map(Self)
            .map_err(|_| McpSchemaError::Invalid)
    }

    pub fn validate(&self, instance: &serde_json::Value) -> Result<(), McpSchemaError> {
        mcp_json_limit(instance)?;
        // instance_path にも利用者の値が入り得るため、件数だけを返す。
        let errors = self.0.iter_errors(instance).take(8).count();
        if errors == 0 {
            Ok(())
        } else {
            Err(McpSchemaError::Arguments { errors })
        }
    }
}

fn mcp_draft(schema: &serde_json::Value) -> Result<jsonschema::Draft, McpSchemaError> {
    match schema.get("$schema") {
        None => Ok(jsonschema::Draft::Draft202012),
        Some(serde_json::Value::String(value)) => match value.as_str() {
            "http://json-schema.org/draft-07/schema#"
            | "http://json-schema.org/draft-07/schema" => Ok(jsonschema::Draft::Draft7),
            "https://json-schema.org/draft/2020-12/schema"
            | "https://json-schema.org/draft/2020-12/schema#" => Ok(jsonschema::Draft::Draft202012),
            _ => Err(McpSchemaError::Unsupported),
        },
        Some(_) => Err(McpSchemaError::Unsupported),
    }
}

fn mcp_json_limit(value: &serde_json::Value) -> Result<(), McpSchemaError> {
    let mut pending = vec![(value, 0usize)];
    let mut nodes = 0;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        if nodes > 16_384 || depth > 64 {
            return Err(McpSchemaError::Limit);
        }
        match value {
            serde_json::Value::Array(values) => {
                if values.len() > 16_384 - nodes {
                    return Err(McpSchemaError::Limit);
                }
                pending.extend(values.iter().map(|value| (value, depth + 1)));
            }
            serde_json::Value::Object(values) => {
                if values.len() > 16_384 - nodes {
                    return Err(McpSchemaError::Limit);
                }
                pending.extend(values.values().map(|value| (value, depth + 1)));
            }
            _ => {}
        }
    }
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > 256 * 1024 {
                return Err(std::io::Error::other("mcp_schema_limit"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter(0), value).map_err(|_| McpSchemaError::Limit)
}

fn mcp_local_references(schema: &serde_json::Value) -> Result<(), McpSchemaError> {
    let mut pending = vec![schema];
    while let Some(value) = pending.pop() {
        let Some(object) = value.as_object() else {
            continue;
        };
        mcp_draft(value)?;
        for key in ["$ref", "$dynamicRef", "$recursiveRef"] {
            if let Some(reference) = object.get(key) {
                let local = reference
                    .as_str()
                    .is_some_and(|reference| reference == "#" || reference.starts_with("#/"));
                if !local {
                    return Err(McpSchemaError::Unsupported);
                }
            }
        }
        // enum/const/examples の data を schema と誤認しない。
        for key in [
            "$defs",
            "definitions",
            "properties",
            "patternProperties",
            "dependentSchemas",
        ] {
            if let Some(values) = object.get(key).and_then(|value| value.as_object()) {
                pending.extend(values.values());
            }
        }
        if let Some(values) = object
            .get("dependencies")
            .and_then(|value| value.as_object())
        {
            pending.extend(values.values().filter(|value| !value.is_array()));
        }
        for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
            if let Some(values) = object.get(key).and_then(|value| value.as_array()) {
                pending.extend(values);
            }
        }
        for key in [
            "items",
            "additionalItems",
            "additionalProperties",
            "unevaluatedItems",
            "unevaluatedProperties",
            "contains",
            "propertyNames",
            "contentSchema",
            "not",
            "if",
            "then",
            "else",
        ] {
            if let Some(value) = object.get(key) {
                if let Some(values) = value.as_array() {
                    pending.extend(values);
                } else {
                    pending.push(value);
                }
            }
        }
    }
    Ok(())
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

    #[test]
    fn mcp_selected_schema_supports_both_drafts_and_recursive_local_pointers() {
        for dialect in [
            None,
            Some("http://json-schema.org/draft-07/schema#"),
            Some("https://json-schema.org/draft/2020-12/schema"),
        ] {
            let mut schema = json!({
                "type":"object", "required":["name"],
                "properties":{"name":{"type":"string"},"next":{"$ref":"#"}},
                "additionalProperties":false
            });
            if let Some(dialect) = dialect {
                schema["$schema"] = json!(dialect);
            }
            let compiled = McpSchema::compile(&schema).unwrap();
            assert_eq!(
                compiled.validate(&json!({"name":"first","next":{"name":"second"}})),
                Ok(())
            );
            assert!(matches!(
                compiled.validate(&json!({"name":"first","next":{}})),
                Err(McpSchemaError::Arguments { .. })
            ));
        }
        // Literal data may itself contain strings named $ref or $schema.
        let value = json!({"$ref":"file:///secret", "$schema":"not a dialect"});
        let compiled = McpSchema::compile(&json!({"const":value})).unwrap();
        assert_eq!(compiled.validate(&value), Ok(()));
    }

    #[test]
    fn mcp_refuses_external_or_unresolved_references_and_unknown_dialects() {
        assert!(matches!(
            McpSchema::compile(
                &json!({"contentSchema":{"$ref":"https://example.invalid/content"}})
            ),
            Err(McpSchemaError::Unsupported)
        ));
        for reference in [
            "file:///secret",
            "https://example.invalid/schema",
            "http://127.0.0.1:9/schema",
            "other.json#/x",
            "#anchor",
        ] {
            assert!(matches!(
                McpSchema::compile(&json!({"properties":{"value":{"$ref":reference}}})),
                Err(McpSchemaError::Unsupported)
            ));
        }
        assert!(matches!(
            McpSchema::compile(&json!({"$ref":"#/$defs/missing"})),
            Err(McpSchemaError::Invalid)
        ));
        for dialect in [
            json!("https://json-schema.org/draft/2019-09/schema"),
            json!(null),
            json!(false),
        ] {
            assert!(matches!(
                McpSchema::compile(&json!({"$schema":dialect})),
                Err(McpSchemaError::Unsupported)
            ));
        }
        assert!(matches!(
            McpSchema::compile(
                &json!({"properties":{"value":{"$schema":"https://example.invalid/meta"}}})
            ),
            Err(McpSchemaError::Unsupported)
        ));
        assert!(matches!(
            McpSchema::compile(&json!({"type":12})),
            Err(McpSchemaError::Invalid)
        ));
    }

    #[test]
    fn mcp_enforces_encoded_bytes_depth_and_nodes_for_schema_and_arguments() {
        let compiled = McpSchema::compile(&json!(true)).unwrap();
        let exact_bytes = json!("x".repeat(256 * 1024 - 2));
        assert_eq!(compiled.validate(&exact_bytes), Ok(()));
        assert!(matches!(
            compiled.validate(&json!("x".repeat(256 * 1024 - 1))),
            Err(McpSchemaError::Limit)
        ));
        assert!(matches!(
            McpSchema::compile(&json!({"description":"x".repeat(256 * 1024)})),
            Err(McpSchemaError::Limit)
        ));
        let nodes = json!(vec![0; 16_383]);
        assert_eq!(compiled.validate(&nodes), Ok(()));
        assert_eq!(
            compiled.validate(&json!(vec![0; 16_384])),
            Err(McpSchemaError::Limit)
        );
        let mut deep = json!(null);
        for _ in 0..64 {
            deep = json!([deep]);
        }
        assert_eq!(compiled.validate(&deep), Ok(()));
        assert_eq!(
            compiled.validate(&json!([deep])),
            Err(McpSchemaError::Limit)
        );
        // Escaping is counted on the actual JSON wire, not the decoded string.
        assert_eq!(
            compiled.validate(&json!("\n".repeat(128 * 1024))),
            Err(McpSchemaError::Limit)
        );
    }

    #[test]
    fn mcp_argument_diagnostics_do_not_reflect_values_or_property_names() {
        let compiled = McpSchema::compile(
            &json!({"type":"object", "additionalProperties":{"type":"integer"}}),
        )
        .unwrap();
        let object: serde_json::Map<String, serde_json::Value> = (0..32)
            .map(|n| (format!("secret-key-{n}"), json!("synthetic-secret-value")))
            .collect();
        let error = compiled.validate(&json!(object)).unwrap_err();
        assert_eq!(error, McpSchemaError::Arguments { errors: 8 });
        assert!(!format!("{error:?} {error}").contains("secret"));
        assert!(error.to_string().len() < 4096);
    }
}
