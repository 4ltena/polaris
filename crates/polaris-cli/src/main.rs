//! `polaris` バイナリの入口。環境変数から接続先を決め、常時コンテキストを
//! 組み立てて、エージェントループを1回走らせる。

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use polaris_core::{
    agent, audit::AuditLog, constitution, prompt, session::Session, stop::StopTracker,
};
use polaris_provider::openai::OpenAiProvider;

#[derive(Parser)]
#[command(
    name = "polaris",
    about = "最小コンテキストのコーディングエージェント",
    after_help = "\
環境変数:
  POLARIS_API_KEY   必須。OpenAI 互換エンドポイントの API キー。既定値は無い。
  POLARIS_BASE_URL  省略時 https://api.openai.com/v1
  POLARIS_MODEL     省略時 gpt-5.4
"
)]
struct Args {
    /// 実行する指示。
    #[arg(short, long)]
    prompt: String,

    /// 監査ログの出力先。省略すると `~/.polaris/state/<project-id>/audit.jsonl` を使う。
    #[arg(long)]
    audit: Option<PathBuf>,

    /// 1 回の実行で許すターン数の上限。
    #[arg(long, default_value_t = 20)]
    max_turns: u32,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    let Ok(api_key) = std::env::var("POLARIS_API_KEY") else {
        eprintln!("POLARIS_API_KEY が設定されていない");
        return ExitCode::FAILURE;
    };
    let base_url =
        std::env::var("POLARIS_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
    let model = std::env::var("POLARIS_MODEL").unwrap_or_else(|_| "gpt-5.4".into());

    let provider = match OpenAiProvider::new(base_url, api_key, model) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let mut session = Session::new();
    session.push_user(&args.prompt);

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let audit_path = match args.audit {
        Some(p) => p,
        None => match default_audit_path(&cwd) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("監査ログの既定パスを決められない: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    let mut audit = match AuditLog::open(&audit_path) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("監査ログを開けない: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut stop = StopTracker::new(args.max_turns);

    let constitution = constitution::load(&cwd);
    let environment = constitution::environment_block(&cwd, None);
    let system = prompt::build_system(&constitution, &environment);

    match agent::run(&provider, &mut session, &mut audit, &mut stop, &system).await {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// 監査ログの既定パスを組み立てる。作業ディレクトリの内側には置かない。
/// リポジトリを `git add -A` した瞬間に、伏字化を1件でも取りこぼした行が
/// そのままコミットへ混ざりうるため（secret_screen は自称するとおり保険で
/// あって保証ではない）、常にホーム配下の状態ディレクトリへ書く。
///
/// `<project-id>` はプロジェクトを一意に識別できればよく、プロジェクトの
/// 正体を推測できる必要は無いため、canonicalize したパスのハッシュ値を使う。
fn default_audit_path(cwd: &Path) -> io::Result<PathBuf> {
    let canonical = cwd.canonicalize()?;
    let id = project_id(&canonical);

    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME が設定されていない"))?;
    let dir = Path::new(&home).join(".polaris").join("state").join(id);
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("audit.jsonl"))
}

/// canonicalize 済みパスから決定的なプロジェクト識別子を作る。
///
/// 標準ライブラリの `DefaultHasher` はアルゴリズムを規定しておらず
/// Rust のバージョンを跨いで変わりうる（変われば同じプロジェクトの監査ログが
/// 別ディレクトリへ分裂する）ため使わない。ここでは FNV-1a
/// をそのまま書き下し、アルゴリズムを固定する。
fn project_id(canonical_path: &Path) -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET_BASIS;
    for byte in canonical_path.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_id_is_deterministic_for_the_same_path() {
        let a = project_id(Path::new("/w/polaris"));
        let b = project_id(Path::new("/w/polaris"));
        assert_eq!(a, b);
    }

    #[test]
    fn project_id_differs_for_different_paths() {
        let a = project_id(Path::new("/w/polaris"));
        let b = project_id(Path::new("/w/other"));
        assert_ne!(a, b);
    }

    #[test]
    fn default_audit_path_lives_under_home_state_not_cwd() {
        let home = tempfile::tempdir().expect("一時ディレクトリ");
        let project = tempfile::tempdir().expect("一時ディレクトリ");

        // SAFETY: このテストは同一プロセス内で HOME を一時的に差し替えるだけで、
        // 他プロセスへは影響しない。std::env::set_var の安全条件はマルチ
        // スレッドからの同時読み書きだが、cargo test はテストごとに別プロセス
        // または直列実行のどちらでもこの変更を他のテストと共有しないため、
        // ここでの使用に問題はない。
        let prev_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", home.path());
        }

        let got = default_audit_path(project.path()).expect("既定パスを決められない");

        if let Some(p) = prev_home {
            unsafe {
                std::env::set_var("HOME", p);
            }
        } else {
            unsafe {
                std::env::remove_var("HOME");
            }
        }

        assert!(
            got.starts_with(home.path()),
            "監査ログがホーム配下にない: {}",
            got.display()
        );
        assert!(
            !got.starts_with(project.path()),
            "監査ログが作業ディレクトリの内側にある: {}",
            got.display()
        );
        assert_eq!(got.file_name().unwrap(), "audit.jsonl");
        assert!(
            got.parent().unwrap().is_dir(),
            "ディレクトリが作られていない"
        );
    }
}
