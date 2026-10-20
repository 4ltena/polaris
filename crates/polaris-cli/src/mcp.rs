//! MCP registrations, secrets, catalog checks and setup from the terminal.
//! Values are never printed; registrations apply from the next run.

use polaris_core::{
    approval::{ApprovalPolicy, Approver, Decision, Gate},
    mcp::{local::LocalHost, operation::ExternalCeiling},
};
use polaris_mcp::{Error, registration::Destination, store::SecretValue};
use std::io::{self, BufRead, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(clap::Args)]
pub struct Args {
    /// 対象プロジェクト。既定は現在のディレクトリ。
    #[arg(long, global = true)]
    project: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// 登録済みのサーバーと認証情報の参照名を表示する。値は表示しない。
    List,
    /// このプロジェクトでサーバーを使えるようにする。
    Enable { server: String },
    /// このプロジェクトでサーバーを使わないようにする。
    Disable { server: String },
    /// 認証情報をパイプで受け取り、参照名で保存する。
    SecretSet { reference: String },
    /// 保存した認証情報を削除する。
    SecretDelete { reference: String },
    /// サーバーへ接続してカタログを確認し、索引を更新する。モデルは使わない。
    Check { server: String },
    /// CodexとClaudeの設定からサーバーを検出し、確認したものを登録する。
    Setup {
        /// 1件ずつ答える代わりに、全件を承認する。**確認を省くのではなく、
        /// 確認をこの引数に置く。** 画面は従来どおり全部出るので、何を
        /// 承認したかは記録に残る。ツール実行のたびの確認は別の境界で、
        /// これでは変わらない。
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// 起動のたびに取得する形式のサーバーを、1度だけ取得して固定する。
    /// 取得後は普通のローカル実行物として登録できるようになる。
    Materialize {
        /// 設定に書かれたサーバー名。
        server: String,
        /// 実行に使うinterpreter。省略時は取得の形式から決める。
        /// npm形式はnode、PyPI形式はuvが管理するPython。
        #[arg(long)]
        runtime: Option<PathBuf>,
        /// 確認を省く。**省くのではなく、確認をこの引数に置く。**
        /// 第三者のコードを取得して実行する操作なので、端末で1件ずつ答える
        /// か、これを書いて実行するかのどちらかが要る。まとめて取得する
        /// script のためにある。
        #[arg(long)]
        yes: bool,
    },
    /// 登録済みUnity MCPのruntimeを固定配置し、次回起動用の設定を更新する。
    Configure {
        server: String,
        #[arg(long, value_parser = ["unity-local-v2"])]
        preset: String,
        #[arg(long)]
        source: PathBuf,
        /// 表示する構成内容を承認する。接続確認やtool実行は行わない。
        #[arg(long)]
        yes: bool,
    },
}

/// Answers every registration card with yes, after printing it. Used only
/// when the person supplied `--yes` (or setup's `-y` alias).
struct BulkApprover;

impl Approver for BulkApprover {
    fn ask(&mut self, reason: &str) -> Decision {
        eprintln!("Approval required: {reason}");
        eprintln!("Allow this? [y/N] y  (--yes により承認)");
        Decision::Allow
    }
}

fn refused(reason: polaris_core::mcp::setup::launcher::Refused) -> &'static str {
    use polaris_core::mcp::setup::launcher::Refused;
    match reason {
        Refused::NotALauncher => "その名前の、起動のたびに取得する形式の設定が見つかりません。",
        Refused::ManagerNotSupported => "この取得方法にはまだ対応していません（npmのみ）。",
        Refused::FetchFailed => {
            "取得できませんでした。ネットワークとパッケージ名を確認してください。"
        }
        Refused::NoEntryPoint => {
            "取得しましたが、パッケージが起動するファイルを示していないか、置き場所の外を指しています。"
        }
        Refused::RuntimeNotPinned => "指定されたinterpreterが見つかりません。",
        Refused::NotSaved => "固定した内容を保存できません。",
    }
}

