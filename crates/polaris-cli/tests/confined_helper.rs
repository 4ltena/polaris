//! 本物の `polaris` バイナリを、本物の拘束プロファイルの下でヘルパとして
//! 起動する統合テスト。
//!
//! これが無かったために M2 の中心機能が壊れたまま緑で出荷されかけた。
//! ツール層のテストはすべて `/bin/sh` の代役ヘルパを使っており、代役は
//! どんなプロファイルの下でも起動できる。一方、本物のヘルパは Rust の
//! ランタイムを持つ。macOS の既定（`workspace-write`）と `read-only` では
//! プロファイルに `(allow sysctl-read)` が無く、ガードページを張る前段で
//! `sysconf(_SC_PAGESIZE)` が拒否されて起動時に SIGABRT していた。つまり
//! ワークスペースの *内側* への `write` / `edit` すら失敗し、しかもその
//! 中断が「方針による拒否」と同じ形（非0終了 + stderr）でモデルへ届いて
//! いた。代役ヘルパのテストはこの区別を作れない。
//!
//! ここで確かめるのは 3 点である。
//!
//! 1. 書込可能ルートの内側への変更が本当に成立する（ファイルが存在し、
//!    中身が `content` そのものである = 本物のヘルパが JSON を解釈した）
//! 2. ルートの外への変更が本当に拒否される。しかも「ヘルパが起動できな
//!    かった」ではなく「方針が拒んだ」として拒否される
//! 3. 目印が一致しない `edit` は拒否ではなく要求の問題として返る
//!
//! この場所を選んだ理由: 本物のバイナリのパスをテストから得る手段が
//! `env!("CARGO_BIN_EXE_polaris")` であり、これはバイナリを宣言している
//! クレート（`polaris-cli`）の統合テストでのみ使える。`polaris-tools` の
//! ユニットテストからは、ビルド生成物のパスを推測する以外に到達できず、
//! 推測はプロファイルや `--target` の指定で静かに外れる。

use std::path::{Path, PathBuf};

use polaris_sandbox::{SandboxMode, SandboxPolicy};
use polaris_tools::ToolError;

/// OS がその操作を拒んだときに現れる文言。macOS の Seatbelt は `EPERM`、
/// Linux の landlock は `EACCES` を返し、それぞれ Rust の `io::Error` では
/// `(os error 1)` / `(os error 13)` として表示される。子が `/bin/sh` の
/// 場合はシェル自身の文言（`Operation not permitted`）が載る。
const OS_REFUSAL: &[&str] = &[
    "Operation not permitted",
    "Permission denied",
    "(os error 1)",
    "(os error 13)",
];

/// ヘルパが起動そのものに失敗したときに現れる文言。拒否と取り違えて
/// いないことを確かめるために使う。
const STARTUP_FAILURE: &[&str] = &["panicked", "fatal runtime error", "guard page"];

/// 本番と同じ経路でヘルパのパスを得る。`main.rs` は `staged_helper` を
/// 通しており、テストはその実体版（実行ファイルを差し替えられる方）を
/// 本物のバイナリに対して呼ぶ。
///
/// 本物のバイナリを *書込可能ルートの内側へ置いてから* 渡す。これが本番の
/// 配置そのものである（`<project>/target/debug/polaris` を `<project>` を
/// ルートにして起動する）。`env!("CARGO_BIN_EXE_polaris")` をそのまま渡すと、
/// ルートは毎回新しい一時ディレクトリなのでバイナリは必ずルートの外にあり、
/// `staged_helper_from` は早期 return して複製も権限設定も退避先の検証も
/// 一度も走らない。それでは「退避先がルートの外にある」という主張が構造上
/// 必ず真になり、何も確かめていないことになる。
fn staged_real_binary(policy: &SandboxPolicy, state_dir: &Path) -> PathBuf {
    let inside_the_root = policy.writable_roots()[0].join("polaris");
    std::fs::copy(env!("CARGO_BIN_EXE_polaris"), &inside_the_root)
        .expect("本物のバイナリを書込可能ルートの内側へ置けない");

    polaris_sandbox::stage::staged_helper_from(policy, state_dir, &inside_the_root)
        .expect("ヘルパを用意できない")
}

fn workspace_write(root: &Path) -> SandboxPolicy {
    SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.to_path_buf()]).expect("方針")
}

