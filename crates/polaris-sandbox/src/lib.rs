//! サンドボックス方針の定義と、OS 機構への委譲。
//!
//! このクレートは強制の仕組みそのものを実装しない。方針と正規化済みの
//! 書込可能ルートを保持し、プラットフォーム固有の機構へ渡すだけである。

pub mod policy;

pub use policy::{SandboxMode, SandboxPolicy};

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("サンドボックスを適用できなかった: {0}")]
    NotEnforced(String),
    #[error("拒否された: {path}（方針 {policy}）")]
    Denied { path: String, policy: String },
    #[error("入出力: {0}")]
    Io(#[from] std::io::Error),
    #[error("このプラットフォームには強制の委譲先が無い")]
    UnsupportedPlatform,
}
