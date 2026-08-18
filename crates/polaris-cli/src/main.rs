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

    /// 拘束された子として 1 件の変更操作を標準入力から読んで実行する。
    /// 内部用であり、利用者が直接使うものではない。
    #[arg(long, hide = true)]
    confined_apply: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    if args.confined_apply {
        return run_confined_apply();
    }

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

    let config = polaris_core::config::load(&cwd).unwrap_or_else(|e| {
        eprintln!("設定を読めない: {e}");
        polaris_core::config::Config::default()
    });
    let discovered = polaris_skills::discover(&cwd, &config.skills_paths);
    for line in format_skipped_skills(&discovered.skipped) {
        eprintln!("{line}");
    }

    // 毎ターン載るものはここで一度だけ組み立てる。組み立てそのものは
    // polaris-core にあり、予算のテストも同じ関数を呼ぶ。ここで組み立て直したり
    // 継ぎ足したりすると、本番が送るものとテストが測るものが別になる。
    let always_on = prompt::assemble_always_on(&constitution, &environment, &discovered.skills);

    match agent::run(
        &provider,
        &mut session,
        &mut audit,
        &mut stop,
        &always_on,
        &discovered.skills,
    )
    .await
    {
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

/// 拘束された子としての入口。標準入力の JSON 1 件を実行して終わる。
fn run_confined_apply() -> ExitCode {
    use std::io::Read;

    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        eprintln!("標準入力を読めない: {e}");
        return ExitCode::FAILURE;
    }
    let mutation: polaris_sandbox::Mutation = match serde_json::from_str(&buf) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("操作を解釈できない: {e}");
            return ExitCode::FAILURE;
        }
    };
    match polaris_sandbox::helper::apply(&mutation) {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{msg}");
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

/// 飛ばした skill を1件1行に整形する。壊れた skill が1件あっても、読めなかった
/// ことと理由を利用者へ黙って握り潰さないための表示用ロジックを、
/// eprintln! 呼び出しから切り離してここへ置く（副作用なしでテストできる）。
fn format_skipped_skills(skipped: &[polaris_skills::Skipped]) -> Vec<String> {
    skipped
        .iter()
        .map(|s| format!("skill を読めない: {s}"))
        .collect()
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
    fn skipped_skills_are_reported_one_line_each_naming_directory_and_cause() {
        // discover が拾った skipped を握り潰さず、1件1行で標準エラーへ渡す
        // ための整形ロジックを直接確かめる。実プロセスを起動して stderr を
        // 検証すると API キーを要求する経路まで踏む必要があるため、ここでは
        // 整形関数だけを切り出して検証する。
        //
        // ディレクトリ名だけを見ると、原因を丸ごと捨てる整形（`{s.dir_name}`
        // だけを出す）でも通ってしまう。この関数は「skill が黙って消えた」と
        // 利用者の間に立つ唯一のものなので、原因が出ていること、しかも
        // 読めなかったのか検証に落ちたのかを取り違えていないことまで見る。
        // 2件は別々の SkipCause 変種にしてある。
        let skipped = vec![
            polaris_skills::Skipped {
                dir_name: "unreadable-one".into(),
                cause: polaris_skills::SkipCause::Unreadable(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "権限が無い",
                )),
            },
            polaris_skills::Skipped {
                dir_name: "invalid-two".into(),
                cause: polaris_skills::SkipCause::Invalid(
                    polaris_skills::SkillError::InvalidName {
                        name: "Invalid-Two".into(),
                    },
                ),
            },
        ];

        let lines = format_skipped_skills(&skipped);

        assert_eq!(lines.len(), 2, "1件1行になっていない: {lines:?}");
        assert!(
            lines[0].contains("unreadable-one"),
            "ディレクトリ名が含まれていない: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("権限が無い"),
            "読めなかった原因が含まれていない: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("invalid-two"),
            "ディレクトリ名が含まれていない: {}",
            lines[1]
        );
        assert!(
            lines[1].contains("命名規則"),
            "検証に落ちた原因が含まれていない: {}",
            lines[1]
        );
        // 各行が自分の原因だけを運ぶ。全件の原因を全行へ書くような整形は、
        // どの skill がなぜ消えたのかを結局伝えない。
        assert!(
            !lines[0].contains("命名規則") && !lines[1].contains("権限が無い"),
            "行ごとの原因が混ざっている: {lines:?}"
        );
    }

    #[test]
    fn nothing_is_printed_when_no_skill_was_skipped() {
        // 何も飛ばしていないのに1行でも出れば、利用者は存在しない障害を
        // 追うことになる。`.map().collect()` の副産物としてではなく、
        // 空入力から空出力であることを直接固定する。
        assert!(
            format_skipped_skills(&[]).is_empty(),
            "飛ばした skill が無いのに出力がある"
        );
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
