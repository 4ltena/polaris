//! サブコマンドが実際に到達することを固定する。
//!
//! M2 の Task 8 で、`--prompt` の必須検証によって `--confined-apply` が
//! 到達不能になっていた。同じ形の罠なので、引数の組み立てを目で読むのでは
//! なく、実バイナリを起動して確かめる。
//!
//! `HOME` を一時ディレクトリへ向けるので、実の `~/.polaris` にも
//! `~/.codex` にも触れない。

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_polaris")
}

/// `login` は `--prompt` 無しで解析を通る。`--help` で止めるので
/// ブラウザは開かず、ネットワークにも出ない。
#[test]
fn the_login_subcommand_is_reachable_without_a_prompt() {
    let out = Command::new(bin())
        .args(["login", "--help"])
        .output()
        .expect("起動できない");
    assert!(
        out.status.success(),
        "login --help が失敗した。stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `logout` は `--prompt` 無しで最後まで走る。ログインしていない状態でも
/// 失敗しない。
#[test]
fn the_logout_subcommand_runs_without_a_prompt() {
    let home = tempfile::tempdir().expect("一時ディレクトリ");
    let out = Command::new(bin())
        .arg("logout")
        .env("HOME", home.path())
        .output()
        .expect("起動できない");
    assert!(
        out.status.success(),
        "logout が失敗した。stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `logout` は自分の store だけを消す。`~/.codex/auth.json` には触れない。
#[test]
fn logout_never_touches_the_codex_store() {
    let home = tempfile::tempdir().expect("一時ディレクトリ");
    let codex = home.path().join(".codex");
    std::fs::create_dir_all(&codex).expect("作れない");
    let codex_auth = codex.join("auth.json");
    std::fs::write(&codex_auth, b"{\"sentinel\":true}").expect("書けない");

    let polaris_dir = home.path().join(".polaris");
    std::fs::create_dir_all(&polaris_dir).expect("作れない");
    let polaris_auth = polaris_dir.join("auth.json");
    std::fs::write(
        &polaris_auth,
        b"{\"access_token\":\"a\",\"refresh_token\":\"r\",\"account_id\":\"x\"}",
    )
    .expect("書けない");

    let out = Command::new(bin())
        .arg("logout")
        .env("HOME", home.path())
        .output()
        .expect("起動できない");
    assert!(out.status.success(), "logout が失敗した");

    assert!(!polaris_auth.exists(), "自分の store を消していない");
    assert_eq!(
        std::fs::read(&codex_auth).expect("読めない"),
        b"{\"sentinel\":true}",
        "codex の store に触れている"
    );
}

/// 通常経路では `--prompt` が要る。任意にしたことで、指示なしの実行が
/// 黙って走り出してはいけない。上の 3 本の対であり、これが無いと
/// 「prompt を一切見ない」実装が通る。
#[test]
fn the_normal_path_still_requires_a_prompt() {
    let out = Command::new(bin()).output().expect("起動できない");
    assert!(!out.status.success(), "指示なしで成功している");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("prompt"),
        "何が足りないかを言っていない: {stderr}"
    );
}

/// 受け入れ基準 5。`POLARIS_PROVIDER` を設定しない既定の実行が、これまで
/// どおり openai の経路へ入る。キーが無いことを openai の言葉で叱ることで、
/// codex の経路へ逸れていないことが分かる。
#[test]
fn the_default_provider_is_still_openai() {
    let out = Command::new(bin())
        .args(["-p", "x"])
        .env_remove("POLARIS_PROVIDER")
        .env_remove("POLARIS_API_KEY")
        .output()
        .expect("起動できない");
    assert!(!out.status.success(), "キー無しで成功している");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("POLARIS_API_KEY"),
        "openai の経路へ入っていない: {stderr}"
    );
}

/// 受け入れ基準 4。ログアウト状態で codex を指すと、`polaris login` を
/// 名指しするエラーが出る。HTTP エラーにはならない。ネットワークへ出る前に
/// 止まるので、実 API は叩かない。
#[test]
fn a_logged_out_codex_run_names_the_login_command() {
    let home = tempfile::tempdir().expect("一時ディレクトリ");
    let out = Command::new(bin())
        .args(["-p", "何行か"])
        .env("HOME", home.path())
        .env("POLARIS_PROVIDER", "codex")
        .env_remove("POLARIS_API_KEY")
        .output()
        .expect("起動できない");
    assert!(!out.status.success(), "ログインしていないのに成功している");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("polaris login"),
        "やるべきことを名指ししていない: {stderr}"
    );
    assert!(
        !stderr.contains("status "),
        "HTTP エラーとして出ている: {stderr}"
    );
}

/// 未知のプロバイダ名は起動時に落とす。実行してから「モデルが応答しない」
/// で気付くのでは遅い。
#[test]
fn an_unknown_provider_name_fails_fast() {
    let out = Command::new(bin())
        .args(["-p", "x"])
        .env("POLARIS_PROVIDER", "nonesuch")
        .env("POLARIS_API_KEY", "dummy")
        .output()
        .expect("起動できない");
    assert!(!out.status.success(), "未知のプロバイダで成功している");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nonesuch"), "名前を出していない: {stderr}");
}
