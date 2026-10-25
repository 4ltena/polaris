//! Direct environment setup: parsing and presentation only. No model is used.
use std::{path::PathBuf, process::ExitCode};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(clap::Subcommand)]
enum Command {
    /// Provision a fixed environment after reviewing one exact plan.
    Set(Selection),
    /// Inspect a fixed environment plan without downloading or installing.
    Plan(Selection),
    /// List supported definitions and local verification status.
    Status {
        #[arg(long)]
        operation: Option<PathBuf>,
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Reconcile a fully verified retained operation without repeating effects.
    Recover {
        #[arg(long)]
        operation: PathBuf,
        #[arg(long)]
        path: Option<PathBuf>,
    },
}
#[derive(clap::Args)]
struct Selection {
    /// node, nextjs, react, vue, svelte, astro, nuxt, angular, python, rust, or go.
    profile: String,
    /// Existing target directory; defaults to the current directory.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Exact release or a profile-supported version constraint.
    #[arg(long)]
    version: Option<String>,
    /// Reuse an unstarted saved plan from env plan.
    #[arg(long)]
    plan_directory: Option<PathBuf>,
    /// Approve the exact saved plan SHA-256 in noninteractive use.
    #[arg(long, requires = "plan_directory")]
    approve_plan: Option<String>,
}

pub async fn run(
    args: Args,
    policy: polaris_core::approval::ApprovalPolicy,
    sandbox: polaris_sandbox::SandboxMode,
) -> ExitCode {
    #[cfg(target_os = "macos")]
    let result = execute(args, policy, sandbox).await;
    #[cfg(not(target_os = "macos"))]
    let result: Result<(), String> = {
        let _ = (args, policy, sandbox);
        Err("Environment setup currently supports macOS arm64".into())
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "macos")]
async fn execute(
    args: Args,
    policy: polaris_core::approval::ApprovalPolicy,
    sandbox: polaris_sandbox::SandboxMode,
) -> Result<(), String> {
    use polaris_core::environment::{
        native,
        owner::{EffectContext, Lease, Prepared},
    };
    use polaris_core::{
        management_effect::ManagementEffectStoreV1, project_mutation::ProjectMutationOwnerV1,
    };
    use polaris_envkit::{
        catalog::ALL,
        request::{Operation, Request},
    };
    use std::{
        io::{self, IsTerminal, Write},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let signal_cancel = Arc::clone(&cancel);
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.store(true, Ordering::Release);
        }
    });
    if sandbox == polaris_sandbox::SandboxMode::ReadOnly && matches!(args.command, Command::Set(_))
    {
        return Err("Environment setup is forbidden by the read-only parent sandbox".into());
    }
    if policy == polaris_core::approval::ApprovalPolicy::Never
        && matches!(args.command, Command::Set(_) | Command::Recover { .. })
    {
        return Err("Environment setup or recovery is forbidden by --approval never".into());
    }
    if let Command::Status {
        operation: Some(operation),
        path,
    }
    | Command::Recover { operation, path } = &args.command
    {
        let project = path
            .clone()
            .unwrap_or(std::env::current_dir().map_err(|error| error.to_string())?)
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let project = polaris_core::project::resolve_root(&project);
        let report = if matches!(args.command, Command::Recover { .. }) {
            let owner =
                ProjectMutationOwnerV1::for_project(&project).map_err(|error| error.to_string())?;
            let lease = owner.try_mutation().map_err(|error| error.to_string())?;
            let effects = ManagementEffectStoreV1::open(&project, &lease)
                .map_err(|error| error.to_string())?;
            polaris_core::environment::recovery::reconcile(
                &project,
                effects.state_dir(),
                operation,
                &effects,
                &lease,
            )?
        } else {
            let state =
                polaris_core::project::state_path(&project).map_err(|error| error.to_string())?;
            polaris_core::environment::recovery::inspect(&project, &state, operation)?
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    if matches!(args.command, Command::Status { .. }) {
        for profile in ALL {
            let definition =
                polaris_envkit::bundled::load(profile).map_err(|error| error.to_string())?;
            println!(
                "{}\tavailable\tunverified\t{}",
                profile.entry().name,
                definition
                    .candidates
                    .iter()
                    .map(|candidate| candidate.version.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        println!(
            "Verification is operation-specific. Inspect the retained receipt for the exact binary, profile and target."
        );
        return Ok(());
    }
    let (selection, plan_only) = match args.command {
        Command::Set(selection) => (selection, false),
        Command::Plan(selection) => (selection, true),
        Command::Status { .. } | Command::Recover { .. } => unreachable!(),
    };
    let profile = serde_json::from_value(serde_json::Value::String(selection.profile))
        .map_err(|_| "Unsupported environment profile")?;
    let target = selection
        .path
        .unwrap_or(std::env::current_dir().map_err(|error| error.to_string())?)
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let request = Request {
        operation: Operation::Set,
        profile,
        target: Some(
            target
                .to_str()
                .ok_or("Invalid environment target path")?
                .into(),
        ),
        version: selection.version,
    };
    request.validate().map_err(|error| error.to_string())?;
    let project = polaris_core::project::resolve_root(&target);
    let mutation =
        ProjectMutationOwnerV1::for_project(&project).map_err(|error| error.to_string())?;
    let lease = mutation.try_mutation().map_err(|error| error.to_string())?;
    let effects =
        ManagementEffectStoreV1::open(&project, &lease).map_err(|error| error.to_string())?;
    polaris_core::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
        &project,
        effects.state_dir(),
        &lease,
    )
    .map_err(|error| error.to_string())?;
    let mut prepared = if let Some(root) = &selection.plan_directory {
        let saved = Prepared::load(&project, effects.state_dir(), root)?;
        if saved.plan.snapshot.target.canonical_path
            != target.to_str().ok_or("Invalid environment target path")?
            || saved.plan.profile != profile
            || saved.plan.normalized_version
                != request
                    .normalized_version()
                    .map_err(|error| error.to_string())?
        {
            return Err("Saved environment plan does not match this selection".into());
        }
        saved
    } else {
        Prepared::prepare(
            &project,
            effects.state_dir(),
            &request,
            native::capture_with_cancel(profile, || cancel.load(Ordering::Acquire))?,
        )?
    };
    println!("{}", prepared.description()?);
    if plan_only {
        return Ok(());
    }
    if let Some(accepted) = &selection.approve_plan {
        if *accepted != prepared.digest()? {
            return Err("Environment approval hash does not match the saved plan".into());
        }
    } else {
        if !io::stdin().is_terminal() {
            return Err(
                "Environment setup needs interactive plan-bound approval; no download started"
                    .into(),
            );
        }
        print!("Execute this exact plan? [y/N] ");
        io::stdout().flush().map_err(|error| error.to_string())?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .map_err(|error| error.to_string())?;
        if answer.trim() != "y" {
            println!("Environment setup cancelled");
            return Ok(());
        }
    }
    let digest = prepared.digest()?;
    let prerequisites = prepared.plan.snapshot.host_prerequisites.clone();
    let context = EffectContext {
        store: &effects,
        lease: Lease::Direct(&lease),
        session: None,
    };
    let result = prepared
        .execute(
            &context,
            &digest,
            || native::refresh(&prerequisites, || cancel.load(Ordering::Acquire)),
            || cancel.load(Ordering::Acquire),
        )
        .await;
    while prepared.cleanup_pending() {
        let _ = prepared.poll_cleanup();
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    signal.abort();
    let receipt = result?;
    println!(
        "Verified {} ({}). Receipt: {}",
        receipt.selected_version,
        receipt.plan_sha256,
        prepared.retained_root().join("receipt.json").display()
    );
    for runtime in receipt.runtime_paths {
        println!("Private runtime: {runtime}");
    }
    Ok(())
}