#[test]
fn a_write_inside_the_root_lands_through_the_real_binary_under_a_real_profile() {
    let root = tempfile::tempdir().expect("一時ディレクトリ");
    let state = tempfile::tempdir().expect("一時ディレクトリ");
    let policy = workspace_write(root.path());
    let helper = staged_real_binary(&policy, state.path());

    // ここは `staged_helper_from` が実際に複製したことを見ている。渡した
    // 実体は書込可能ルートの内側にあるので、退避が働かなければこの主張は
    // 偽になる（ワークスペースへ書ける者が次の変更操作のヘルパを差し替え
    // られる状態がそのまま残る）。
    assert!(
        !helper.starts_with(policy.writable_roots()[0].as_path()),
        "ヘルパが書込可能ルートの内側にある: {}",
        helper.display()
    );
    assert!(
        helper.exists(),
        "退避したはずのヘルパが無い: {}",
        helper.display()
    );

    let target = policy.writable_roots()[0].join("in.txt");
    let msg = polaris_tools::write::write(&policy, &helper, &target, "本文")
        .expect("ワークスペースの内側への書き込みが失敗した（本物のヘルパが拘束下で起動できていない可能性がある）");

    // 中身が `content` そのものであることを見る。代役ヘルパのテストが
    // 見ているのは「stdin に乗った直列化結果」であって、これではない。
    // ここが一致するのは、本物のヘルパが JSON を `Mutation` として解釈し、
    // `helper::apply` が実際に走ったときだけである。
    assert_eq!(
        std::fs::read_to_string(&target).expect("ファイルが無い"),
        "本文",
        "書かれた内容が content そのものでない"
    );
    assert!(!msg.trim().is_empty(), "結果の説明が空");
}

#[test]
fn a_write_outside_the_root_is_refused_by_the_policy_not_by_a_helper_that_could_not_start() {
    let root = tempfile::tempdir().expect("一時ディレクトリ");
    let outside = tempfile::tempdir().expect("一時ディレクトリ");
    let state = tempfile::tempdir().expect("一時ディレクトリ");
    let policy = workspace_write(root.path());
    let helper = staged_real_binary(&policy, state.path());

    // 対照実験をこのテストの中に置く。ルート外への失敗だけを見るテストは、
    // 「方針が拒んだ」と「ヘルパが一度も走らなかった」を区別できない
    // ——「Err である」「ファイルが無い」「文面にパスと方針がある」の3点は、
    // 起動に失敗したヘルパでも完全に満たされる（現に、この修正前の
    // 本物のバイナリはそう振る舞っていた）。同じ方針・同じヘルパで内側
    // への書き込みが成立することを先に確かめ、以降の失敗が起動の失敗では
    // ないことをこのテスト自身で保証する。
    let control = policy.writable_roots()[0].join("control.txt");
    polaris_tools::write::write(&policy, &helper, &control, "control")
        .expect("対照実験が失敗した: このヘルパはこのプロファイルの下で起動できていない");
    assert_eq!(
        std::fs::read_to_string(&control).expect("対照のファイルが無い"),
        "control"
    );

    let target = outside
        .path()
        .canonicalize()
        .expect("canonicalize")
        .join("pwned.txt");
    let err = polaris_tools::write::write(&policy, &helper, &target, "本文")
        .expect_err("ルート外への書き込みが成功した");

    assert!(
        !target.exists(),
        "ファイルが作られている: {}",
        target.display()
    );

    let ToolError::WriteDenied { detail, .. } = &err else {
        panic!("方針による拒否として返っていない: {err:?}");
    };
    assert!(
        OS_REFUSAL.iter().any(|s| detail.contains(s)),
        "子の出力に OS の拒否が無い（拒否ではない何かを拒否と名付けている）: {detail}"
    );
    assert!(
        !STARTUP_FAILURE.iter().any(|s| detail.contains(s)),
        "ヘルパが起動に失敗しており、それが拒否として報告されている: {detail}"
    );

    // 仕様が要求する拒否メッセージの中身（パス・方針・書込可能ルート）。
    let msg = err.to_string();
    assert!(
        msg.contains(&target.display().to_string()),
        "パスが無い: {msg}"
    );
    assert!(msg.contains("workspace-write"), "方針が無い: {msg}");
    assert!(
        msg.contains(&policy.writable_roots()[0].display().to_string()),
        "書込可能ルートが無い: {msg}"
    );
}

#[test]
fn an_edit_whose_marker_is_absent_is_a_request_problem_not_a_policy_denial() {
    // ルートの *内側* にあるファイルを、存在しない目印で置換しようとする。
    // 方針は一切関係しない失敗であり、モデルがすべきことは目印を選び直す
    // ことである。これを拒否として返すと、モデルは権限の問題を探しに行き、
    // 往復を1回捨てる。
    let root = tempfile::tempdir().expect("一時ディレクトリ");
    let state = tempfile::tempdir().expect("一時ディレクトリ");
    let policy = workspace_write(root.path());
    let helper = staged_real_binary(&policy, state.path());

    let target = policy.writable_roots()[0].join("f.txt");
    std::fs::write(&target, "元の本文").expect("書けない");

    let err = polaris_tools::edit::edit(&policy, &helper, &target, "存在しない目印", "新")
        .expect_err("目印が無いのに置換が成功した");

    let ToolError::MutationFailed { detail, .. } = &err else {
        panic!("要求の問題ではなく別の種類として返っている: {err:?}");
    };
    assert!(
        detail.contains("not found"),
        "子が報告した理由が届いていない: {detail}"
    );

    let msg = err.to_string();
    assert!(
        !msg.contains("workspace-write"),
        "要求の問題なのに方針を名指ししている（モデルが権限の問題を探し始める）: {msg}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("読めない"),
        "元の本文",
        "失敗したのに中身が変わっている"
    );
}
