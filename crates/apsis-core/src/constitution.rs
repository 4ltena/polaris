//! 常時載る文脈のうち、ハーネスが所有しない部分。AGENTS.md の全文は載せない。
//! 上限で切り詰めるため、AGENTS.md がどれだけ大きくても予算は破れない。

use std::path::Path;

use crate::budget::count_tokens;

/// 憲法ブロックに許すトークン数の上限。
pub const CONSTITUTION_LIMIT: usize = 150;

const BEGIN: &str = "<!-- apsis:always-on -->";
const END: &str = "<!-- /apsis:always-on -->";

/// AGENTS.md から常時載せる部分だけを取り出す。
///
/// マーカーで囲まれていればその内側を返す。マーカーが無ければ `## Always on`
/// 見出しの節を次の `## ` の手前まで返す。どちらも無ければ空文字列を返す。
/// 全文を返す経路は存在しない。
pub fn extract_always_on(markdown: &str) -> String {
    if let Some(start) = markdown.find(BEGIN) {
        let after = start + BEGIN.len();
        if let Some(rel) = markdown[after..].find(END) {
            return markdown[after..after + rel].trim().to_string();
        }
    }

    let mut out: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in markdown.lines() {
        if inside {
            if line.starts_with("## ") {
                break;
            }
            out.push(line);
        } else if line.starts_with("## ") && line[3..].trim() == "Always on" {
            inside = true;
        }
    }
    out.join("\n").trim().to_string()
}

/// 上限を超えていたら行単位で切り詰める。全部落とすことはしない。
fn cap(text: &str, limit: usize) -> String {
    if count_tokens(text) <= limit {
        return text.to_string();
    }
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        let mut trial = kept.clone();
        trial.push(line);
        if count_tokens(&trial.join("\n")) > limit {
            break;
        }
        kept.push(line);
    }
    if kept.is_empty() {
        // 1 行目だけで上限を超える場合は、文字単位で落として先頭を残す。
        let mut s: String = text.lines().next().unwrap_or_default().to_string();
        while count_tokens(&s) > limit && !s.is_empty() {
            s.truncate(s.len() - s.chars().last().map_or(0, |c| c.len_utf8()));
        }
        return s;
    }
    kept.join("\n")
}

fn read_block(path: &Path) -> String {
    std::fs::read_to_string(path)
        .map(|b| extract_always_on(&b))
        .unwrap_or_default()
}

/// グローバル規則とプロジェクト規則をこの順に読み、合算して上限で切り詰める。
/// どちらが欠けていても失敗ではない。テストから経路を固定できるよう、
/// グローバル側のパスは引数で受ける。
pub fn load_from(global_agents: Option<&Path>, project_root: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(g) = global_agents {
        let b = read_block(g);
        if !b.is_empty() {
            parts.push(b);
        }
    }
    let b = read_block(&project_root.join("AGENTS.md"));
    if !b.is_empty() {
        parts.push(b);
    }
    cap(&parts.join("\n"), CONSTITUTION_LIMIT)
}

/// `~/.apsis/AGENTS.md` をグローバル規則として解決してから読む。
pub fn load(project_root: &Path) -> String {
    let global = std::env::var_os("HOME").map(|h| Path::new(&h).join(".apsis").join("AGENTS.md"));
    load_from(global.as_deref(), project_root)
}

/// 環境情報。モデルが 1 ターン使って調べる事実を先に渡す。
pub fn environment_block(cwd: &Path, branch: Option<&str>) -> String {
    let mut s = format!("cwd: {}", cwd.display());
    if let Some(b) = branch {
        s.push_str(&format!("\ngit branch: {b}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const MARKED: &str = "\
# AGENTS

前置き。ここは載せない。

<!-- apsis:always-on -->
main へ直接 push しない。
<!-- /apsis:always-on -->

## 詳細
長い手続き。ここも載せない。
";

    const HEADING: &str = "\
# AGENTS

## Always on

main へ直接 push しない。

## Skill routing

長い手続き。ここは載せない。
";

    #[test]
    fn extracts_marked_block_only() {
        let got = extract_always_on(MARKED);
        assert_eq!(got, "main へ直接 push しない。");
    }

    #[test]
    fn falls_back_to_always_on_heading_and_stops_at_next_section() {
        let got = extract_always_on(HEADING);
        assert_eq!(got, "main へ直接 push しない。");
        assert!(!got.contains("Skill routing"));
    }

    #[test]
    fn returns_empty_without_marker_or_heading() {
        assert_eq!(extract_always_on("# AGENTS\n\n本文だけ。\n"), "");
    }

    #[test]
    fn merges_global_then_project_rules() {
        let g = tempfile::tempdir().expect("一時ディレクトリ");
        let pj = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::write(
            g.path().join("AGENTS.md"),
            "## Always on\n\n日本語で応答する。\n",
        )
        .expect("書けない");
        std::fs::write(
            pj.path().join("AGENTS.md"),
            "## Always on\n\nmain へ直接 push しない。\n",
        )
        .expect("書けない");

        let got = load_from(Some(&g.path().join("AGENTS.md")), pj.path());
        assert!(
            got.contains("日本語で応答する。"),
            "グローバル規則が落ちている"
        );
        assert!(
            got.contains("main へ直接 push しない。"),
            "プロジェクト規則が落ちている"
        );
        let gi = got.find("日本語").expect("グローバルが無い");
        let pi = got.find("main へ").expect("プロジェクトが無い");
        assert!(gi < pi, "グローバルが先に来ていない");
    }

    #[test]
    fn caps_oversized_constitution() {
        let mut body = String::from("<!-- apsis:always-on -->\n");
        for i in 0..500 {
            body.push_str(&format!("規則 {i}: 長い行をここに書き連ねる。\n"));
        }
        body.push_str("<!-- /apsis:always-on -->\n");

        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::write(dir.path().join("AGENTS.md"), &body).expect("書けない");

        let got = load(dir.path());
        assert!(
            count_tokens(&got) <= CONSTITUTION_LIMIT,
            "切り詰められていない: {} トークン",
            count_tokens(&got)
        );
        assert!(!got.is_empty(), "全部捨ててはいけない");
    }

    #[test]
    fn returns_empty_when_neither_file_exists() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        assert_eq!(load_from(None, dir.path()), "");
    }

    #[test]
    fn environment_block_carries_cwd_and_branch() {
        let got = environment_block(Path::new("/w/apsis"), Some("feat/x"));
        assert!(got.contains("/w/apsis"));
        assert!(got.contains("feat/x"));
    }

    #[test]
    fn full_always_on_context_stays_within_budget() {
        let constitution = "a".repeat(2000);
        let capped = cap(&constitution, CONSTITUTION_LIMIT);
        let env = environment_block(Path::new("/w/apsis"), Some("feat/m1-headless-loop"));
        let system = crate::prompt::build_system(&capped, &env);

        let n = crate::budget::always_on_tokens(&system, &apsis_tools::all_specs());
        assert!(
            n <= crate::budget::BUDGET_LIMIT,
            "憲法と環境を含めた常時コンテキストが {n} トークン。上限を超えている"
        );
    }
}
