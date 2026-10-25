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
    /// Target project; defaults to the current directory.
    #[arg(long, global = true)]
    project: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// List registered servers and credential reference names; credential values are hidden.
    List,
    /// Enable a server for this project.
    Enable { server: String },
    /// Disable a server for this project.
    Disable { server: String },
    /// Read credentials from a pipe and store them under a reference name.
    SecretSet { reference: String },
    /// Delete stored credentials.
    SecretDelete { reference: String },
    /// Connect to the server, verify its catalog, and update the index without using a model.
    Check { server: String },
    /// Discover servers in Codex and Claude configuration and register confirmed entries.
    Setup {
        /// Approve all entries instead of answering individually. This flag supplies confirmation; all details remain displayed. Per-tool-call confirmation is a separate boundary and is unchanged.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Download and pin a server package once instead of on every launch. It can then be registered as a local executable.
    Materialize {
        /// Server name in the detected configuration.
        server: String,
        /// Runtime interpreter. Defaults to node for npm packages or uv-managed Python for PyPI packages.
        #[arg(long)]
        runtime: Option<PathBuf>,
        /// Explicitly confirm the displayed download without an interactive answer. Downloads involve third-party code; either interactive confirmation or this flag is required. Intended for batch scripts.
        #[arg(long)]
        yes: bool,
    },
    /// Deploy a pinned runtime for an existing Unity MCP registration and update its next-launch configuration.
    Configure {
        server: String,
        #[arg(long, value_parser = ["unity-local-v2"])]
        preset: String,
        #[arg(long)]
        source: PathBuf,
        /// Approve the displayed configuration. Does not check connections or execute tools.
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
        eprintln!("Allow this? [y/N] y  (approved by --yes)");
        Decision::Allow
    }
}

fn refused(reason: polaris_core::mcp::setup::launcher::Refused) -> &'static str {
    use polaris_core::mcp::setup::launcher::Refused;
    match reason {
        Refused::NotALauncher => {
            "No configuration with that name downloads a package on every launch."
        }
        Refused::ManagerNotSupported => "This retrieval method is not yet supported (npm only).",
        Refused::FetchFailed => "Download failed. Check the network and package name.",
        Refused::NoEntryPoint => {
            "Downloaded, but the package has no entry point or points outside its storage location."
        }
        Refused::RuntimeNotPinned => "The specified interpreter was not found.",
        Refused::NotSaved => "Cannot save the pinned content.",
    }
}

fn message(error: Error) -> &'static str {
    match error {
        Error::RevisionConflict => "Configuration changed concurrently. Run the command again.",
        Error::InvalidRegistration | Error::Limit => {
            "Registration is invalid or exceeds the limit."
        }
        Error::Auth => "Credentials are missing or still referenced.",
        Error::Protection => "Contains locations or files that cannot be used safely.",
        Error::Transport | Error::Timeout | Error::Protocol => "Cannot connect to the server.",
        Error::StopUnconfirmed => "Cannot verify that the server stopped.",
        _ => "Cannot read or write MCP configuration.",
    }
}

