//! polaris の組込みツール。常時提供するツールは 6 本を超えない。

pub mod path_policy;
pub mod read;

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
}

/// 常時提供するツールの一覧。
pub fn all_specs() -> Vec<ToolSpec> {
    vec![read_spec()]
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
    fn all_specs_has_unique_names() {
        let specs = all_specs();
        let mut names: Vec<&str> = specs.iter().map(|s| s.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "ツール名が重複している");
    }
}