fn message(error: Error) -> &'static str {
    match error {
        Error::RevisionConflict => "設定が同時に変更されました。もう一度実行してください。",
        Error::InvalidRegistration | Error::Limit => "登録内容が不正か上限を超えています。",
        Error::Auth => "認証情報が見つからないか、参照されています。",
        Error::Protection => "安全に使えない場所やファイルが含まれています。",
        Error::Transport | Error::Timeout | Error::Protocol => "サーバーに接続できません。",
        Error::StopUnconfirmed => "サーバーの停止を確認できません。",
        _ => "MCPの設定を読み書きできません。",
    }
}

pub async fn run(args: Args) -> ExitCode {
    if cfg!(windows) {
        eprintln!("WindowsではMCPの保護された保存先を利用できません。変更していません。");
        return ExitCode::FAILURE;
    }
    let cwd = args
        .project
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let result = match args.command {
        Command::Configure {
            server,
            preset,
            source,
            yes,
        } => configure(&cwd, &server, &preset, &source, yes),
        command_args => match LocalHost::open(&cwd, true) {
            Ok(host) => command(&host, &cwd, command_args).await,
            Err(error) => Err(message(error).to_owned()),
        },
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(text) => {
            eprintln!("{text}");
            ExitCode::FAILURE
        }
    }
}

async fn command(host: &LocalHost, cwd: &std::path::Path, command: Command) -> Result<(), String> {
    let fail = |error: Error| message(error).to_owned();
    let store = host.store().ok_or_else(|| fail(Error::Storage))?;
    match command {
        Command::List => {
            let snapshot = store.load_snapshot().map_err(fail)?;
            let indexed = snapshot.registry.as_ref().and_then(|registry| {
                store
                    .load_catalog_index()
                    .ok()
                    .flatten()
                    .map(|index| index.verified(registry))
            });
            let Some(registry) = snapshot.registry else {
                println!("登録済みのサーバーはありません。`polaris mcp setup`で検出できます。");
                return Ok(());
            };
            println!("登録の版 {}", registry.revision);
            for server in &registry.servers {
                let destination = match &server.transport {
                    Destination::StreamableHttp { url, .. } => url.clone(),
                    Destination::Stdio { command, .. } => command.display().to_string(),
                };
                let enabled = server.enabled_projects.contains(host.project());
                let checked = indexed
                    .as_ref()
                    .and_then(|index| index.get(&server.server_id))
                    .map_or("カタログ未確認".to_owned(), |entry| {
                        format!("ツール{}件", entry.tools.len())
                    });
                println!(
                    "{}\t{}\t{}\t{}",
                    server.server_id,
                    if enabled { "有効" } else { "無効" },
                    checked,
                    destination
                );
            }
            if let Some(secrets) = snapshot.secrets {
                let names: Vec<_> = secrets.references().collect();
                println!(
                    "認証情報の参照名（版 {}）: {}",
                    secrets.revision(),
                    names.join(", ")
                );
            }
            Ok(())
        }
        Command::Enable { server } => toggle(host, cwd, &server, true),
        Command::Disable { server } => toggle(host, cwd, &server, false),
        Command::SecretSet { reference } => {
            if io::stdin().is_terminal() {
                return Err("端末に入力すると値が表示されるため、パイプで渡してください。".into());
            }
            let mut value = String::new();
            io::stdin()
                .lock()
                .read_line(&mut value)
                .map_err(|_| "認証情報を読み込めません。".to_owned())?;
            let value = value.trim_end_matches(['\r', '\n']);
            if value.is_empty() {
                return Err("認証情報が空です。".into());
            }
            let revision = store
                .load_secrets()
                .map_err(fail)?
                .map_or(0, |secrets| secrets.revision());
            store
                .update_secret(revision, &reference, Some(SecretValue::new(value)))
                .map_err(fail)?;
            println!("{reference} を保存しました。次の実行から使えます。");
            Ok(())
        }
        Command::SecretDelete { reference } => {
            let revision = store
                .load_secrets()
                .map_err(fail)?
                .map_or(0, |secrets| secrets.revision());
            store
                .update_secret(revision, &reference, None)
                .map_err(fail)?;
            println!("{reference} を削除しました。");
            Ok(())
        }
        Command::Check { server } => {
            println!("{}", check(host, &server).await?);
            Ok(())
        }
        Command::Setup { yes } => setup(host, yes).await,
        Command::Materialize {
            server,
            runtime,
            yes,
        } => materialize(&server, runtime.as_deref(), yes),
        // Configuration must enter through the noncreating preflight above.
        Command::Configure { .. } => Err("構成には専用の読み取り準備経路が必要です。".into()),
    }
}