pub async fn run(args: Args) -> ExitCode {
    if cfg!(windows) {
        eprintln!("Protected MCP storage is unavailable on Windows. No changes were made.");
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
                println!("No registered servers. Discover them with `polaris mcp setup`.");
                return Ok(());
            };
            println!("Registry revision {}", registry.revision);
            for server in &registry.servers {
                let destination = match &server.transport {
                    Destination::StreamableHttp { url, .. } => url.clone(),
                    Destination::Stdio { command, .. } => command.display().to_string(),
                };
                let enabled = server.enabled_projects.contains(host.project());
                let checked = indexed
                    .as_ref()
                    .and_then(|index| index.get(&server.server_id))
                    .map_or("catalog unverified".to_owned(), |entry| {
                        format!("{} tools", entry.tools.len())
                    });
                println!(
                    "{}\t{}\t{}\t{}",
                    server.server_id,
                    if enabled { "enabled" } else { "disabled" },
                    checked,
                    destination
                );
            }
            if let Some(secrets) = snapshot.secrets {
                let names: Vec<_> = secrets.references().collect();
                println!(
                    "Credential references (revision {}): {}",
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
                return Err(
                    "Pipe the value into this command; typing in a terminal would expose it."
                        .into(),
                );
            }
            let mut value = String::new();
            io::stdin()
                .lock()
                .read_line(&mut value)
                .map_err(|_| "Cannot read credentials.".to_owned())?;
            let value = value.trim_end_matches(['\r', '\n']);
            if value.is_empty() {
                return Err("Credentials are empty.".into());
            }
            let revision = store
                .load_secrets()
                .map_err(fail)?
                .map_or(0, |secrets| secrets.revision());
            store
                .update_secret(revision, &reference, Some(SecretValue::new(value)))
                .map_err(fail)?;
            println!("Saved {reference}. Available from the next run.");
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
            println!("Deleted {reference}.");
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
        Command::Configure { .. } => {
            Err("Configuration requires the dedicated read preparation path.".into())
        }
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
    Err("Cannot verify the protections required for MCP configuration on this platform.".into())
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
        return Err("Configuration confirmation requires an interactive terminal or --yes.".into());
    }
    if preset != "unity-local-v2" {
        return Err("Unsupported configuration preset.".into());
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
        return Err("Configuration cancelled.".into());
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
        "Configured {} (registry revision {}). Effective from the next run.\nDestination: {}\nCheck the connection with `polaris mcp check {}`.",
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
        "Pin Unity MCP deployment and update its registration".into(),
        format!(
            "preset: unity-local-v2 / server: {}",
            text(&preview.server_id)
        ),
        format!("project: {}", text(&preview.project_id)),
        format!("source: {}", path(&preview.source)),
        format!("node: {}", path(&preview.node)),
        format!("node SHA256: {}", text(&preview.node_sha256)),
        format!("entry: {}", path(&preview.entry)),
        format!("New entry: {}", path(&preview.new_cwd.join("build/index.js"))),
        format!("entry SHA256: {}", text(&preview.entry_sha256)),
        format!("manifest SHA256: {}", text(&preview.manifest_sha256)),
        format!(
            "Selected: {} files / {} bytes",
            preview.selected_files, preview.selected_bytes
        ),
        "Scope: build/, node_modules/, package.json, and existing lockfiles".into(),
        format!("Current cwd: {}", path(&preview.old_cwd)),
        format!("Destination and new cwd: {}", path(&preview.new_cwd)),
        format!(
            "protocol: {:?} / sandbox: {}",
            preview.protocol_profile,
            text(&preview.sandbox_profile)
        ),
        format!(
            "Registry revision: {} → {}",
            preview.registry_before_revision, preview.registry_after_revision
        ),
        "Registry changes: cwd, entry arguments, script pin, protocol, sandbox".into(),
        "Server ID, project set, capabilities, timeouts, and secret references are retained.".into(),
        "Bounded hardlink pair (9,817,266 bytes; same inode/nlink=2 copied into separate nlink=1 files):".into(),
        format!("pair SHA256: {}", text(&preview.hardlink_pair_sha256)),
    ];
    for entry in &preview.hardlink_pair {
        lines.push(format!("  {}", path(entry)));
    }
    lines.push("Fixed exclusions:".into());
    for entry in &preview.fixed_exclusions {
        lines.push(format!("  {}", path(entry)));
    }
    lines.push("Retained secret reference names (values are hidden):".into());
    if preview.secret_references.is_empty() {
        lines.push("  none".into());
    } else {
        for reference in &preview.secret_references {
            lines.push(format!("  {}", text(reference)));
        }
    }
    lines.push("Network scope: loopback on this Mac only.".into());
    lines.push(
        "The C27 registry is updated after C28 deployment completes. The old runtime and unreferenced copies are retained.".into(),
    );
    lines.push(
        "Preparation and deployment do not launch programs, download, or check connections.".into(),
    );
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
    Err("Cannot verify the protections required for MCP downloads on this platform.".into())
}

