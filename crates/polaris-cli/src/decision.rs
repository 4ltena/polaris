//! Explicit local decision management, separate from agent/provider startup.
use polaris_core::managed_decision;
use std::{
    io::{self, BufRead, IsTerminal, Read, Write},
    path::PathBuf,
    process::ExitCode,
};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Show registered profiles and capacity recommendations without installation, authentication, network access, or inference.
    List,
    /// Verify every SHA in the installed pinned generation without launching, activating, or copying it.
    Verify {
        profile: String,
        /// Internal result channel for the packaged GUI; status is in the frame.
        #[arg(long, hide = true)]
        stdio_result: bool,
    },
    /// Show an explicit installation plan as JSON without downloading or copying files.
    Plan {
        #[command(flatten)]
        sources: Sources,
        /// Plan a repair while retaining the existing generation.
        #[arg(long)]
        repair: bool,
    },
    /// Show a download plan for pinned sources without network access or copying.
    PlanDownload {
        profile: String,
        #[arg(long)]
        repair: bool,
    },
    /// Download from pinned sources and install after SHA verification; no inference.
    Download {
        profile: String,
        #[arg(long)]
        repair: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long, hide = true, conflicts_with = "yes")]
        stdio_confirm: bool,
    },
    /// Copy confirmed pinned artifacts without changing backend settings or starting inference.
    Import {
        #[command(flatten)]
        sources: Sources,
        /// Explicitly approve installation using the displayed plan.
        #[arg(long)]
        yes: bool,
        /// Internal same-process confirmation channel for the packaged GUI.
        #[arg(long, hide = true, conflicts_with = "yes")]
        stdio_confirm: bool,
    },
    /// Repair using pinned artifacts; retain the old generation at the displayed recovery location.
    Repair {
        #[command(flatten)]
        sources: Sources,
        #[arg(long)]
        yes: bool,
        #[arg(long, hide = true, conflicts_with = "yes")]
        stdio_confirm: bool,
    },
}

#[derive(clap::Args)]
struct Sources {
    profile: String,
    /// Absolute worker path to verify against the pinned SHA.
    #[arg(long)]
    worker: PathBuf,
    /// Absolute model path to verify against the pinned SHA.
    #[arg(long)]
    model: PathBuf,
}

pub async fn run(args: Args) -> ExitCode {
    match perform(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn perform(args: Args) -> Result<(), String> {
    let (sources, import, repair) = match args.command {
        Command::List => {
            let registered = managed_decision::registered_profiles().map_err(|e| e.to_string())?;
            let observation = polaris_core::decision_capacity::observation::host_memory();
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "schema_version":1,
                "registered_profiles":registered,
                "capacity":polaris_core::decision_capacity::observation::diagnostic_lines(&observation),
                "note":"Registration does not verify installation or execution availability. Jev/SemIf settings remain unchanged."
            })).map_err(|e| e.to_string())?);
            return Ok(());
        }
        Command::Verify {
            profile,
            stdio_result,
        } => return verify(profile, stdio_result).await,
        Command::PlanDownload { profile, repair } => return download(profile, repair, None).await,
        Command::Download {
            profile,
            repair,
            yes,
            stdio_confirm,
        } => return download(profile, repair, Some((yes, stdio_confirm))).await,
        Command::Plan { sources, repair } => (sources, None, repair),
        Command::Import {
            sources,
            yes,
            stdio_confirm,
        } => (sources, Some((yes, stdio_confirm)), false),
        Command::Repair {
            sources,
            yes,
            stdio_confirm,
        } => (sources, Some((yes, stdio_confirm)), true),
    };
    let plan = if repair {
        managed_decision::plan_repair_local(&sources.profile, &sources.worker, &sources.model)
    } else {
        managed_decision::plan_local(&sources.profile, &sources.worker, &sources.model)
    }
    .map_err(|e| e.to_string())?;
    let backup = plan.recovery_backup().map(|p| p.to_owned());
    let consent = plan.consent_sha256().map_err(|e| e.to_string())?;
    let frame =
        serde_json::json!({"schema_version":1,"type":"plan","consent_sha256":consent,"plan":&plan});
    let stdio = import.is_some_and(|(_, stdio)| stdio);
    if !approve(
        &frame,
        &consent,
        import,
        if repair { "repaired" } else { "installed" },
    )
    .await?
    {
        return Ok(());
    }
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = cancelled.clone();
    let execution_consent = consent.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        managed_decision::install_local(plan, &execution_consent, || {
            flag.load(std::sync::atomic::Ordering::Acquire)
        })
    });
    let result = tokio::select! {
        result = &mut task => result,
        signal = tokio::signal::ctrl_c() => {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            let result = task.await;
            signal.map_err(|e| e.to_string())?;
            result
        }
    };
    result
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    if stdio {
        println!(
            "{}",
            serde_json::json!({"schema_version":1,"type":"complete","status":"installed","consent_sha256":consent})
        );
    } else {
        println!(
            "Clef generation {}. Backend selection and activation remain unchanged.",
            if repair { "repaired" } else { "installed" }
        );
        if let Some(backup) = backup {
            println!("Previous generation retained at: {}", backup.display());
        }
    }
    Ok(())
}