#[cfg(not(unix))]
fn configure(
    _cwd: &std::path::Path,
    _server: &str,
    _preset: &str,
    _source: &std::path::Path,
    _confirmed: bool,
) -> Result<(), String> {
    Err("このプラットフォームではMCP構成に必要な保護を確認できません。".into())
}

#[cfg(unix)]
fn configure(
    cwd: &std::path::Path,
    server: &str,
    preset: &str,
    source: &std::path::Path,
    confirmed: bool,
) -> Result<(), String> {
    use polaris_core::{
        management_effect::ManagementEffectStoreV1, mcp::configure::UnityLocalV2Preparation,
        project_mutation::ProjectMutationOwnerV1,
    };

    // Refuse before opening even a noncreating store when no answer is possible.
    if !confirmed && !io::stdin().is_terminal() {
        return Err("構成の確認には対話端末が必要です。--yes でも承認できます。".into());
    }
    if preset != "unity-local-v2" {
        return Err("対応する構成presetではありません。".into());
    }
    let safe = |text: String| text.escape_debug().to_string();
    let host = LocalHost::open_existing(cwd).map_err(|error| message(error).to_owned())?;
    let prepared = UnityLocalV2Preparation::prepare(&host, server, source).map_err(safe)?;
    let card = configure_card(prepared.preview());
    let decision = if confirmed {
        BulkApprover.ask(&card)
    } else {
        crate::TerminalApprover.ask(&card)
    };
    if !matches!(decision, Decision::Allow) {
        return Err("構成を取り消しました。".into());
    }

    // The preparation holds no registry lock across the approval question.
    // Destination/effect creation begins only after the answer above.
    let project = polaris_core::project::resolve_root(cwd)
        .canonicalize()
        .map_err(|error| safe(error.to_string()))?;
    let mutation =
        ProjectMutationOwnerV1::for_project(&project).map_err(|error| safe(error.to_string()))?;
    let lease = mutation
        .try_mutation()
        .map_err(|error| safe(error.to_string()))?;
    let state =
        polaris_core::project::state_dir(&project).map_err(|error| safe(error.to_string()))?;
    let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease)
        .map_err(|error| safe(error.to_string()))?;
    let receipt = prepared
        .apply(&host, &effects, &lease, &project)
        .map_err(safe)?;
    println!(
        "{} を構成しました（登録の版 {}）。次の実行から反映します。\n配置先: {}\n接続確認は `polaris mcp check {}` で行います。",
        receipt.server_id.escape_debug(),
        receipt.registry_revision,
        receipt.cwd.display().to_string().escape_debug(),
        receipt.server_id.escape_debug(),
    );
    Ok(())
}