#[cfg(unix)]
fn materialize(
    server: &str,
    runtime: Option<&std::path::Path>,
    confirmed: bool,
) -> Result<(), String> {
    if !confirmed && !io::stdin().is_terminal() {
        return Err("Download confirmation requires an interactive terminal or --yes.".into());
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("Cannot read HOME")?;
    println!(
        "Download and pin the package for {server} once.\n         Source: npm or PyPI (installation code is never executed)\n         Storage: a dedicated server directory under ~/.polaris/mcp/servers/\n         Runtime interpreter: {}\n         Network: used\n         This does not register the server. Registration is confirmed separately after download.",
        match runtime {
            Some(held) => held.display().to_string(),
            None => "Determined by the download format".to_owned(),
        }
    );
    if !confirmed {
        print!("Continue? [y/N] ");
        use std::io::Write;
        let _ = io::stdout().flush();
        let mut answer = String::new();
        io::stdin()
            .lock()
            .read_line(&mut answer)
            .map_err(|_| "Cannot read the response".to_owned())?;
        if !matches!(answer.trim(), "y" | "Y") {
            return Err("Download cancelled.".into());
        }
    }
    let detector = polaris_core::mcp::setup::Detector::new(home, search_path());
    match detector.materialise(server, runtime) {
        Ok(pinned) => {
            println!(
                "Pinned {} {}.\nRun: {} {}\nNext, register with `polaris mcp setup`.",
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
        .ok_or("No enabled servers for this project.")?;
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
        other => format!("Cannot verify the catalog ({other})."),
    })?;
    let (tools, partial) = store.save_catalog_entry(server, entry).map_err(fail)?;
    stopped.map_err(fail)?;
    Ok(format!(
        "{server}: {tools} tools{}",
        if partial {
            " (partial due to limit)"
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
                "{}: {text} It will remain unverified for the next run.",
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
            "{server_id} is now {} for this project. Effective from the next run.",
            if enable { "enabled" } else { "disabled" }
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
            .ok_or("No registered servers.")?;
        let server = registry
            .servers
            .iter_mut()
            .find(|server| server.server_id == server_id)
            .ok_or("That server is not registered.")?;
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
            "{server_id} is now {} for this project. Effective from the next run.",
            if enable { "enabled" } else { "disabled" }
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
        Some("denied") => "Confirmation was denied.",
        Some("credential_changed") => {
            "Configured credentials changed since they were read. Run the command again."
        }
        Some("unsafe_location") => "The executable or working directory cannot be used safely.",
        Some("registry_changed") => "The registry changed concurrently. Run the command again.",
        Some("invalid") => {
            "Registration refused. The working directory or executable does not meet requirements."
        }
        Some("stale") => "Detected configuration is stale. Run the command again.",
        _ => "Could not save the registration.",
    }
}

fn reason(code: &str) -> &'static str {
    match code {
        "launcher_not_supported" => {
            "Cannot import a configuration that downloads packages on every launch"
        }
        "environment_reference" => {
            "An environment variable name is specified, but its value is missing"
        }
        "credential_in_arguments" => "Arguments or the URL appear to contain credentials",
        "unsupported_transport" => "Unsupported transport",
        "unsupported_auth" => "Unsupported authentication method",
        "command_not_found" => "Command not found",
        "command_not_absolute" => {
            "The command is relative and has no defined base. Specify an absolute path"
        }
        "command_missing" => "The command is missing or is not a string",
        "argument_not_text" => "args contains a non-string value",
        "url_missing" => "The URL is missing or is not a string",
        "url_not_usable" => "Unsupported URL format",
        "unsupported_install_location" => {
            "Cannot import from a shared location or directly under home"
        }
        "already_registered" => "An identical registration already exists",
        _ => "Cannot read configuration",
    }
}

/// The explicit subcommand is the external permission; each registration
/// still shows its full card and needs a terminal answer.
async fn setup(host: &LocalHost, confirmed: bool) -> Result<(), String> {
    if !confirmed && !io::stdin().is_terminal() {
        return Err("Registration confirmation requires an interactive terminal or -y.".into());
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
        .map_err(|_| "Cannot detect configuration.".to_owned())?;
    let detected: serde_json::Value =
        serde_json::from_str(&detected).map_err(|_| "Cannot detect configuration.".to_owned())?;
    let candidates = detected["candidates"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if candidates.is_empty() {
        println!("No MCP servers found in Codex or Claude configuration.");
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
            Ok(_) => println!("{name}: registered. Available from the next run."),
            // Taking the person's approval and then refusing without saying
            // why leaves them with nothing to act on. The code is already in
            // hand; only the printing was missing.
            Err(text) => println!("{name}: not registered. {}", failure(&text)),
        }
    }
    Ok(())
}
