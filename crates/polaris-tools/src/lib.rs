//! polaris の組込みツール。常時提供するツールは 6 本を超えない。

pub mod path_policy;
pub mod read;
pub mod skill;

use serde::Serialize;

/// モデルへ渡すツール定義。`parameters` は JSON Schema。
#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("パス {0} は読み取りを許可されていない")]
    PathDenied(String),
    #[error("入出力エラー: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0} は通常ファイルではない")]
    NotAFile(String),
    #[error("{path} は上限 {limit} バイトを超えている（実際 {actual} バイト）")]
    TooLarge {
        path: String,
        limit: u64,
        actual: u64,
    },
}

/// 常時提供するツールの一覧。
pub fn all_specs() -> Vec<ToolSpec> {
    vec![read_spec(), skill_spec()]
}

fn read_spec() -> ToolSpec {
    ToolSpec {
        name: "read",
        description: "ファイルを読む。行番号付きで返す。offset と limit で範囲を指定できる。",
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "offset": { "type": "integer" },
                "limit": { "type": "integer" }
            },
            "required": ["path"]
        }),
    }
}

fn skill_spec() -> ToolSpec {
    ToolSpec {
        name: "skill",
        description: "skill を引く。名前に完全一致すれば本文を返し、そうでなければ候補の名前と説明を返す。",
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "q": {
                    "type": "string",
                    "description": "skill 名（完全一致で本文）、または検索語（名前と説明を照合して候補）。空文字列は全件列挙。"
                }
            },
            "required": ["q"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_spec_serializes_with_required_path() {
        let specs = all_specs();
        let read = specs
            .iter()
            .find(|s| s.name == "read")
            .expect("read が無い");
        let json = serde_json::to_value(read).expect("直列化できない");
        assert_eq!(json["name"], "read");
        assert_eq!(json["parameters"]["required"][0], "path");
        assert_eq!(json["parameters"]["properties"]["path"]["type"], "string");
    }

    #[test]
    fn skill_spec_publishes_the_parameter_name_it_requires() {
        // read 側と対になる公開スキーマの形の固定。ここが見るのは「公開した
        // 引数名とその型が変わっていないこと」だけである。宣言した名前と
        // dispatch が実際に読む名前が同じものを指しているかは、この
        // クレートからは確かめられない（呼ぶ側が別クレートにある）ので、
        // polaris-core 側の
        // `agent::tests::the_skill_tool_reads_the_argument_name_its_schema_declares`
        // が公開スキーマから引数名を取り出して束ねている。
        let specs = all_specs();
        let skill = specs
            .iter()
            .find(|s| s.name == "skill")
            .expect("skill が無い");
        let json = serde_json::to_value(skill).expect("直列化できない");
        assert_eq!(json["name"], "skill");
        assert_eq!(json["parameters"]["required"][0], "q");
        assert_eq!(json["parameters"]["properties"]["q"]["type"], "string");
        // 引数ごとの説明。ツール全体の説明だけでは、名前を渡すと本文が返り
        // それ以外は検索になるという 1 引数 2 モードの規約をモデルが引数の
        // 側から知る手段が無い。説明を消せばここで落ちる。
        let param_doc = json["parameters"]["properties"]["q"]["description"]
            .as_str()
            .expect("引数 q に説明が無い");
        assert!(!param_doc.trim().is_empty(), "引数 q の説明が空");
    }

    #[test]
    fn all_specs_has_unique_names() {
        let specs = all_specs();
        let mut names: Vec<&str> = specs.iter().map(|s| s.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "ツール名が重複している");
    }
}