#[cfg(unix)]
fn configure_card(preview: &polaris_core::mcp::configure::UnityLocalV2Preview) -> String {
    let text = |value: &str| value.escape_debug().to_string();
    let path = |value: &std::path::Path| text(&value.display().to_string());
    let mut lines = vec![
        "Unity MCPの固定配置と登録更新".into(),
        format!(
            "preset: unity-local-v2 / server: {}",
            text(&preview.server_id)
        ),
        format!("project: {}", text(&preview.project_id)),
        format!("source: {}", path(&preview.source)),
        format!("node: {}", path(&preview.node)),
        format!("node SHA256: {}", text(&preview.node_sha256)),
        format!("entry: {}", path(&preview.entry)),
        format!("新entry: {}", path(&preview.new_cwd.join("build/index.js"))),
        format!("entry SHA256: {}", text(&preview.entry_sha256)),
        format!("manifest SHA256: {}", text(&preview.manifest_sha256)),
        format!(
            "選択: {} files / {} bytes",
            preview.selected_files, preview.selected_bytes
        ),
        "対象: build/、node_modules/、package.json、存在するlockfile".into(),
        format!("現在cwd: {}", path(&preview.old_cwd)),
        format!("配置先・新cwd: {}", path(&preview.new_cwd)),
        format!(
            "protocol: {:?} / sandbox: {}",
            preview.protocol_profile,
            text(&preview.sandbox_profile)
        ),
        format!(
            "登録の版: {} → {}",
            preview.registry_before_revision, preview.registry_after_revision
        ),
        "登録差分: cwd・entry引数・script pin・protocol・sandbox".into(),
        "server ID・project集合・capabilities・timeouts・秘密参照は保持します。".into(),
        "限定hardlink pair（9,817,266 bytes・同一inode/nlink=2を別々のnlink=1へコピー）:".into(),
        format!("pair SHA256: {}", text(&preview.hardlink_pair_sha256)),
    ];
    for entry in &preview.hardlink_pair {
        lines.push(format!("  {}", path(entry)));
    }
    lines.push("固定除外:".into());
    for entry in &preview.fixed_exclusions {
        lines.push(format!("  {}", path(entry)));
    }
    lines.push("保持する秘密参照名（値は表示しません）:".into());
    if preview.secret_references.is_empty() {
        lines.push("  なし".into());
    } else {
        for reference in &preview.secret_references {
            lines.push(format!("  {}", text(reference)));
        }
    }
    lines.push("通信範囲: このMac内のloopbackのみ。".into());
    lines.push(
        "C28の配置完了後にC27の登録を更新します。旧runtimeと未参照のコピーは保持します。".into(),
    );
    lines.push("準備・配置ではプログラムの起動、download、接続確認は行いません。".into());
    lines.join("\n")
}

#[cfg(all(test, unix))]
mod configure_tests {
    use super::configure_card;
    use polaris_core::mcp::configure::UnityLocalV2Preview;
    use polaris_mcp::registration::ProtocolProfile;
    use std::path::PathBuf;

    #[test]
    fn configuration_card_binds_digests_and_escapes_untrusted_fields() {
        let preview = UnityLocalV2Preview {
            server_id: "unity\nforged approval".into(),
            project_id: "project-test".into(),
            source: PathBuf::from("/source\u{1b}[2J"),
            old_cwd: PathBuf::from("/store/servers/unity"),
            new_cwd: PathBuf::from("/store/servers/unity/runtime-1234"),
            node: PathBuf::from("/runtime/node"),
            entry: PathBuf::from("/source/build/index.js"),
            node_sha256: "a".repeat(64),
            entry_sha256: "b".repeat(64),
            manifest_sha256: "c".repeat(64),
            selected_files: 42,
            selected_bytes: 12345,
            fixed_exclusions: vec![PathBuf::from("node_modules/.bin")],
            hardlink_pair: [
                PathBuf::from("node_modules/esbuild/bin/esbuild"),
                PathBuf::from("node_modules/@esbuild/darwin-arm64/bin/esbuild"),
            ],
            hardlink_pair_sha256: "d".repeat(64),
            secret_references: vec!["reference\nforged-value".into()],
            registry_before_revision: 7,
            registry_after_revision: 8,
            protocol_profile: ProtocolProfile::Auto2024Compat,
            sandbox_profile: "loopback".into(),
        };
        let card = configure_card(&preview);
        for digest in [
            &preview.node_sha256,
            &preview.entry_sha256,
            &preview.manifest_sha256,
            &preview.hardlink_pair_sha256,
        ] {
            assert!(card.contains(digest));
        }
        assert!(card.contains("/store/servers/unity/runtime-1234/build/index.js"));
        assert!(card.contains("42 files / 12345 bytes"));
        assert!(card.contains("7 → 8"));
        assert!(card.contains("node_modules/.bin"));
        assert!(card.contains("unity\\nforged approval"));
        assert!(!card.contains("\nforged approval"));
        assert!(!card.contains('\u{1b}'));
        assert!(card.contains("reference\\nforged-value"));
        assert!(!card.contains("\nforged-value"));
        for path in &preview.hardlink_pair {
            assert!(card.contains(path.to_str().unwrap()));
        }
    }
}