async fn approve(
    frame: &serde_json::Value,
    consent: &str,
    execution: Option<(bool, bool)>,
    action: &str,
) -> Result<bool, String> {
    let stdio = execution.is_some_and(|(_, stdio)| stdio);
    println!(
        "{}",
        if stdio {
            serde_json::to_string(&frame)
        } else {
            serde_json::to_string_pretty(&frame)
        }
        .map_err(|e| e.to_string())?
    );
    io::stdout().flush().map_err(|e| e.to_string())?;
    let Some((yes, stdio)) = execution else {
        return Ok(false);
    };
    if stdio {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let expected = consent.to_owned();
        std::thread::spawn(move || {
            let _ = sender.send(decode_confirmation(io::stdin().lock(), &expected));
        });
        let approved = tokio::task::spawn_blocking(move || {
            receiver.recv_timeout(std::time::Duration::from_secs(120))
        })
        .await
        .map_err(|_| "Could not stop waiting for installation confirmation")?
        .map_err(|_| "Installation confirmation timed out. No changes were made.")??;
        if !approved {
            println!(
                "{}",
                serde_json::json!({"schema_version":1,"type":"complete","status":"cancelled","consent_sha256":consent})
            );
            return Ok(false);
        }
    } else if !yes {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err("Installation confirmation requires an interactive terminal or explicit --yes for this selection. No changes were made.".into());
        }
        eprint!("Proceed with {} using the displayed plan? [y/n]: ", action);
        io::stderr().flush().map_err(|e| e.to_string())?;
        let mut answer = String::new();
        io::stdin()
            .lock()
            .take(16)
            .read_line(&mut answer)
            .map_err(|e| e.to_string())?;
        if answer.trim() != "y" {
            eprintln!("Clef installation cancelled. No changes were made.");
            return Ok(false);
        }
    }
    Ok(true)
}

async fn download(
    profile: String,
    repair: bool,
    execution: Option<(bool, bool)>,
) -> Result<(), String> {
    let plan = managed_decision::plan_download(&profile, repair).map_err(|e| e.to_string())?;
    let consent = plan.consent_sha256().map_err(|e| e.to_string())?;
    let backup = plan.recovery_backup().map(|p| p.to_owned());
    let frame =
        serde_json::json!({"schema_version":1,"type":"plan","consent_sha256":consent,"plan":&plan});
    if !approve(
        &frame,
        &consent,
        execution,
        if repair {
            "download and repair"
        } else {
            "download and install"
        },
    )
    .await?
    {
        return Ok(());
    }
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut task = std::pin::pin!(managed_decision::download_local(
        plan,
        &consent,
        flag.clone()
    ));
    let result = tokio::select! {
        result = &mut task => result,
        signal = tokio::signal::ctrl_c() => {
            flag.store(true, std::sync::atomic::Ordering::Release);
            let result = task.await;
            signal.map_err(|e| e.to_string())?;
            result
        }
    };
    result.map_err(|e| e.to_string())?;
    if execution.is_some_and(|(_, stdio)| stdio) {
        println!(
            "{}",
            serde_json::json!({"schema_version":1,"type":"complete","status":"installed","consent_sha256":consent})
        );
    } else {
        println!(
            "Downloaded and verified the pinned artifacts and installed the generation. Backend selection and activation remain unchanged."
        );
        if let Some(backup) = backup {
            println!("Previous generation retained at: {}", backup.display());
        }
    }
    Ok(())
}

