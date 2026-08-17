//! CLI バイナリの統合テスト。実プロセスを起動して振る舞いを確かめる。

use std::process::Command;

/// API キーが無い状態で起動したら、鍵が無いことを明示して終了する。
/// 実際のネットワークへは出ない。
#[test]
fn reports_missing_api_key() {
    let exe = env!("CARGO_BIN_EXE_polaris");
    let out = Command::new(exe)
        .args(["-p", "hello"])
        .env_remove("POLARIS_API_KEY")
        .output()
        .expect("起動できない");

    assert!(!out.status.success(), "鍵が無いのに成功している");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("POLARIS_API_KEY"),
        "鍵が無いことを伝えていない: {err}"
    );
}

/// `--help` が接続先を決める3つの環境変数（と既定値）に触れていない場合、
/// 新しく手にした人はソースを読まないと必要な環境変数に気づけない。
#[test]
fn help_mentions_the_three_environment_variables() {
    let exe = env!("CARGO_BIN_EXE_polaris");
    let out = Command::new(exe)
        .arg("--help")
        .output()
        .expect("起動できない");

    assert!(out.status.success(), "--help が失敗している");
    let text = String::from_utf8_lossy(&out.stdout);
    for var in ["POLARIS_API_KEY", "POLARIS_BASE_URL", "POLARIS_MODEL"] {
        assert!(text.contains(var), "--help に {var} が無い: {text}");
    }
}