/// Fetches once, pins, and says what was pinned. **Registration is still a
/// separate act**: this only turns a command that fetches at every launch
/// into a file that does not, so the approval that follows has a digest and a
/// version to show.
#[cfg(not(unix))]
fn materialize(
    _server: &str,
    _runtime: Option<&std::path::Path>,
    _confirmed: bool,
) -> Result<(), String> {
    Err("このプラットフォームではMCPの取得に必要な保護を確認できません。".into())
}

#[cfg(unix)]
fn materialize(
    server: &str,
    runtime: Option<&std::path::Path>,
    confirmed: bool,
) -> Result<(), String> {
    if !confirmed && !io::stdin().is_terminal() {
        return Err("取得の確認には対話端末が必要です。--yes でも進められます。".into());
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOMEを読めません")?;
    println!(
        "{server} のパッケージを1度だけ取得して固定します。\n         取得: npm または PyPI（いずれも導入時のcodeは実行しません）\n         置き場所: ~/.polaris/mcp/servers/ のこのサーバー専用ディレクトリ\n         実行に使うinterpreter: {}\n         ネットワーク: 使います\n         この操作は登録ではありません。登録は取得後に別途確認します。",
        match runtime {
            Some(held) => held.display().to_string(),
            None => "取得の形式から決めます".to_owned(),
        }
    );
    if !confirmed {
        print!("続けますか? [y/N] ");
        use std::io::Write;
        let _ = io::stdout().flush();
        let mut answer = String::new();
        io::stdin()
            .lock()
            .read_line(&mut answer)
            .map_err(|_| "応答を読めません".to_owned())?;
        if !matches!(answer.trim(), "y" | "Y") {
            return Err("取得しませんでした。".into());
        }
    }
    let detector = polaris_core::mcp::setup::Detector::new(home, search_path());
    match detector.materialise(server, runtime) {
        Ok(pinned) => {
            println!(
                "{} {} を固定しました。\n実行: {} {}\n次に `polaris mcp setup` で登録します。",
                pinned.package,
                pinned.version,
                pinned.runtime.display(),
                pinned.entry.display()
            );
            Ok(())
        }
        Err(reason) => Err(refused(reason).to_owned()),
    }
}

/// The directories a command name is resolved against. The environment's own
/// `PATH` is deliberately not used: a version manager rewrites it, and the
/// interpreter that resolves through one is not the interpreter that will be
/// there next time.
#[cfg(unix)]
fn search_path() -> Vec<PathBuf> {
    ["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin", "/bin"]
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

async fn check(host: &LocalHost, server: &str) -> Result<String, String> {
    let fail = |error: Error| message(error).to_owned();
    let store = host.store().ok_or_else(|| fail(Error::Storage))?;
    let (mut runtime, quarantined) = host
        .runtime("mcp-check", "check", ExternalCeiling::Denied)
        .map_err(fail)?
        .ok_or("このプロジェクトで有効なサーバーがありません。")?;
    if let Some(reason) = quarantined.get(server) {
        return Err(message(*reason).to_owned());
    }
    let entry = runtime.index_entry(server, std::future::pending()).await;
    let stopped = runtime.shutdown().await;
    let entry = entry.map_err(|error| match error {
        polaris_core::mcp::runtime::Error::Transport(error) => message(error).to_owned(),
        // The other two variants carry their own text. Dropping it left a
        // person who had already answered the approval with "cannot be
        // confirmed" and nowhere to look — the worst shape a failure can
        // take, because the permission was already spent.
        other => format!("カタログを確認できません（{other}）。"),
    })?;
    let (tools, partial) = store.save_catalog_entry(server, entry).map_err(fail)?;
    stopped.map_err(fail)?;
    Ok(format!(
        "{server}: ツール{tools}件{}",
        if partial {
            "（上限のため一部）"
        } else {
            ""
        }
    ))
}

/// Before a one-shot run exits after a registration: this project's servers
/// without a catalog index entry. Failures stay unchecked for the next start.
pub async fn check_unchecked(host: &LocalHost) {
    let Some(store) = host.store() else {
        return;
    };
    let Ok(snapshot) = store.load_snapshot() else {
        return;
    };
    let Some(registry) = &snapshot.registry else {
        return;
    };
    let indexed = store
        .load_catalog_index()
        .ok()
        .flatten()
        .map(|index| index.verified(registry))
        .unwrap_or_default();
    for server in registry.servers.iter().filter(|server| {
        server.enabled_projects.contains(host.project()) && !indexed.contains_key(&server.server_id)
    }) {
        match check(host, &server.server_id).await {
            Ok(line) => eprintln!("{line}"),
            Err(text) => eprintln!(
                "{}: {text} 次の実行でも未確認として扱います。",
                server.server_id
            ),
        }
    }
}

fn toggle(
    host: &LocalHost,
    cwd: &std::path::Path,
    server_id: &str,
    enable: bool,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        use polaris_core::{
            management_effect::{ManagementEffectStoreV1, mcp_change::McpRegistryChangeOwnerV1},
            project_mutation::ProjectMutationOwnerV1,
        };
        let root = polaris_core::project::resolve_root(cwd)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let project = root.as_path();
        let state = polaris_core::project::state_dir(project).map_err(|e| e.to_string())?;
        let mutation = ProjectMutationOwnerV1::for_project(project).map_err(|e| e.to_string())?;
        let lease = mutation.try_mutation().map_err(|e| e.to_string())?;
        let effects = ManagementEffectStoreV1::open_at_state_dir(project, &state, &lease)
            .map_err(|e| e.to_string())?;
        let owner = McpRegistryChangeOwnerV1::new(host, &effects, &lease, project)?;
        let prepared = owner.prepare(server_id, enable)?;
        owner.apply(&prepared, true)?;
        println!(
            "{server_id} をこのプロジェクトで{}にしました。次の実行から反映します。",
            if enable { "有効" } else { "無効" }
        );
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let fail = |error: Error| message(error).to_owned();
        let store = host.store().ok_or_else(|| fail(Error::Storage))?;
        let mut registry = store
            .load_registry()
            .map_err(fail)?
            .ok_or("登録済みのサーバーはありません。")?;
        let server = registry
            .servers
            .iter_mut()
            .find(|server| server.server_id == server_id)
            .ok_or("そのサーバーは登録されていません。")?;
        if enable {
            server.enabled_projects.insert(host.project().to_owned());
            host.validate(server).map_err(fail)?;
        } else {
            server.enabled_projects.remove(host.project());
        }
        let expected = registry.revision;
        registry.revision += 1;
        store.save_registry(expected, &registry).map_err(fail)?;
        println!(
            "{server_id} をこのプロジェクトで{}にしました。次の実行から反映します。",
            if enable { "有効" } else { "無効" }
        );
        Ok(())
    }
}

/// Why a registration that was approved did not happen. The tool hands back
/// `{"error": code}`; this is the sentence for each code.
fn failure(body: &str) -> &'static str {
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|held| held.get("error")?.as_str().map(str::to_owned));
    match code.as_deref() {
        Some("denied") => "確認で許可されませんでした。",
        Some("credential_changed") => {
            "設定の認証情報が読み取り時から変わっています。もう一度実行してください。"
        }
        Some("unsafe_location") => "実行物か作業場所が安全に使えない場所にあります。",
        Some("registry_changed") => "登録内容が同時に変更されました。もう一度実行してください。",
        Some("invalid") => {
            "登録内容が受け付けられません。作業場所か実行物の条件を満たしていません。"
        }
        Some("stale") => "検出した内容が古くなっています。もう一度実行してください。",
        _ => "登録を保存できませんでした。",
    }
}

