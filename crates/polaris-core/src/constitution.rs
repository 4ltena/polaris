//! 常時載る文脈のうち、ハーネスが所有しない部分。AGENTS.md の全文は載せない。
//! 上限で切り詰めるため、AGENTS.md がどれだけ大きくても予算は破れない。

use std::path::Path;

use crate::budget::count_tokens;

/// 憲法ブロックに許すトークン数の上限。
pub const CONSTITUTION_LIMIT: usize = 150;

/// 環境ブロックに許すトークン数の上限。
///
/// `environment_block` は cwd とブランチ名しか積まないが、どちらもディスク
/// /Git の言いなりの文字列で、ハーネスが長さを決めていない。実在する値は
/// この実リポジトリで cwd + branch 合わせて 24 トークン程度だが、上限を
/// 明示しておかないと「skill の数、AGENTS.md の大きさ、環境情報の長さの
/// いずれが増えても、上限を超える経路が存在しない」という約束が、異常に
/// 長い cwd や巨大なブランチ名に対して破れる。実在する cwd・ブランチ名の
/// 実測値（24 トークン）の8倍以上の余裕を持たせつつ、憲法上限
/// （`CONSTITUTION_LIMIT` = 150）と同程度の桁に収め、両方が同時に上限まで
/// 積まれても常時コンテキストの合計が 990 に対して十分な余裕を残す値として
/// 200 を選んだ。両方を同時に飽和させ、さらに skill を 100 件渡したときの
/// 実測は 626 トークンであり（skill は 1 トークンも足さない）、これは
/// `absurdly_long_cwd_cannot_push_the_assembled_system_over_budget`
/// が組み立てる入力そのものである（受け入れ基準 1 の3つの入力を同時に
/// 最大まで積むのはこのテストだけで、
/// `full_always_on_context_stays_within_budget` は環境ブロックが実在の
/// 長さの場合を測る）。この数はツールの本数とスキーマが変われば動く
/// —— ここに書かれていた 525 は skill ツール以前の値のまま更新されて
/// おらず、生きたテストの後ろ盾を持たない数字が腐るという、このファイル
/// 自身の主張どおりのことが起きていた。
pub const ENVIRONMENT_LIMIT: usize = 200;

const BEGIN: &str = "<!-- polaris:always-on -->";
const END: &str = "<!-- /polaris:always-on -->";

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
///
/// `build_system` が構成ミスで未切り詰めの憲法テキストを渡されても安全に
/// なるよう、クレート内から呼べる可視性にしてある。
pub(crate) fn cap(text: &str, limit: usize) -> String {
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
        // 1 行目だけで上限を超える場合は、char 境界を保ったまま二分探索で
        // 収まる最大の接頭辞を探す。
        return cap_by_char_boundary(text.lines().next().unwrap_or_default(), limit);
    }
    kept.join("\n")
}

