/// 常時載るシステムプロンプト。振る舞いの指示を削ると往復が増えて総コストが
/// 上がるため、短さのためにここを削らない。削る対象は構造の重複に限る。
pub const SYSTEM_PROMPT: &str = "\
You are apsis, a coding agent. Read files and answer with what the code actually does.

Rules:
- State file paths as path:line so they can be opened directly.
- Never guess file contents. Read them.
- If the same error occurs three times in a row, stop and report it.
- Do not claim work is done without showing the command output that proves it.
";

/// 常時載る文脈を組み立てる。空の節は見出しごと落とす。
///
/// 組み立て結果はセッションを通して同一でなければならない。ここが毎ターン
/// 変わるとプロンプトキャッシュの接頭辞が動き、履歴全体が未キャッシュ扱いになる。
pub fn build_system(constitution: &str, environment: &str) -> String {
    let mut s = String::from(SYSTEM_PROMPT);
    if !constitution.is_empty() {
        s.push_str("\n## Project rules\n");
        s.push_str(constitution);
        s.push('\n');
    }
    if !environment.is_empty() {
        s.push_str("\n## Environment\n");
        s.push_str(environment);
        s.push('\n');
    }
    s
}