fn reason(code: &str) -> &'static str {
    match code {
        "launcher_not_supported" => "起動のたびにパッケージを取得する形式のため取り込めません",
        "environment_reference" => "環境変数の名前だけが書かれていて、値が見つかりません",
        "credential_in_arguments" => "引数やURLに認証情報らしき値があります",
        "unsupported_transport" => "対応していない接続方式です",
        "unsupported_auth" => "対応していない認証方式です",
        "command_not_found" => "コマンドが見つかりません",
        "command_not_absolute" => {
            "commandが相対パスです。何を起点にするかを決められないので、絶対パスで書いてください"
        }
        "command_missing" => "commandがないか、文字列ではありません",
        "argument_not_text" => "argsに文字列でない値があります",
        "url_missing" => "urlがないか、文字列ではありません",
        "url_not_usable" => "urlの形式を扱えません",
        "unsupported_install_location" => "導入場所が共有の場所やhome直下のため取り込めません",
        "already_registered" => "同じ内容で登録済みです",
        _ => "設定を読み取れません",
    }
}

/// The explicit subcommand is the external permission; each registration
/// still shows its full card and needs a terminal answer.
async fn setup(host: &LocalHost, confirmed: bool) -> Result<(), String> {
    if !confirmed && !io::stdin().is_terminal() {
        return Err("登録の確認には対話端末が必要です。-y でも進められます。".into());
    }
    let tool = host.setup_tool(ExternalCeiling::Allowed);
    let mut gate = Gate::new(ApprovalPolicy::Always);
    // The gate, the ceiling and every check below it run exactly as they do
    // when a person answers. What `-y` supplies is the answer, not a way
    // around the question: the card is still printed for every server, so
    // the record of what was approved is the same either way.
    let mut bulk = BulkApprover;
    let mut terminal = crate::TerminalApprover;
    let approver: &mut dyn Approver = if confirmed { &mut bulk } else { &mut terminal };
    let detected = tool
        .call(
            &serde_json::json!({"op":"detect"}),
            std::future::pending(),
            &mut gate,
            approver,
        )
        .await
        .map_err(|_| "設定を検出できません。".to_owned())?;
    let detected: serde_json::Value =
        serde_json::from_str(&detected).map_err(|_| "設定を検出できません。".to_owned())?;
    let candidates = detected["candidates"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if candidates.is_empty() {
        println!("CodexとClaudeの設定にMCPサーバーは見つかりませんでした。");
    }
    for candidate in candidates {
        let name = candidate["name"].as_str().unwrap_or("-");
        if let Some(code) = candidate["reason"].as_str() {
            println!("{name}: {}", reason(code));
            continue;
        }
        let result = tool
            .call(
                &serde_json::json!({"op":"register","candidate":candidate["candidate"]}),
                std::future::pending(),
                &mut gate,
                approver,
            )
            .await;
        match result {
            Ok(_) => println!("{name}: 登録しました。次の実行から使えます。"),
            // Taking the person's approval and then refusing without saying
            // why leaves them with nothing to act on. The code is already in
            // hand; only the printing was missing.
            Err(text) => println!("{name}: 登録しませんでした。{}", failure(&text)),
        }
    }
    Ok(())
}
