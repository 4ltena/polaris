//! Native-owner launch. Isolate the inherited recovery socket before diagnostics.
#[cfg(unix)]
fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let recovery_requested = args.iter().any(|a| a == "--source-recovery-fd2");
    // fd2 is protocol data, including when argument parsing or setup fails.
    let recovery = if recovery_requested {
        match polaris_desktop_service::take_recovery_socket() {
            Ok(socket) => Some(socket),
            Err(_) => std::process::exit(1),
        }
    } else {
        None
    };
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let args = polaris_desktop_service::parse_launch_args(&args)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(run(args, recovery))
    })();
    if let Err(error) = result {
        if !recovery_requested {
            eprintln!("desktop serviceを開始または終了できませんでした");
        }
        std::process::exit(startup_exit_code(error.as_ref()));
    }
}
#[cfg(unix)]
fn startup_exit_code(error: &(dyn std::error::Error + 'static)) -> i32 {
    #[cfg(target_os = "macos")]
    if let Some(error) = error.downcast_ref::<polaris_desktop_service::StartupError>() {
        return error.exit_code();
    }
    let _ = error;
    1
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn startup_resource_exits_are_typed_and_do_not_expose_error_text() {
        use polaris_desktop_service::{MemoryResourceError as M, StartupError};
        for (error, code) in [
            (M::Configuration, 81),
            (M::Manifest, 84),
            (M::Readiness, 88),
        ] {
            assert_eq!(super::startup_exit_code(&StartupError::Memory(error)), code);
        }
        assert_eq!(
            super::startup_exit_code(&std::io::Error::other("untrusted upstream text")),
            1
        );
    }
}
#[cfg(unix)]
async fn run(
    args: polaris_desktop_service::LaunchArguments,
    recovery: Option<std::os::unix::net::UnixStream>,
) -> Result<(), Box<dyn std::error::Error>> {
    use polaris_desktop_service::{DesktopService, LaunchArguments};
    use std::os::fd::AsFd;
    use tokio::net::unix::pipe::{Receiver, Sender};
    let reader = Receiver::from_owned_fd(std::io::stdin().as_fd().try_clone_to_owned()?)?;
    let writer = Sender::from_owned_fd(std::io::stdout().as_fd().try_clone_to_owned()?)?;
    match args {
        LaunchArguments::Legacy { store, .. } => {
            let mut service =
                tokio::task::spawn_blocking(move || DesktopService::open(store)).await??;
            // Storage-only clients cannot obtain source recovery authority.
            drop(recovery);
            service.serve(reader, writer).await?;
        }
        LaunchArguments::OwnerV1(args) => {
            #[cfg(target_os = "macos")]
            {
                let mut owner =
                    tokio::task::spawn_blocking(move || polaris_desktop_service::start_owner(args))
                        .await??;
                let socket =
                    tokio::net::UnixStream::from_std(recovery.ok_or("recovery socket required")?)?;
                let handle = owner.service.attach_source_recovery()?;
                let (stop, stopped) = tokio::sync::oneshot::channel();
                let control = polaris_desktop_service::serve_source_recovery_draining(
                    socket,
                    handle,
                    async {
                        let _ = stopped.await;
                    },
                );
                let main = async {
                    let result = owner.service.serve(reader, writer).await;
                    let _ = stop.send(());
                    result
                };
                // A failed control channel must not drop an active engine. Wait
                // for the owner and any accepted result-save ACK before exit.
                let (main_result, control_result) = tokio::join!(main, control);
                main_result?;
                control_result?;
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = args;
                return Err("このOSのowner実行は未対応です".into());
            }
        }
    }
    Ok(())
}
#[cfg(not(unix))]
fn main() {
    eprintln!("このOSのdesktop service pipeは未対応です");
    std::process::exit(1);
}
