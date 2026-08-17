//! `docs/filemap.md` がリポジトリの実体と一致しているかを確かめるスナップショットテスト。
//!
//! エージェントは探索の代わりにこの文書を読む。文書がずれていれば古い経路へ
//! 確信を持って誘導してしまうため、ずれを検出したら黙って通さず必ず落ちる。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `CARGO_MANIFEST_DIR` は `crates/polaris-core` を指す。リポジトリルートは
/// その2階層上。
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR からリポジトリルートを求められない")
        .to_path_buf()
}

/// 台帳となるファイル一覧を取得する。`git` が無い、またはここが git
/// リポジトリでない場合はテストとして失敗させる。スキップは false pass に
/// なるため許さない。
fn list_files(root: &Path) -> Vec<String> {
    let output = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| {
            panic!("git を実行できない: {e}。git がインストールされているか確認する")
        });

    if !output.status.success() {
        panic!(
            "git ls-files が失敗した（{} が git リポジトリでない可能性がある）: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8(output.stdout).expect("git ls-files の出力が UTF-8 でない");
    let mut files: Vec<String> = stdout
        .lines()
        .map(str::to_string)
        .filter(|p| p.ends_with(".rs") || p.ends_with(".toml") || p.ends_with(".md"))
        .collect();
    files.sort();
    files
}

fn directory_of(path: &str) -> String {
    match path.rfind('/') {
        Some(idx) => path[..idx].to_string(),
        None => ".".to_string(),
    }
}

fn file_name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `.rs` ファイルの要約。ファイル中で最初に現れる、トリム後に非空である
/// `//!` 行の中身を返す。マーカーだけで中身が無い行は「無い」のと同じ扱いで
/// 読み飛ばす（そこで打ち切って空文字列を返すと、`//!` はあるが何も
/// 書いていないファイルが missing-doc チェックをすり抜けてしまう）。
/// 非空の行が1つも無ければ `None`（呼び出し側で失敗として扱う）。
fn rust_summary(root: &Path, path: &str) -> Option<String> {
    let body = std::fs::read_to_string(root.join(path))
        .unwrap_or_else(|e| panic!("{path} を読めない: {e}"));
    body.lines()
        .filter_map(|line| {
            line.trim_start()
                .strip_prefix("//!")
                .map(|rest| rest.trim().to_string())
        })
        .find(|s| !s.is_empty())
}

/// `.md` ファイルの要約。最初の `# ` 見出しの本文。無ければファイル名。
fn markdown_summary(root: &Path, path: &str) -> String {
    let body = std::fs::read_to_string(root.join(path))
        .unwrap_or_else(|e| panic!("{path} を読めない: {e}"));
    body.lines()
        .find_map(|line| line.strip_prefix("# ").map(|rest| rest.trim().to_string()))
        .unwrap_or_else(|| file_name_of(path).to_string())
}

/// `.toml` ファイルの要約。パスから決まる固定ラベル。未知のパターンは
/// ラベルを推測せず失敗させる。
fn toml_summary(path: &str) -> String {
    if path == "Cargo.toml" {
        return "ワークスペース定義と共有依存".to_string();
    }
    if path == "rust-toolchain.toml" {
        return "ツールチェイン固定".to_string();
    }
    if let Some(rest) = path.strip_prefix("crates/")
        && let Some(name) = rest.strip_suffix("/Cargo.toml")
        && !name.contains('/')
    {
        return format!("{name} クレートのマニフェスト");
    }
    panic!("{path} に割り当てるラベルが無い。filemap.rs の toml_summary にパターンを追加する");
}

/// リポジトリの実体から `docs/filemap.md` の期待される本文を組み立てる。
/// `.rs` に `//!` が無いファイルがあれば、文書を組み立てず失敗させる。
fn build_expected(root: &Path) -> String {
    let files = list_files(root);

    let mut missing_doc: Vec<String> = Vec::new();
    let mut grouped: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();

    for path in &files {
        let summary = if path.ends_with(".rs") {
            match rust_summary(root, path) {
                Some(s) => s,
                None => {
                    missing_doc.push(path.clone());
                    continue;
                }
            }
        } else if path.ends_with(".toml") {
            toml_summary(path)
        } else {
            markdown_summary(root, path)
        };

        grouped
            .entry(directory_of(path))
            .or_default()
            .push((file_name_of(path).to_string(), summary));
    }

    assert!(
        missing_doc.is_empty(),
        "{} 個の .rs ファイルに `//!` 行が無い。モジュールの責務を1行で書く:\n{}",
        missing_doc.len(),
        missing_doc
            .iter()
            .map(|p| format!("  - {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let mut out = String::new();
    out.push_str("# ファイルマップ\n\n");
    out.push_str(
        "生成ファイルである。手で編集しない。`crates/polaris-core/tests/filemap.rs` が\n\
         `git ls-files --cached --others --exclude-standard` の結果からリポジトリの実体を\n\
         読み、本文を再構築して `docs/filemap.md` と突き合わせる。ずれていればテストが\n\
         失敗する。更新するときは次を実行する。\n\n\
         ```\n\
         UPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap\n\
         ```\n",
    );

    for (dir, mut entries) in grouped {
        entries.sort();
        out.push_str(&format!("\n## `{dir}`\n\n"));
        for (name, summary) in entries {
            out.push_str(&format!("- `{name}` — {summary}\n"));
        }
    }

    out
}

/// 期待値と実際の内容が食い違った箇所だけを示す。文書全体を2回貼らない。
fn diff_message(expected: &str, actual: &str) -> String {
    let exp: Vec<&str> = expected.lines().collect();
    let act: Vec<&str> = actual.lines().collect();

    let mut start = 0;
    while start < exp.len() && start < act.len() && exp[start] == act[start] {
        start += 1;
    }

    let mut exp_end = exp.len();
    let mut act_end = act.len();
    while exp_end > start && act_end > start && exp[exp_end - 1] == act[act_end - 1] {
        exp_end -= 1;
        act_end -= 1;
    }

    let mut msg = format!(
        "docs/filemap.md がリポジトリの実体とずれている（期待 {} 行 / 実際 {} 行、{} 行目付近から食い違う）\n",
        exp.len(),
        act.len(),
        start + 1
    );
    msg.push_str("--- 期待（再構築した内容）\n");
    for l in &exp[start..exp_end] {
        msg.push_str(&format!("+ {l}\n"));
    }
    msg.push_str("--- 実際（docs/filemap.md の現在の内容）\n");
    for l in &act[start..act_end] {
        msg.push_str(&format!("- {l}\n"));
    }
    msg.push_str("\nUPDATE_FILEMAP=1 cargo test -p polaris-core --test filemap で再生成できる。\n");
    msg
}

#[test]
fn filemap_matches_repository() {
    let root = repo_root();
    let expected = build_expected(&root);
    let doc_path = root.join("docs/filemap.md");

    if std::env::var("UPDATE_FILEMAP").is_ok_and(|v| v != "0" && !v.is_empty()) {
        std::fs::write(&doc_path, &expected).expect("docs/filemap.md を書けない");
        return;
    }

    let actual = std::fs::read_to_string(&doc_path).unwrap_or_default();
    if expected != actual {
        panic!("{}", diff_message(&expected, &actual));
    }
}

#[cfg(test)]
mod rust_summary_tests {
    use super::*;

    #[test]
    fn blank_bang_comment_with_no_other_line_is_none() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::write(dir.path().join("blank.rs"), "//!\n\nfn f() {}\n").expect("書けない");
        assert_eq!(
            rust_summary(dir.path(), "blank.rs"),
            None,
            "中身の無い `//!` 行を要約として拾ってはいけない"
        );
    }

    #[test]
    fn whitespace_only_bang_comment_is_none() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::write(dir.path().join("blank.rs"), "//!   \n\nfn f() {}\n").expect("書けない");
        assert_eq!(
            rust_summary(dir.path(), "blank.rs"),
            None,
            "空白だけの `//!` 行も無いのと同じ扱いにする"
        );
    }

    #[test]
    fn later_non_blank_bang_comment_is_used_when_first_is_blank() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::write(
            dir.path().join("blank_then_real.rs"),
            "//!\n//! 実際の説明。\n\nfn f() {}\n",
        )
        .expect("書けない");
        assert_eq!(
            rust_summary(dir.path(), "blank_then_real.rs"),
            Some("実際の説明。".to_string())
        );
    }

    #[test]
    fn normal_bang_comment_is_unaffected() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::write(dir.path().join("normal.rs"), "//! 普通の説明。\n").expect("書けない");
        assert_eq!(
            rust_summary(dir.path(), "normal.rs"),
            Some("普通の説明。".to_string())
        );
    }
}
