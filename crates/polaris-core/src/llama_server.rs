//! Supervising the bundled `llama-server`.
//!
//! The other two local runtimes are servers somebody else installed and
//! runs; polaris only talks to them. This one polaris starts, which is the
//! whole difference and the whole risk. Every rule below exists because
//! starting a process is not the same as connecting to one.
//!
//! The shape is `strict_provider::local_embedder`'s, because that is the
//! only other place in polaris that runs a pinned local program: an
//! absolute path from configuration and nowhere else, `env_clear`, a
//! private directory, a deadline, and a kill on drop.
//!
//! What is deliberately not here: downloading, `PATH` lookup, choosing a
//! model, and any default that would make this start without being asked.
//! A project that configures nothing gets nothing started.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use polaris_provider::local::{ApiToken, Endpoint, LocalAdapter, LocalError, Runtime};

/// How long to wait for the server to answer `/health` before giving up.
///
/// Router mode does not load a model at startup (`--no-models-autoload`),
/// so this covers process start and HTTP bring-up, not weights.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const READY_POLL: Duration = Duration::from_millis(100);

/// The most models the router may hold in memory at once.
///
/// llama-server's own default is 4. One is the honest number for a laptop
/// shared with an editor and a browser: a second resident model is memory
/// nobody asked for, and the router loads on demand anyway.
const MAX_RESIDENT: u32 = 1;

/// What a project must write down before anything starts.
///
/// Every field is required and there is no default. A half-configured
/// runtime is not started — a guess about which binary to execute is the
/// one guess this module must never make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlamaCppConfig {
    /// Absolute path to `llama-server`.
    pub server: PathBuf,
    /// Absolute path to the directory of `.gguf` files it serves.
    pub models_dir: PathBuf,
    /// The build this configuration was written against, as `/props`
    /// reports it (`build_info`, e.g. `b11026-b49650adb`).
    ///
    /// Checked after startup. A different build is a different program:
    /// the routes, the flags and the chat templates all move between
    /// releases, and silently talking to whichever one happens to be on
    /// disk is how a pinned runtime stops being pinned.
    pub build: String,
}

/// A running `llama-server`, owned by this value.
///
/// Dropping it kills the child. That is deliberate rather than tidy: an
/// orphaned server holds a loopback port and a model in memory, and the
/// next run would find a listener it did not start and cannot vouch for.
pub struct LlamaServer {
    child: tokio::process::Child,
    endpoint: Endpoint,
    token: ApiToken,
    /// The probe used for readiness and the build check. The same client,
    /// deadline and size cap the discovery side uses; a second HTTP client
    /// here would be a second set of limits to keep in step.
    probe: LocalAdapter,
    /// Deleted when this value drops, taking the key file with it.
    _private: tempfile::TempDir,
}

impl LlamaServer {
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The bearer token the server was started with. Discovery and
    /// inference both need it; nothing else on the machine has it.
    pub fn token(&self) -> &ApiToken {
        &self.token
    }

    /// Starts the configured server and waits until it answers.
    pub async fn start(config: &LlamaCppConfig, private_root: &Path) -> io::Result<Self> {
        let server = check_file(&config.server, "llama-serverの実行ファイル")?;
        let models = check_dir(&config.models_dir, "GGUFのディレクトリ")?;
        if config.build.trim().is_empty() {
            return Err(io::Error::other(
                "llama_cpp.buildに固定するビルド識別子が必要です",
            ));
        }
        if !private_root.is_absolute() {
            return Err(io::Error::other("保存先は絶対パスで指定してください"));
        }
        std::fs::create_dir_all(private_root)?;

        let private = tempfile::Builder::new()
            .prefix("llama-server-")
            .tempdir_in(private_root)?;
        let token = ApiToken::generate()?;
        // Through a file, not `--api-key`: an argv is readable by every
        // process of this user, and the token is the only thing standing
        // between the model and anything else running on the machine.
        let key_file = private.path().join("api-key");
        write_private(&key_file, token.as_str())?;
        let log = std::fs::File::create(private.path().join("server.log"))?;

        let port = free_loopback_port()?;
        let mut command = tokio::process::Command::new(&server);
        command
            .arg("--models-dir")
            .arg(&models)
            .arg("--no-models-autoload")
            .arg("--models-max")
            .arg(MAX_RESIDENT.to_string())
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .arg("--api-key-file")
            .arg(&key_file)
            .arg("--no-webui")
            .kill_on_drop(true)
            // llama-server reads an environment variable for **every**
            // flag (`LLAMA_ARG_MODELS_DIR` and the rest). Inheriting the
            // environment would let anything already exported override
            // what was just pinned above, including the models directory.
            .env_clear()
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("TMPDIR", private.path())
            .stdin(std::process::Stdio::null())
            // To a file, never a pipe. An unread pipe fills and the child
            // blocks on its own logging, which looks exactly like a hang.
            .stdout(log.try_clone()?)
            .stderr(log);

        let child = command.spawn()?;
        let endpoint = Endpoint::parse(&format!("http://127.0.0.1:{port}"))
            .map_err(|_| io::Error::other("起動先のendpointを組み立てられません"))?;
        let probe =
            LocalAdapter::with_token(Runtime::LlamaCpp, endpoint.clone(), Some(token.clone()))
                .map_err(|_| io::Error::other("llama-serverへの問い合わせ口を作成できません"))?;
        let mut held = Self {
            child,
            endpoint,
            token,
            probe,
            _private: private,
        };
        held.wait_until_ready().await?;
        held.check_build(&config.build).await?;
        Ok(held)
    }