async fn verify(profile: String, stdio_result: bool) -> Result<(), String> {
    let selected = profile.clone();
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = cancelled.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        managed_decision::verify_installed(&profile, || {
            flag.load(std::sync::atomic::Ordering::Acquire)
        })
    });
    let result = tokio::select! {
        result=&mut task => result,
        signal=tokio::signal::ctrl_c() => {
            cancelled.store(true,std::sync::atomic::Ordering::Release);
            let result=task.await;
            signal.map_err(|error| error.to_string())?;
            result
        }
    };
    let result = result.map_err(|error| error.to_string())?;
    if stdio_result {
        let (status, generation) = match result {
            Ok(generation) => ("verified", Some(generation)),
            Err(error) => (verification_failure_status(error), None),
        };
        println!("{}",serde_json::to_string(&serde_json::json!({"schema_version":1,"type":"verification","profile":selected,"status":status,"generation":generation})).map_err(|error|error.to_string())?);
    } else {
        let verified = result.map_err(|error| error.to_string())?;
        println!("{}",serde_json::to_string_pretty(&serde_json::json!({"schema_version":1,"type":"verified","generation":verified,"note":"Verified the pinned SHA at read time. No backend selection, activation, inference, or capacity assessment was performed. Future replacement or execution availability is not guaranteed."})).map_err(|error|error.to_string())?);
    }
    Ok(())
}

fn verification_failure_status(error: managed_decision::ManagedDecisionError) -> &'static str {
    use managed_decision::ManagedDecisionError::*;
    match error {
        NotInstalled => "not_installed",
        UnverifiedProfile => "unverified_profile",
        Integrity => "integrity",
        UnsupportedPlatform => "unsupported_platform",
        UnsupportedDevice => "unsupported_device",
        Cancelled => "cancelled",
        _ => "unavailable",
    }
}

fn decode_confirmation(reader: impl Read, expected: &str) -> Result<bool, String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Confirmation {
        schema_version: u32,
        consent_sha256: String,
        decision: String,
    }
    let mut bytes = Vec::new();
    io::BufReader::new(reader)
        .take(4097)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| "Could not read installation confirmation")?;
    if bytes.len() > 4096 || bytes.last() != Some(&b'\n') {
        return Err("Invalid installation confirmation frame. No changes were made.".into());
    }
    let frame: Confirmation = serde_json::from_slice(&bytes)
        .map_err(|_| "Invalid installation confirmation format. No changes were made.")?;
    if frame.schema_version != 1 || frame.consent_sha256 != expected {
        return Err(
            "Installation confirmation does not match this plan. No changes were made.".into(),
        );
    }
    match frame.decision.as_str() {
        "approve" => Ok(true),
        "cancel" => Ok(false),
        _ => Err("Unknown installation confirmation response. No changes were made.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_an_exact_bounded_confirmation_can_approve_the_live_plan() {
        let hash = "a".repeat(64);
        for (choice, expected) in [("approve", true), ("cancel", false)] {
            let frame = format!(
                "{{\"schema_version\":1,\"consent_sha256\":\"{hash}\",\"decision\":\"{choice}\"}}\n"
            );
            assert_eq!(
                decode_confirmation(frame.as_bytes(), &hash).unwrap(),
                expected
            );
            assert!(decode_confirmation(frame.as_bytes(), &"b".repeat(64)).is_err());
        }
        for bad in ["{}\n", "{}", "", "{\"schema_version\":2}\n"] {
            assert!(decode_confirmation(bad.as_bytes(), &hash).is_err());
        }
        assert!(decode_confirmation(&vec![b' '; 4097][..], &hash).is_err());
        let extra = format!(
            "{{\"schema_version\":1,\"consent_sha256\":\"{hash}\",\"decision\":\"approve\",\"other\":true}}\n"
        );
        assert!(decode_confirmation(extra.as_bytes(), &hash).is_err());
    }
}
