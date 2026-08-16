//! 毎ターン送るシステムプロンプトを定義し、憲法と環境情報を差し込んで組み立てる。

/// 常時載るシステムプロンプト。振る舞いの指示を削ると往復が増えて総コストが
/// 上がるため、短さのためにここを削らない。削る対象は構造の重複に限る。
pub const SYSTEM_PROMPT: &str = "\
You are polaris, a coding agent. Read files and answer with what the code actually does.

Rules:
- State file paths as path:line so they can be opened directly.
- Never guess file contents. Read them.
- If the same error occurs three times in a row, stop and report it.
- Do not claim work is done without showing the command output that proves it.
- Read docs/filemap.md before searching the tree.
";

/// 常時載る文脈を組み立てる。空の節は見出しごと落とす。
///
/// 組み立て結果はセッションを通して同一でなければならない。ここが毎ターン
/// 変わるとプロンプトキャッシュの接頭辞が動き、履歴全体が未キャッシュ扱いになる。
///
/// `constitution` は呼び出し側で `constitution::load` 等により既に
/// `CONSTITUTION_LIMIT` へ切り詰められている想定だが、ここでも
/// `cap()` を通す。未切り詰めの生テキストを渡す呼び出し側が将来増えても、
/// このガードを経由しない限り上限を破れない。既に切り詰め済みの入力は
/// `cap()` が冪等なため変化しない。
pub fn build_system(constitution: &str, environment: &str) -> String {
    let constitution =
        crate::constitution::cap(constitution, crate::constitution::CONSTITUTION_LIMIT);
    let mut s = String::from(SYSTEM_PROMPT);
    if !constitution.is_empty() {
        s.push_str("\n## Project rules\n");
        s.push_str(&constitution);
        s.push('\n');
    }
    if !environment.is_empty() {
        s.push_str("\n## Environment\n");
        s.push_str(environment);
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constitution::CONSTITUTION_LIMIT;

    #[test]
    fn caps_an_oversized_constitution_passed_directly() {
        // build_system 自身が守りを持つことを確認する。呼び出し側が切り詰めを
        // 忘れた生テキストを渡しても、憲法部分は上限を超えない。
        let oversized = "規則".repeat(5000);
        let system = build_system(&oversized, "");

        let prefix = format!("{SYSTEM_PROMPT}\n## Project rules\n");
        let body = system
            .strip_prefix(&prefix)
            .expect("Project rules 節が組み立てられていない")
            .strip_suffix('\n')
            .expect("末尾の改行が無い");

        let n = crate::budget::count_tokens(body);
        assert!(
            n <= CONSTITUTION_LIMIT,
            "憲法部分が上限を超えている: {n} トークン"
        );
        assert!(!body.is_empty(), "非空の入力から空を返してはいけない");
    }

    #[test]
    fn already_capped_input_is_unchanged() {
        // 既に切り詰め済みの入力は cap() が冪等なため変化しない。
        let already_capped = crate::constitution::cap("固定の規則。", CONSTITUTION_LIMIT);
        let system = build_system(&already_capped, "");
        assert!(system.contains("固定の規則。"));
    }
}
