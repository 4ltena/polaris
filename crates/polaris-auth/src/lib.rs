//! ChatGPT のサブスクリプション認証（OAuth）の寿命と資格情報の保管。
//!
//! このクレートは polaris の他のクレートに依存しない。Responses API も
//! `Provider` トレイトも知らない。認証は時計とファイルシステムと
//! ネットワークの競合で壊れ、ワイヤ変換は形式の解釈で壊れる。同じ場所に
//! 置くと、落ちたテストがどちらの欠陥かを指さない。
//!
//! `~/.codex/` には読み書きとも一切触れない。サーバが refresh token を
//! ローテーションさせる場合、こちらが更新した瞬間に codex 側の控えが
//! 失効しうる。独立した store を持てば、この事故は原理的に起きない。

pub mod pkce;

/// 認可の発行者。
pub const ISSUER: &str = "https://auth.openai.com";
/// codex CLI に登録された client_id。redirect_uri もこれに紐づく。
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// 登録済みの redirect_uri。ポートは選び直せない。
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
/// コールバックを待ち受けるポート。`REDIRECT_URI` と必ず一致させる。
pub const CALLBACK_PORT: u16 = 1455;
/// 要求する scope。`offline_access` が無いと refresh token が返らない。
pub const SCOPE: &str = "openid profile email offline_access";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("入出力エラー: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP エラー: {0}")]
    Http(String),
    #[error("応答を解釈できない: {0}")]
    Decode(String),
    #[error("認可が拒否された: {0}")]
    Denied(String),
    // 書式に `{CALLBACK_PORT}` と書いてはいけない。thiserror は `{NAME}` を
    // variant のフィールド名として解釈するので、定数を書くとコンパイルが
    // 通らない。ポート番号は literal で書き、`CALLBACK_PORT` と一致させる。
    #[error(
        "ポート 1455 を使えない。redirect_uri がこのポートに登録されているため選び直せない。\
         `codex login` が同時に走っていないか確かめること: {0}"
    )]
    PortInUse(String),
    #[error("ログインしていない")]
    NotLoggedIn,
}
