//! 試験用の匿名stdin/stdout入口。常に自身の新規TempDirを所有し、path引数を持たない。

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use polaris_core::desktop_store::PrototypeRoot;
    use polaris_desktop_service::{FakeService, Options};
    use std::os::fd::AsFd;
    use tokio::net::unix::pipe::{Receiver, Sender};
    if std::env::args_os().len() != 1 {
        return Err("引数は受け付けません".into());
    }
    let reader = Receiver::from_owned_fd(std::io::stdin().as_fd().try_clone_to_owned()?)?;
    let writer = Sender::from_owned_fd(std::io::stdout().as_fd().try_clone_to_owned()?)?;
    let root = PrototypeRoot::new()?;
    FakeService::create(&root, Options::default())?
        .serve(reader, writer)
        .await?;
    Ok(())
}

#[cfg(not(unix))]
fn main() {
    eprintln!("このOSのfake service pipeは未対応です");
    std::process::exit(1);
}
