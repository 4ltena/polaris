//! SKILL.md のフロントマター解析。仕様が定める制約だけを検証し、独自の制約を足さない。

/// 仕様が `name` に課す上限。
const NAME_MAX: usize = 64;
/// 仕様が `description` に課す上限。
const DESCRIPTION_MAX: usize = 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SkillError {
    #[error("{skill} の SKILL.md にフロントマターが無い")]
    NoFrontmatter { skill: String },
    #[error("{skill} の SKILL.md に必須フィールド {field} が無い")]
    MissingField { skill: String, field: &'static str },
    #[error("name {name} が仕様の命名規則に反する")]
    InvalidName { name: String },
    #[error("name {name} が親ディレクトリ名 {dir} と一致しない")]
    NameMismatch { name: String, dir: String },
    #[error("{skill} の SKILL.md で description の長さ {len} が範囲外")]
    InvalidDescription { skill: String, len: usize },
    #[error("{skill} の SKILL.md の {field} が未対応の構文を含む: {detail}")]
    UnsupportedSyntax {
        skill: String,
        field: &'static str,
        detail: String,
    },
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

/// `start` 以降で、行頭が空白始まりか完全に空である行を連続して集める。
/// インデントの無い行に達したら、そこはトップレベルの次のキーか終端なので
/// そこで止める。
fn collect_indented_block<'a>(lines: &[&'a str], start: usize) -> Vec<&'a str> {
    let mut out = Vec::new();
    if start >= lines.len() {
        return out;
    }
    for line in &lines[start..] {
        if line.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
            out.push(*line);
        } else {
            break;
        }
    }
    out
}

/// ブロックの行から共通の先頭インデントを取り除いて連結する。`fold` が真なら
/// `>`（フォールド）として改行を空白へ、偽なら `|`（リテラル）として改行を
/// 保持する。末尾の空行は落とす。
fn join_block(block_lines: &[&str], fold: bool) -> String {
    let indent = block_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);

    let mut stripped: Vec<String> = block_lines
        .iter()
        .map(|l| {
            let l = l.strip_suffix('\r').unwrap_or(l);
            if l.len() >= indent {
                l[indent..].to_string()
            } else {
                String::new()
            }
        })
        .collect();

    while stripped.last().is_some_and(|s| s.is_empty()) {
        stripped.pop();
    }

    if fold {
        stripped.join(" ")
    } else {
        stripped.join("\n")
    }
}

/// フロントマターの `key: value` 行から値を取り出す。単一行のプレーン値・
/// 引用符付き値に加えて、ブロックスカラー `|`（改行を保持するリテラル）と
/// `>`（改行を空白へ折り畳むフォールド）、および素のプレーン値の複数行継続を
/// 認める。仕様が要求する範囲を超える構文（チョンピング指定や明示インデント
/// 指定を伴うブロックスカラーなど）は確信を持って解釈できないため、値を
/// でっち上げず `SkillError::UnsupportedSyntax` を返す。知らないキーそのもの
/// は黙って読み飛ばす — 仕様が任意フィールドを認めているため。
fn field(lines: &[&str], key: &'static str, skill: &str) -> Result<Option<String>, SkillError> {
    for (i, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        let value_part = rest.trim();

        if value_part == "|" || value_part == ">" {
            let block = collect_indented_block(lines, i + 1);
            return Ok(Some(join_block(&block, value_part == ">")));
        }
        if value_part.starts_with('|') || value_part.starts_with('>') {
            // チョンピング指定（`|-`/`|+`）や明示インデント指定（`|2`）などの
            // ブロックスカラー変種は対応範囲外。値をでっち上げず拒否する。
            return Err(SkillError::UnsupportedSyntax {
                skill: skill.to_string(),
                field: key,
                detail: value_part.to_string(),
            });
        }

        let quoted = value_part
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'));
        if let Some(v) = quoted {
            return Ok(Some(v.to_string()));
        }

        let continuation = collect_indented_block(lines, i + 1);
        if continuation.is_empty() {
            return Ok(Some(value_part.to_string()));
        }
        let folded = join_block(&continuation, true);
        let combined = if folded.is_empty() {
            value_part.to_string()
        } else {
            format!("{value_part} {folded}")
        };
        return Ok(Some(combined));
    }
    Ok(None)
}