    /// Polls `/health` until the server answers or the deadline passes.
    ///
    /// A child that exited is noticed here rather than waited out: the
    /// usual cause is a bad models directory, and twenty seconds of
    /// silence hides the message the server already wrote to its log.
    async fn wait_until_ready(&mut self) -> io::Result<()> {
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Err(io::Error::other(format!(
                    "llama-serverが起動直後に終了しました（{status}）"
                )));
            }
            if self.probe.health().await.is_ok() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::other("llama-serverが応答しませんでした"));
            }
            tokio::time::sleep(READY_POLL).await;
        }
    }

    /// Refuses a build other than the pinned one.
    async fn check_build(&self, pinned: &str) -> io::Result<()> {
        let value = self
            .probe
            .props()
            .await
            .map_err(|_| io::Error::other("llama-serverの/propsを読めません"))?;
        build_matches(&value, pinned)
    }

    /// Stops the server and waits for it to go.
    pub async fn shutdown(mut self) -> io::Result<()> {
        self.child.start_kill()?;
        self.child.wait().await?;
        Ok(())
    }
}

/// Whether `/props` describes the server this configuration pinned.
///
/// Separate from the request so the decision can be tested without
/// standing up a server: the two things that matter here are which build
/// answered and whether it is a router, and neither is about HTTP.
fn build_matches(value: &serde_json::Value, pinned: &str) -> io::Result<()> {
    let observed = value
        .get("build_info")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if observed != pinned {
        return Err(io::Error::other(format!(
            "llama-serverのビルドが固定値と一致しません（設定 {pinned} / 実際 {observed}）"
        )));
    }
    // Router mode is the contract the discovery side is written against.
    // A single-model server answers `/v1/models` with one entry and no
    // `status`, which reads as a runtime with one model of unknown
    // residency rather than as the misconfiguration it is.
    if value.get("role").and_then(serde_json::Value::as_str) != Some("router") {
        return Err(io::Error::other(
            "llama-serverがrouterとして起動していません",
        ));
    }
    Ok(())
}

/// A port nothing is listening on, learned by briefly holding it.
///
/// There is a window between releasing it and llama-server binding it. On
/// loopback it is small, and the alternative — `--port 0` and parsing the
/// port back out of the server's log — makes startup depend on a log
/// format that is not part of any contract. A failure here is loud: the
/// server exits and `wait_until_ready` reports it.
fn free_loopback_port() -> io::Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()
}

fn check_file(path: &Path, what: &str) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::other(format!(
            "{what}は絶対パスで指定してください"
        )));
    }
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(io::Error::other(format!(
            "{what}が通常のファイルではありません"
        )));
    }
    Ok(path.to_path_buf())
}

fn check_dir(path: &Path, what: &str) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::other(format!(
            "{what}は絶対パスで指定してください"
        )));
    }
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(io::Error::other(format!(
            "{what}がディレクトリではありません"
        )));
    }
    Ok(path.to_path_buf())
}

/// An adapter for the running server, carrying the token discovery needs.
///
/// A fresh one rather than the supervisor's own probe: discovery is used
/// by a different caller with a different lifetime, and sharing one would
/// tie the inventory's failures to the readiness check's.
pub fn adapter(server: &LlamaServer) -> Result<LocalAdapter, LocalError> {
    LocalAdapter::with_token(
        Runtime::LlamaCpp,
        server.endpoint().clone(),
        Some(server.token().clone()),
    )
}

#[cfg(test)]
mod tests;