/// `text` の先頭から、トークン数が `limit` 以下になる最大の接頭辞を返す。
///
/// 文字単位で 1 つずつ削ると、行の長さに比例した回数だけ `count_tokens` を
/// 呼ぶことになり、`count_tokens` の呼び出しコストと無関係に遅い
/// （20KB の1行なら数万回）。`char_indices` が返す文字境界だけを候補にして
/// 二分探索するため、呼び出し回数は候補数の対数に収まる。マルチバイト文字を
/// 跨いで切ることはない。空文字列は必ず `limit` 以下になるため、`lo` は
/// 探索の初期値からループの不変条件として常に条件を満たし続ける。
fn cap_by_char_boundary(text: &str, limit: usize) -> String {
    if count_tokens(text) <= limit {
        return text.to_string();
    }

    let mut boundaries: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    boundaries.push(text.len());

    let mut lo = 0usize;
    let mut hi = boundaries.len() - 1;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if count_tokens(&text[..boundaries[mid]]) <= limit {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    text[..boundaries[lo]].to_string()
}

/// ファイルが無いことと、それ以外の読み取り失敗（権限、不正な UTF-8 など）
/// を区別して返す。「無い」は `Ok(None)` — 通常の状態で、黙って無視して
/// よい。それ以外は `Err` — 呼び出し側がユーザーへ知らせる責任を持つ。
fn try_read_block(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(body) => Ok(Some(extract_always_on(&body))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// ファイルが無ければ静かに空文字列を返す。それ以外の失敗（権限が無い、
/// 文字コードが UTF-8 でない等）は、規則が読めなかったことを stderr へ
/// 警告してから空文字列を返す。どちらも起動は止めない — 憲法が読めない
/// ことは致命的ではないが、黙って「規則が無いことにする」のと、ユーザーへ
/// 知らせた上で「規則を読めなかったので今回は適用しない」のとでは、後者
/// でなければ、ユーザーは自分の規則が一度も効いていないことに気づけない。
fn read_block(path: &Path) -> String {
    match try_read_block(path) {
        Ok(Some(block)) => block,
        Ok(None) => String::new(),
        Err(e) => {
            eprintln!(
                "警告: {} を読めない（{e}）。この規則は今回は適用されない。",
                path.display()
            );
            String::new()
        }
    }
}

/// グローバル規則とプロジェクト規則をこの順に読み、合算して上限で切り詰める。
/// どちらが欠けていても失敗ではない。テストから経路を固定できるよう、
/// グローバル側のパスは引数で受ける。
///
/// `cap()` は接頭辞を残すため、素朴に連結してから丸ごと切り詰めると、より
/// 具体的なプロジェクト側の規則（常に後方）がグローバル側だけで上限に
/// 達したときに丸ごと消える。ソースが両方とも有る場合は、グローバルを
/// `CONSTITUTION_LIMIT / 2` で切り詰めてから、残り予算をプロジェクトへ渡す
/// ことで両方を残す。片方しか無ければ、その1つが上限の全量を使ってよい。
pub fn load_from(global_agents: Option<&Path>, project_root: &Path) -> String {
    let global = global_agents.map(read_block).unwrap_or_default();
    let project = read_block(&project_root.join("AGENTS.md"));

    match (global.is_empty(), project.is_empty()) {
        (true, true) => String::new(),
        (true, false) => cap(&project, CONSTITUTION_LIMIT),
        (false, true) => cap(&global, CONSTITUTION_LIMIT),
        (false, false) => {
            let global_capped = cap(&global, CONSTITUTION_LIMIT / 2);
            let global_tokens = count_tokens(&global_capped);
            // 連結する改行自体のトークン代も引いておく。o200k_base のような
            // 語境界に敏感なトークナイザでは改行が独立したチャンクになるため、
            // 結合後の実測トークン数は各片の実測の単純和と一致する。
            let separator_tokens = count_tokens("\n");
            let project_limit = CONSTITUTION_LIMIT
                .saturating_sub(global_tokens)
                .saturating_sub(separator_tokens);
            let project_capped = cap(&project, project_limit);
            format!("{global_capped}\n{project_capped}")
        }
    }
}

/// `~/.polaris/AGENTS.md` をグローバル規則として解決してから読む。
pub fn load(project_root: &Path) -> String {
    let global = std::env::var_os("HOME").map(|h| Path::new(&h).join(".polaris").join("AGENTS.md"));
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

<!-- polaris:always-on -->
main へ直接 push しない。
<!-- /polaris:always-on -->

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
        let mut body = String::from("<!-- polaris:always-on -->\n");
        for i in 0..500 {
            body.push_str(&format!("規則 {i}: 長い行をここに書き連ねる。\n"));
        }
        body.push_str("<!-- /polaris:always-on -->\n");

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
    fn caps_single_oversized_line_at_a_char_boundary() {
        // 全角文字（UTF-8で3バイト）を並べた1行。行の途中改行が無いため
        // cap() の二分探索フォールバックに直接入る。バイト単位で素朴に
        // 切り詰めれば文字境界を跨いで不正な UTF-8 になる場面を作る。
        let line = "あ".repeat(3000);
        let got = cap(&line, CONSTITUTION_LIMIT);

        assert!(!got.is_empty(), "非空の入力から空文字列を返してはいけない");
        assert!(
            count_tokens(&got) <= CONSTITUTION_LIMIT,
            "切り詰められていない: {} トークン",
            count_tokens(&got)
        );
        // 文字境界を跨いでいれば、この時点で元の行の正しい接頭辞になっておらず、
        // 文字が壊れて見える。有効な char 境界で切れていることの確認を兼ねる。
        assert!(line.starts_with(&got), "元の行の接頭辞になっていない");
        assert!(got.chars().all(|c| c == 'あ'), "文字が壊れている");
    }

    #[test]
    fn both_sources_present_when_global_alone_is_oversized() {
        // グローバル側だけで上限に達するほど巨大でも、より具体的な
        // プロジェクト側の規則を消してはいけない。両方の文字列が残っている
        // ことと、合計が上限内であることを確認する。
        let g = tempfile::tempdir().expect("一時ディレクトリ");
        let pj = tempfile::tempdir().expect("一時ディレクトリ");

        let mut global_body = String::from("<!-- polaris:always-on -->\n");
        for i in 0..500 {
            global_body.push_str(&format!("全域規則 {i}: 長い行をここに書き連ねる。\n"));
        }
        global_body.push_str("<!-- /polaris:always-on -->\n");
        std::fs::write(g.path().join("AGENTS.md"), &global_body).expect("書けない");

        std::fs::write(
            pj.path().join("AGENTS.md"),
            "## Always on\n\nmain へ直接 push しない。\n",
        )
        .expect("書けない");

        let got = load_from(Some(&g.path().join("AGENTS.md")), pj.path());
        assert!(
            got.contains("main へ直接 push しない。"),
            "プロジェクト規則が消えている: {got}"
        );
        assert!(
            got.contains("全域規則 0"),
            "グローバル規則が残っていない: {got}"
        );
        let n = count_tokens(&got);
        assert!(n <= CONSTITUTION_LIMIT, "上限を超えている: {n}");
    }

    #[test]
    fn single_source_uses_the_full_limit() {
        // グローバル側が無い場合、プロジェクト側だけで CONSTITUTION_LIMIT の
        // 全量を使ってよい。半分に予約されていないことを、上限の半分を
        // 超える量が実際に残ることで確認する。
        let mut body = String::from("<!-- polaris:always-on -->\n");
        for i in 0..500 {
            body.push_str(&format!("規則 {i}: 長い行をここに書き連ねる。\n"));
        }
        body.push_str("<!-- /polaris:always-on -->\n");

        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::write(dir.path().join("AGENTS.md"), &body).expect("書けない");

        let got = load_from(None, dir.path());
        let n = count_tokens(&got);
        assert!(n <= CONSTITUTION_LIMIT, "上限を超えている: {n}");
        assert!(
            n > CONSTITUTION_LIMIT / 2,
            "単独ソースなのに半分しか使えていない: {n}"
        );
    }

    #[test]
    fn try_read_block_returns_none_for_a_missing_file() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let missing = dir.path().join("AGENTS.md");
        assert_eq!(
            try_read_block(&missing).expect("エラーになってはいけない"),
            None
        );
    }

    #[test]
    fn try_read_block_returns_err_for_invalid_utf8() {
        // 非UTF-8（例: Shift-JIS）で保存された AGENTS.md は「無い」のと
        // 同じに潰してはいけない。ユーザーの規則が読めなかったことを
        // 呼び出し側が知れるように、Err として区別する。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, [0x82, 0xa0, 0x82, 0xa2]).expect("書けない"); // Shift-JIS の一部
        let err = try_read_block(&path).expect_err("不正なUTF-8はエラーになるべき");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    #[cfg(unix)]
    fn try_read_block_returns_err_for_permission_denied() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "## Always on\n\n読めないはず\n").expect("書けない");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("権限を変更できない");

        // このテストの前提（このプロセスが実際にパーミッションへ従う環境で
        // 動いている）を、`try_read_block` を経由しない生の読み取りで独立に
        // 確かめる。`result` の判定をこの ground truth に委ねてしまうと、
        // `try_read_block` が常に `Ok` を返す退行が起きても「root 環境
        // だったから」で握り潰され、テストが規制の有無を問わず通ってしまう。
        let ground_truth = std::fs::read_to_string(&path);
        let result = try_read_block(&path);

        // 後片付け: tempdir の Drop が削除できるよう権限を戻す。
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("権限を戻せない");

        match ground_truth {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let err = result.expect_err("権限が無いのでエラーになるべき");
                assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
            }
            // root で実行している等、パーミッションが強制されない環境では
            // このテストの前提が成立しないため、判定をスキップする。
            _ => eprintln!(
                "この環境ではファイル権限が強制されない（root 実行?）。判定をスキップする。"
            ),
        }
    }

    #[test]
    fn returns_empty_when_neither_file_exists() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        assert_eq!(load_from(None, dir.path()), "");
    }

    #[test]
    fn environment_block_carries_cwd_and_branch() {
        let got = environment_block(Path::new("/w/polaris"), Some("feat/x"));
        assert!(got.contains("/w/polaris"));
        assert!(got.contains("feat/x"));
    }

    /// 予算のテストが渡す skill 一式。名前も説明も本文も、常時コンテキストへ
    /// 漏れたら目に見えるだけの長さを持たせてある。ディスクを経由しないのは、
    /// ここで見たいのが「発見できるか」ではなく「積まれないか」だから。
    fn probe_skills(n: usize) -> Vec<polaris_skills::Skill> {
        (0..n)
            .map(|i| polaris_skills::Skill {
                name: format!("budget-probe-{i:03}"),
                description: format!("常時コンテキストへ漏れていないかを見る目印 {i:03}。"),
                body: format!("本文 {i}"),
                path: format!("/x/budget-probe-{i:03}/SKILL.md").into(),
            })
            .collect()
    }

    #[test]
    fn full_always_on_context_stays_within_budget() {
        let constitution = "a".repeat(2000);
        let capped = cap(&constitution, CONSTITUTION_LIMIT);
        let env = environment_block(Path::new("/w/polaris"), Some("feat/m1-headless-loop"));
        let always_on = crate::prompt::assemble_always_on(&capped, &env, &probe_skills(100));

        let n = always_on.tokens();
        assert!(
            n <= crate::budget::BUDGET_LIMIT,
            "憲法と環境を含めた常時コンテキストが {n} トークン。上限を超えている"
        );
    }

    #[test]
    fn absurdly_long_cwd_cannot_push_the_assembled_system_over_budget() {
        // レビューの再現条件（憲法を上限まで埋め、加えて異常に長い cwd を
        // 与えると 990 を超える）を、深くネストした作業ディレクトリで
        // 再現する。単純な同一文字の繰り返しは BPE でごく少ないトークンに
        // 圧縮されてしまい上限を超えないため、実在のパスに近い「多数の
        // ディレクトリ階層」の形を使う。この長さ（7,690 バイト）は macOS の
        // `PATH_MAX` を大きく超えており実在しえないが、`build_system` の
        // 約束は「実在しうる入力でしか壊れない」であってはならない —
        // 環境ブロックが `cap()` を経由しない限り、この入力だけで
        // アサーション対象の合計が 990 を超えていた（キャップ無しで実測
        // 1,538 トークン）。
        //
        // 受け入れ基準 1 が挙げる3つの入力（skill の数、AGENTS.md の大きさ、
        // 環境情報の長さ）を同時に最大まで積む唯一のテストなので、skill も
        // 100 件渡す。ここが真の同時最悪ケースであり、報告する天井の数字は
        // このテストが出す。
        let huge_cwd: String = (0..600).map(|i| format!("/component{i}")).collect();
        let oversized_constitution = "a".repeat(2000);
        let capped_constitution = cap(&oversized_constitution, CONSTITUTION_LIMIT);
        let env = environment_block(Path::new(&huge_cwd), Some("feat/m1-headless-loop"));
        let always_on =
            crate::prompt::assemble_always_on(&capped_constitution, &env, &probe_skills(100));

        let n = always_on.tokens();
        assert!(
            n <= crate::budget::BUDGET_LIMIT,
            "巨大な cwd を含めても常時コンテキストは {n} トークン。上限を超えている"
        );
    }
}
