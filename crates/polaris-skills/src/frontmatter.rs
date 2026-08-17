//! SKILL.md のフロントマター解析。仕様が定める制約だけを検証し、独自の制約を足さない。

/// 仕様が `name` に課す上限。
const NAME_MAX: usize = 64;
/// 仕様が `description` に課す上限。
const DESCRIPTION_MAX: usize = 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SkillError {
    #[error("フロントマターが無い")]
    NoFrontmatter,
    #[error("必須フィールド {field} が無い")]
    MissingField { field: &'static str },
    #[error("name {name} が仕様の命名規則に反する")]
    InvalidName { name: String },
    #[error("name {name} が親ディレクトリ名 {dir} と一致しない")]
    NameMismatch { name: String, dir: String },
    #[error("description の長さ {len} が範囲外")]
    InvalidDescription { len: usize },
}

/// 仕様の命名規則。小文字英数字とハイフンのみ、先頭末尾にハイフンなし、連続ハイフンなし、1 から 64 文字。
fn name_is_valid(name: &str) -> bool {
    if name.is_empty() || name.chars().count() > NAME_MAX {
        return false;
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return false;
    }
    name.chars()
        .all(|c| c == '-' || c.is_ascii_digit() || c.is_ascii_lowercase())
}

/// `key: value` の平坦な走査で `name` と `description` を取り出す。
/// 知らないキーは黙って読み飛ばす。仕様が任意フィールドを認めているため、
/// 未知のキーで落ちる実装は仕様準拠を名乗れない。
fn field(front: &str, key: &str) -> Option<String> {
    for line in front.lines() {
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        let v = rest.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|x| x.strip_suffix('"'))
            .unwrap_or(v);
        return Some(v.to_string());
    }
    None
}

/// フロントマターを解析し、`(name, description, body)` を返す。
pub fn parse(text: &str, dir_name: &str) -> Result<(String, String, String), SkillError> {
    let rest = text.strip_prefix("---").ok_or(SkillError::NoFrontmatter)?;
    let rest = rest.trim_start_matches(['\r', '\n']);
    let end = rest.find("\n---").ok_or(SkillError::NoFrontmatter)?;
    let front = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\r', '\n']).to_string();

    let name = field(front, "name").ok_or(SkillError::MissingField { field: "name" })?;
    let description = field(front, "description").ok_or(SkillError::MissingField {
        field: "description",
    })?;

    if !name_is_valid(&name) {
        return Err(SkillError::InvalidName { name });
    }
    if name != dir_name {
        return Err(SkillError::NameMismatch {
            name,
            dir: dir_name.to_string(),
        });
    }
    let len = description.chars().count();
    if len == 0 || len > DESCRIPTION_MAX {
        return Err(SkillError::InvalidDescription { len });
    }

    Ok((name, description, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "---\nname: git-commit\ndescription: コミットを作る。\n---\n\n本文。\n";

    #[test]
    fn parses_name_description_and_body() {
        let (name, desc, body) = parse(GOOD, "git-commit").expect("読めるべき");
        assert_eq!(name, "git-commit");
        assert_eq!(desc, "コミットを作る。");
        assert_eq!(body.trim(), "本文。");
    }

    #[test]
    fn rejects_a_name_that_does_not_match_the_directory() {
        let err = parse(GOOD, "other-dir").expect_err("親ディレクトリ名と不一致は拒否");
        assert!(matches!(err, SkillError::NameMismatch { .. }));
    }

    #[test]
    fn rejects_names_violating_the_specification() {
        for bad in [
            "Git-Commit",
            "-lead",
            "trail-",
            "double--hyphen",
            "under_score",
        ] {
            let text = format!("---\nname: {bad}\ndescription: x\n---\n本文\n");
            assert!(parse(&text, bad).is_err(), "{bad} は拒否されるべき");
        }
    }

    #[test]
    fn rejects_an_empty_or_oversized_description() {
        let empty = "---\nname: a\ndescription: \"\"\n---\n本文\n";
        assert!(parse(empty, "a").is_err(), "空の description は拒否");

        let long = format!(
            "---\nname: a\ndescription: \"{}\"\n---\n本文\n",
            "x".repeat(1025)
        );
        assert!(
            parse(&long, "a").is_err(),
            "1024 文字超の description は拒否"
        );
    }

    #[test]
    fn rejects_a_file_without_frontmatter() {
        assert!(parse("# 見出しだけ\n", "a").is_err());
    }

    #[test]
    fn accepts_optional_fields_without_complaint() {
        let text =
            "---\nname: a\ndescription: x\nlicense: MIT\nallowed-tools: read bash\n---\n本文\n";
        let (name, _, _) = parse(text, "a").expect("任意フィールドは許容される");
        assert_eq!(name, "a");
    }
}
