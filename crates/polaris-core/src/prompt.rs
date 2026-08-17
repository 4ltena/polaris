//! 毎ターン載るもの一式を組み立てる唯一の場所。
//!
//! システムプロンプトへ憲法と環境情報を差し込み、送るツール定義と束ねて
//! `AlwaysOn` にする。
//!
//! 常時コンテキストが 990 トークンを超えないこと、そして skill が何件あっても
//! 増えないことは、この設計の中心的な主張である。主張を測れるようにするには、
//! 本番が送るものとテストが測るものが同じでなければならない。かつては
//! `main.rs` が組み立て、テストは同じ手順を書き写して別に組み立てていた。
//! 2つは黙って食い違えるので、`main.rs` へ skill のカタログを1行足す変更は
//! どのテストからも届かなかった（再レビューの mutation N7）。組み立てを
//! ここへ1本にまとめ、`main.rs` もテストも同じ `assemble_always_on` を呼ぶ。

use polaris_tools::ToolSpec;

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
///
/// `environment` も同じ理由・同じ仕組みで `ENVIRONMENT_LIMIT` へ切り詰める。
/// cwd もブランチ名もディスク/Git の言いなりの長さで、呼び出し側
/// （`constitution::environment_block`）はそれ自体で長さを制限していない
/// ため、ここで無条件にキャップしないと常時コンテキストの上限は異常に
/// 長い cwd 1つで破れる。呼び出し側を経由しない限り抜け道が無いよう、
/// 憲法と同じ「ここで二重にキャップする」設計に揃えた。
pub(crate) fn build_system(constitution: &str, environment: &str) -> String {
    let constitution =
        crate::constitution::cap(constitution, crate::constitution::CONSTITUTION_LIMIT);
    let environment = crate::constitution::cap(environment, crate::constitution::ENVIRONMENT_LIMIT);
    let mut s = String::from(SYSTEM_PROMPT);
    if !constitution.is_empty() {
        s.push_str("\n## Project rules\n");
        s.push_str(&constitution);
        s.push('\n');
    }
    if !environment.is_empty() {
        s.push_str("\n## Environment\n");
        s.push_str(&environment);
        s.push('\n');
    }
    s
}

/// 毎ターン必ず送るもの一式。システムプロンプトと、そのターンで公開する
/// ツール定義を1つにまとめて持つ。
///
/// フィールドは非公開で、変更する手段も公開していない。組み立てられるのは
/// [`assemble_always_on`] からだけである。これは行儀の問題ではなく、この
/// 型が守っている不変条件そのものによる —— 呼び出し側が組み立て後の
/// システムプロンプトへ何かを継ぎ足せるなら、`polaris-core` の外に
/// 「常時コンテキストを増やせる経路」が残り、受け入れ基準 1 の
/// 「上限を超える経路が存在しない」が `main.rs` の書き方次第になる。
/// 継ぎ足したい変更は [`assemble_always_on`] の内側を触るほかなく、
/// その内側は `budget::tests::the_always_on_total_does_not_move_as_the_number_of_skills_grows`
/// が 0 件 / 15 件 / 100 件の skill で測っている。
#[derive(Debug, Clone)]
pub struct AlwaysOn {
    system: String,
    tools: Vec<ToolSpec>,
    skills_seen: usize,
}

impl AlwaysOn {
    /// 送るシステムプロンプト。
    pub fn system(&self) -> &str {
        &self.system
    }

    /// 送るツール定義。
    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    /// この一式の実測トークン数。予算のテストはすべてこの経路で測る。
    pub fn tokens(&self) -> usize {
        crate::budget::always_on_tokens(&self.system, &self.tools)
    }

    /// 組み立て時に渡された skill の件数。
    ///
    /// 常時コンテキストには一切載らない（載っていないことがこの milestone の
    /// 主張である）。それでも数だけを控えるのは、「skill を 100 件渡しても
    /// 合計が動かない」というテストが、実は 0 件しか渡せていなかった、という
    /// 空振りを検出できるようにするためである。B3 の元の欠陥がまさに
    /// 「渡したはずのものが計算に入っていない」だったので、渡ったことを
    /// 組み立てた側から言えるようにしておく。
    pub fn skills_seen(&self) -> usize {
        self.skills_seen
    }
}

/// 常時コンテキストを組み立てる唯一の関数。`main.rs` もテストもこれを呼ぶ。
///
/// `skills` を受け取って何も載せない。これがこの関数の主張である —— ルータ
/// （`skill` ツール）が発見を担うので、skill の名前も説明も常時コンテキストへ
/// 出す必要が無く、出さない限り合計は skill の件数に依存しない。引数として
/// 受け取るのは、載せないことをテストから測れるようにするためである。件数の
/// 違う3つの入力を同じ関数へ通し、出てきたトークン数が一致することを見る。
/// 引数を取らない関数では、同じ式を3回評価して自分自身と比べるだけの
/// 恒真式にしかならない（B3 の元の欠陥）。
pub fn assemble_always_on(
    constitution: &str,
    environment: &str,
    skills: &[polaris_skills::Skill],
) -> AlwaysOn {
    AlwaysOn {
        system: build_system(constitution, environment),
        tools: polaris_tools::all_specs(),
        skills_seen: skills.len(),
    }
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
    fn skills_seen_reports_what_was_handed_in() {
        // `AlwaysOn::skills_seen` は予算テストが「本当に 100 件渡っているか」を
        // 確かめるための唯一の手がかりなので、それ自体が入力を反映している
        // ことを固定する。常に 0 を返す実装ならここで落ちる。
        let skills: Vec<polaris_skills::Skill> = (0..3)
            .map(|i| polaris_skills::Skill {
                name: format!("s{i}"),
                description: "説明".into(),
                body: "本文".into(),
                path: format!("/x/s{i}/SKILL.md").into(),
            })
            .collect();

        assert_eq!(assemble_always_on("", "", &[]).skills_seen(), 0);
        assert_eq!(assemble_always_on("", "", &skills).skills_seen(), 3);
    }

    #[test]
    fn already_capped_input_is_unchanged() {
        // 既に切り詰め済みの入力は cap() が冪等なため変化しない。
        let already_capped = crate::constitution::cap("固定の規則。", CONSTITUTION_LIMIT);
        let system = build_system(&already_capped, "");
        assert!(system.contains("固定の規則。"));
    }
}