/// フロントマターを解析し、`(name, description, body)` を返す。
pub fn parse(text: &str, dir_name: &str) -> Result<(String, String, String), SkillError> {
    let rest = text
        .strip_prefix("---")
        .ok_or_else(|| SkillError::NoFrontmatter {
            skill: dir_name.to_string(),
        })?;
    let rest = rest.trim_start_matches(['\r', '\n']);

    // `rest` が直接 `---` で始まるなら、開始と終了のデリミタが連続する空の
    // フロントマター。そうでなければ次の `\n---` を終端として探す。
    let (front, after_close) = if let Some(after) = rest.strip_prefix("---") {
        ("", after)
    } else {
        let end = rest
            .find("\n---")
            .ok_or_else(|| SkillError::NoFrontmatter {
                skill: dir_name.to_string(),
            })?;
        (&rest[..end], &rest[end + 4..])
    };

    // 終端デリミタが `----` のように 3 本を超えるハイフンで書かれていても、
    // デリミタ行に直に続く余分なハイフンだけを落とす。本文側の内容は必ず
    // 改行を挟んでから始まるため、この trim_start_matches が本文を削ること
    // はない。
    let after_close = after_close.trim_start_matches('-');
    let body = after_close.trim_start_matches(['\r', '\n']).to_string();

    let front_lines: Vec<&str> = front.lines().collect();

    let name = field(&front_lines, "name", dir_name)?.ok_or_else(|| SkillError::MissingField {
        skill: dir_name.to_string(),
        field: "name",
    })?;
    let description =
        field(&front_lines, "description", dir_name)?.ok_or_else(|| SkillError::MissingField {
            skill: dir_name.to_string(),
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
        return Err(SkillError::InvalidDescription {
            skill: dir_name.to_string(),
            len,
        });
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

    #[test]
    fn parses_a_literal_block_scalar_description() {
        let text = "---\nname: a\ndescription: |\n  line one\n  line two\n---\n本文\n";
        let (_, desc, _) = parse(text, "a").expect("| ブロックスカラーは読めるべき");
        assert_eq!(desc, "line one\nline two");
    }

    #[test]
    fn parses_a_folded_block_scalar_description() {
        let text = "---\nname: a\ndescription: >\n  folded\n  text\n---\n本文\n";
        let (_, desc, _) = parse(text, "a").expect("> ブロックスカラーは読めるべき");
        assert_eq!(desc, "folded text");
    }

    #[test]
    fn parses_a_plain_multiline_continuation() {
        let text = "---\nname: a\ndescription: this continues\n  on the next line\n---\n本文\n";
        let (_, desc, _) = parse(text, "a").expect("素の複数行継続は読めるべき");
        assert_eq!(desc, "this continues on the next line");
    }

    #[test]
    fn refuses_a_construct_it_does_not_support_instead_of_guessing() {
        // `|-` はチョンピング指定付きのブロックスカラーで対応範囲外。値を
        // でっち上げず、必ずエラーで知らせる。
        let text = "---\nname: a\ndescription: |-\n  text\n---\n本文\n";
        let err = parse(text, "a").expect_err("未対応の構文は黙って通さず拒否する");
        assert!(matches!(err, SkillError::UnsupportedSyntax { .. }));
    }

    #[test]
    fn frontmatter_errors_name_the_failing_skill() {
        let no_fm = parse("フロントマターが無い\n", "my-skill").expect_err("拒否されるべき");
        assert!(matches!(no_fm, SkillError::NoFrontmatter { ref skill } if skill == "my-skill"));
        assert!(no_fm.to_string().contains("my-skill"));

        let missing = parse("---\ndescription: x\n---\n本文\n", "my-skill")
            .expect_err("name が無ければ拒否されるべき");
        assert!(matches!(
            missing,
            SkillError::MissingField { ref skill, field } if skill == "my-skill" && field == "name"
        ));
        assert!(missing.to_string().contains("my-skill"));

        let bad_len = parse(
            "---\nname: my-skill\ndescription: \"\"\n---\n本文\n",
            "my-skill",
        )
        .expect_err("空の description は拒否されるべき");
        assert!(matches!(
            bad_len,
            SkillError::InvalidDescription { ref skill, .. } if skill == "my-skill"
        ));
        assert!(bad_len.to_string().contains("my-skill"));
    }

    #[test]
    fn empty_frontmatter_reports_missing_field_not_no_frontmatter() {
        let err = parse("---\n---\n本文\n", "a").expect_err("空のフロントマターは拒否されるべき");
        assert!(matches!(err, SkillError::MissingField { .. }));
    }

    #[test]
    fn a_wide_closing_delimiter_does_not_leak_a_hyphen_into_the_body() {
        let text = "---\nname: a\ndescription: x\n----\n本文\n";
        let (_, _, body) = parse(text, "a").expect("幅広の閉じデリミタも読めるべき");
        assert_eq!(body.trim(), "本文");
    }
}
