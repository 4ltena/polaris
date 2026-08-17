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
