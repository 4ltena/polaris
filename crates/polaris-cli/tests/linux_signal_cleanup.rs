//! Real Linux PTY evidence for interactive shutdown. Every child is the
//! Cargo-built `polaris` binary, with a fresh private home, state root and
//! project. The raw PTY and result record remain under /tmp after the test.
#![cfg(target_os = "linux")]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const RAW_LIMIT: usize = 1024 * 1024;
const SCREEN_DEADLINE: Duration = Duration::from_secs(10);
const NORMAL_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);
const DIFF_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(4);
const ONBOARDING_MARKER: &str = "Welcome to polaris, an improved harness built on codex";
const IDLE_MARKER: &str = "Ask polaris to do anything";
// Ratatui may split one displayed sentence across cursor-positioning escapes.
// These unique state markers are contiguous in the emitted PTY bytes.
const PICKER_MARKER: &str = "approval preference within startup limits";
const CTRL_C_MARKER: &str = "Ctrl+C";
const INFLIGHT_PROMPT: &str = "POLARIS_SIGNAL_PTY_INFLIGHT_ONLY_928";
const DIFF_SENTINEL: &str = "POLARIS_SIGNAL_PTY_DIFF_RESULT_928";

#[derive(Clone, Copy, Debug)]
enum Scene {
    Onboarding,
    Main,
}

impl Scene {
    fn needs_credential(self) -> bool {
        matches!(self, Self::Main)
    }
}

#[derive(Clone, Copy, Debug)]
struct TermiosEvidence {
    iflag: u32,
    oflag: u32,
    cflag: u32,
    lflag: u32,
    canonical: bool,
    echo: bool,
}

impl TermiosEvidence {
    fn read(fd: i32) -> io::Result<Self> {
        let mut value: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut value) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            iflag: value.c_iflag,
            oflag: value.c_oflag,
            cflag: value.c_cflag,
            lflag: value.c_lflag,
            canonical: value.c_lflag & libc::ICANON != 0,
            echo: value.c_lflag & libc::ECHO != 0,
        })
    }

    fn json(self) -> serde_json::Value {
        serde_json::json!({
            "iflag": self.iflag, "oflag": self.oflag, "cflag": self.cflag,
            "lflag": self.lflag, "canonical": self.canonical, "echo": self.echo,
        })
    }
}

fn private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)
}

fn sha256(bytes: &[u8]) -> String {
    polaris_core::conversation_state::content_hash(bytes)
}

fn exact_dir(path: &Path) -> io::Result<()> {
    let info = fs::symlink_metadata(path)?;
    if !info.file_type().is_dir()
        || info.uid() != unsafe { libc::geteuid() }
        || info.mode() & 0o777 != 0o700
    {
        return Err(io::Error::other(format!(
            "PTY artifact directory must be a real owner-only directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn set_nonblocking(file: &File) -> io::Result<()> {
    let old = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if old < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, old | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_pty() -> io::Result<(File, File)> {
    let mut master = -1;
    let mut slave = -1;
    let size = libc::winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &size,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    set_nonblocking(&master)?;
    Ok((master, slave))
}

fn visible_text(raw: &[u8]) -> String {
    // Ignore terminal controls without pretending to emulate the screen.
    // Each marker is unique to its state and appears only after that draw.
    let mut printable = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == 0x1b {
            if raw.get(index + 1) == Some(&b'[') {
                index += 2;
                while index < raw.len() {
                    let current = raw[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&current) {
                        break;
                    }
                }
                continue;
            }
            if raw.get(index + 1) == Some(&b']') {
                index += 2;
                while index < raw.len() {
                    if raw[index] == 0x07 {
                        index += 1;
                        break;
                    }
                    if raw[index] == 0x1b && raw.get(index + 1) == Some(&b'\\') {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
                continue;
            }
            index += 1;
            continue;
        }
        if raw[index] >= 0x20 || raw[index] == b'\n' {
            printable.push(raw[index]);
        }
        index += 1;
    }
    String::from_utf8_lossy(&printable).into_owned()
}

fn sequence_positions(raw: &[u8], sequence: &[u8]) -> Vec<usize> {
    raw.windows(sequence.len())
        .enumerate()
        .filter_map(|(at, window)| (window == sequence).then_some(at))
        .collect()
}

fn pair_balance(raw: &[u8], enter: &[u8], leave: &[u8]) -> (usize, usize, bool) {
    let enters = sequence_positions(raw, enter);
    let leaves = sequence_positions(raw, leave);
    let mut events = enters
        .iter()
        .map(|at| (*at, 1_i32))
        .chain(leaves.iter().map(|at| (*at, -1_i32)))
        .collect::<Vec<_>>();
    events.sort_unstable_by_key(|event| event.0);
    let mut balance = 0;
    let mut ordered = true;
    for (_, delta) in events {
        balance += delta;
        if balance < 0 || balance > 1 {
            ordered = false;
        }
    }
    (enters.len(), leaves.len(), ordered && balance == 0)
}

fn control_balance(raw: &[u8], scene: Scene) -> (bool, serde_json::Value) {
    let (alt_in, alt_out, alt_balanced) = pair_balance(raw, b"\x1b[?1049h", b"\x1b[?1049l");
    let (cursor_in, cursor_out, cursor_balanced) = pair_balance(raw, b"\x1b[5 q", b"\x1b[0 q");
    let mouse = [1000, 1002, 1003, 1015, 1006]
        .map(|mode| {
            let enter = format!("\x1b[?{mode}h");
            let leave = format!("\x1b[?{mode}l");
            let (enabled, disabled, balanced) =
                pair_balance(raw, enter.as_bytes(), leave.as_bytes());
            serde_json::json!({
                "mode": mode, "enable": enabled, "disable": disabled, "balanced": balanced
            })
        })
        .to_vec();
    let mouse_ok = mouse.iter().all(|mode| {
        mode["balanced"] == true
            && (matches!(scene, Scene::Onboarding) || mode["enable"].as_u64().unwrap_or(0) > 0)
    });
    let cursor_ok = cursor_balanced && (matches!(scene, Scene::Onboarding) || cursor_in > 0);
    let balanced = alt_balanced && alt_in > 0 && mouse_ok && cursor_ok;
    (
        balanced,
        serde_json::json!({
            "balanced": balanced,
            "alternate_screen": {"enter": alt_in, "leave": alt_out, "balanced": alt_balanced},
            "mouse": mouse,
            "cursor_shape": {"set_bar": cursor_in, "reset_default": cursor_out, "balanced": cursor_balanced},
        }),
    )
}

struct PtyCase {
    name: &'static str,
    dir: PathBuf,
    raw_file: File,
    result_file: File,
    raw: Vec<u8>,
    master: Option<File>,
    slave: Option<File>,
    child: Option<Child>,
    binary_sha256: String,
    pid: Option<u32>,
    scene: Scene,
    before: Option<TermiosEvidence>,
    after: Option<TermiosEvidence>,
    exit: Option<ExitStatus>,
    exit_at: Option<Instant>,
    signal_label: &'static str,
    signal_at: Option<Instant>,
    shutdown_bound: Option<Duration>,
    raw_offset_at_signal: Option<usize>,
    git_child: Option<GitChild>,
    git_reparented_ppid: Option<u32>,
    finalized: bool,
}

impl PtyCase {
    fn start(name: &'static str, scene: Scene, base_url: &str) -> io::Result<Self> {
        Self::start_with_project(name, scene, base_url, |_| Ok(()))
    }

    fn start_with_project(
        name: &'static str,
        scene: Scene,
        base_url: &str,
        prepare: impl FnOnce(&Path) -> io::Result<()>,
    ) -> io::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("polaris-linux-signal-pty-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp")?
            .keep();
        exact_dir(&dir)?;
        let raw_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join("raw.pty"))?;
        let result_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join("result.json"))?;
        let binary = Path::new(env!("CARGO_BIN_EXE_polaris"));
        let mut case = Self {
            name,
            dir,
            raw_file,
            result_file,
            raw: Vec::new(),
            master: None,
            slave: None,
            child: None,
            binary_sha256: sha256(&fs::read(binary)?),
            pid: None,
            scene,
            before: None,
            after: None,
            exit: None,
            exit_at: None,
            signal_label: "none",
            signal_at: None,
            shutdown_bound: None,
            raw_offset_at_signal: None,
            git_child: None,
            git_reparented_ppid: None,
            finalized: false,
        };
        case.start_inner(binary, base_url, prepare)?;
        Ok(case)
    }

    fn start_inner(
        &mut self,
        binary: &Path,
        base_url: &str,
        prepare: impl FnOnce(&Path) -> io::Result<()>,
    ) -> io::Result<()> {
        let home = self.dir.join("home");
        let data = self.dir.join("data");
        let acceptance =
            self.name == "linux_tui_local_acceptance_commands_draft_refusal_and_clean_exit";
        let project = self.dir.join(if acceptance {
            "project\tPTY_PATH_\x1b]0;POLARIS_PATH\x07"
        } else {
            "project"
        });
        let temporary = home.join("tmp");
        for path in [&home, &data, &project, &temporary] {
            private_dir(path)?;
            exact_dir(path)?;
        }
        private_dir(&project.join(".polaris"))?;
        fs::write(
            project.join(".polaris/config.toml"),
            "[workflow]\nenabled = false\n[files_md]\nauto_regenerate = false\n[decision]\nenabled = false\n",
        )?;
        prepare(&project)?;
        let (master, slave) = open_pty()?;
        self.before = Some(TermiosEvidence::read(slave.as_raw_fd())?);
        let mut command = Command::new(binary);
        command
            .env_clear()
            .env("HOME", &home)
            .env("POLARIS_DATA_DIR", &data)
            .env("TMPDIR", &temporary)
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("POLARIS_PROVIDER", "openai")
            .env("POLARIS_BASE_URL", base_url)
            .env("POLARIS_MODEL", "gpt-6-astra")
            .args([
                "--sandbox",
                "read-only",
                "--approval",
                "never",
                "--max-turns",
                "2",
            ])
            .current_dir(&project)
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave.try_clone()?));
        if acceptance {
            command
                .arg("--audit")
                .arg(data.join("audit\t\x1b]0;POLARIS_STATUS\x07.jsonl"));
        }
        if self.scene.needs_credential() {
            command.env("POLARIS_API_KEY", "synthetic-linux-signal-pty-only");
        }
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        self.pid = Some(child.id());
        self.master = Some(master);
        self.slave = Some(slave);
        self.child = Some(child);
        Ok(())
    }

    fn project(&self) -> PathBuf {
        self.dir.join("project")
    }

    fn pid(&self) -> u32 {
        self.pid.expect("product was spawned")
    }

    fn capture(&mut self) -> io::Result<()> {
        let Some(master) = self.master.as_mut() else {
            return Ok(());
        };
        let mut part = [0_u8; 8192];
        loop {
            match master.read(&mut part) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    if self.raw.len() + n > RAW_LIMIT {
                        return Err(io::Error::other(
                            "raw.pty exceeded 1 MiB; refusing truncation",
                        ));
                    }
                    self.raw_file.write_all(&part[..n])?;
                    self.raw.extend_from_slice(&part[..n]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                // Linux PTY master reports EIO after the final slave closes.
                Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }

    fn marker(&mut self, marker: &str, deadline: Duration) -> Result<(), String> {
        self.marker_since(0, marker, deadline)
    }

    fn marker_since(
        &mut self,
        offset: usize,
        marker: &str,
        deadline: Duration,
    ) -> Result<(), String> {
        let until = Instant::now() + deadline;
        loop {
            self.capture().map_err(|error| error.to_string())?;
            if visible_text(&self.raw[offset..]).contains(marker) {
                return Ok(());
            }
            if self
                .poll_exit()
                .map_err(|error| error.to_string())?
                .is_some()
            {
                return Err(format!(
                    "product exited before visible state marker {marker:?}"
                ));
            }
            if Instant::now() >= until {
                return Err(format!(
                    "no visible state marker within {deadline:?}: {marker:?}"
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn write_key(&mut self, bytes: &[u8]) -> Result<(), String> {
        let master = self.master.as_mut().ok_or("PTY master unavailable")?;
        let until = Instant::now() + Duration::from_secs(1);
        let mut remaining = bytes;
        while !remaining.is_empty() {
            match master.write(remaining) {
                Ok(0) => return Err("PTY input write returned zero".into()),
                Ok(n) => remaining = &remaining[n..],
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= until {
                        return Err("PTY input write timed out".into());
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(())
    }

    fn poll_exit(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.exit.is_some() {
            return Ok(self.exit);
        }
        if let Some(child) = self.child.as_mut() {
            self.exit = child.try_wait()?;
            if self.exit.is_some() {
                self.exit_at = Some(Instant::now());
            }
        }
        Ok(self.exit)
    }

    fn send_os_signal(&mut self, signal: i32, label: &'static str) -> Result<(), String> {
        self.capture().map_err(|error| error.to_string())?;
        if self
            .poll_exit()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err("product exited before OS signal".into());
        }
        let pid = self.pid() as libc::pid_t;
        let delivery_started = Instant::now();
        if unsafe { libc::kill(pid, signal) } != 0 {
            return Err(format!(
                "kill({pid}, {label}) failed: {}",
                io::Error::last_os_error()
            ));
        }
        self.signal_label = label;
        self.signal_at = Some(delivery_started);
        self.raw_offset_at_signal = Some(self.raw.len());
        Ok(())
    }

    fn note_key_shutdown(&mut self) -> Result<(), String> {
        self.capture().map_err(|error| error.to_string())?;
        self.signal_label = "crossterm_ctrl_c_key_twice";
        self.signal_at = Some(Instant::now());
        self.raw_offset_at_signal = Some(self.raw.len());
        Ok(())
    }

    fn wait_exit(&mut self, bound: Duration) -> Result<(), String> {
        self.shutdown_bound = Some(bound);
        let begin = self.signal_at.ok_or("shutdown was not delivered")?;
        let until = begin + bound;
        loop {
            self.capture().map_err(|error| error.to_string())?;
            if self
                .poll_exit()
                .map_err(|error| error.to_string())?
                .is_some()
            {
                let observed_exit = self
                    .exit_at
                    .expect("poll_exit records its observation time");
                self.capture().map_err(|error| error.to_string())?;
                self.after = self
                    .slave
                    .as_ref()
                    .map(|slave| TermiosEvidence::read(slave.as_raw_fd()))
                    .transpose()
                    .map_err(|error| error.to_string())?;
                if observed_exit > until {
                    return Err(format!(
                        "product exit observed {:?} after shutdown, beyond {bound:?}",
                        observed_exit.duration_since(begin)
                    ));
                }
                return Ok(());
            }
            if Instant::now() >= until {
                return Err(format!(
                    "product did not exit within {bound:?} after shutdown"
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn signal_output(&self) -> &[u8] {
        &self.raw[self.raw_offset_at_signal.unwrap_or(self.raw.len())..]
    }

    fn finish(&mut self, expected_code: i32, scenario: Result<(), String>) -> Result<(), String> {
        let mut failures = Vec::new();
        if let Err(error) = scenario {
            failures.push(error);
        }
        if let Some(status) = self.exit {
            if status.signal().is_some() || status.code() != Some(expected_code) {
                failures.push(format!(
                    "expected WIFEXITED code {expected_code}, observed {status:?}"
                ));
            }
        } else {
            failures.push("no waitpid exit classification".into());
        }
        let git_survived_product_exit =
            self.exit.is_some() && self.git_child.as_ref().is_some_and(git_identity_alive);
        if git_survived_product_exit {
            failures.push(format!(
                "verified /usr/bin/git child {} survived product exit",
                self.git_child.as_ref().expect("observed Git child").pid
            ));
        }
        if self.exit.is_none() || git_survived_product_exit {
            self.cleanup_owned();
            if self.after.is_none() {
                self.after = self
                    .slave
                    .as_ref()
                    .and_then(|slave| TermiosEvidence::read(slave.as_raw_fd()).ok());
            }
        }
        match (self.before, self.after) {
            (Some(before), Some(after)) => {
                if !before.canonical || !before.echo || !after.canonical || !after.echo {
                    failures.push("canonical mode or echo was not restored".into());
                }
            }
            _ => failures.push("missing before/after slave termios".into()),
        }
        let (balanced, controls) = control_balance(&self.raw, self.scene);
        if !balanced {
            failures.push("emitted terminal controls are missing or unbalanced".into());
        }
        let elapsed = self
            .signal_at
            .zip(self.exit_at)
            .map(|(signal, exit)| exit.duration_since(signal).as_millis());
        let result = serde_json::json!({
            "test": self.name,
            "status": if failures.is_empty() { "passed" } else { "failed" },
            "failures": failures,
            "binary_sha256": self.binary_sha256,
            "pid": self.pid,
            "signal": self.signal_label,
            "signal_to_exit_ms": elapsed,
            "before_slave_termios": self.before.map(TermiosEvidence::json),
            "after_slave_termios": self.after.map(TermiosEvidence::json),
            "waitpid": self.exit.map(|status| serde_json::json!({
                "classification": if status.signal().is_none() { "WIFEXITED" } else { "WIFSIGNALED" },
                "exit_code": status.code(),
                "term_signal": status.signal(),
                "raw_status": status.into_raw(),
            })),
            "control_sequence_balance": controls,
            "raw_sha256": sha256(&self.raw),
            "raw_bytes": self.raw.len(),
            "git_child": self.git_child.as_ref().map(|git| serde_json::json!({
                "pid": git.pid, "ppid": git.ppid, "exe": git.exe,
                "starttime": git.starttime,
                "identity_alive_at_product_exit": git_survived_product_exit,
                "identity_alive_after_harness_cleanup": git_identity_alive(git),
                "reparented_ppid_after_product_wait": self.git_reparented_ppid,
            })),
        });
        let encoded = serde_json::to_vec_pretty(&result).map_err(|error| error.to_string())?;
        self.result_file
            .set_len(0)
            .map_err(|error| error.to_string())?;
        self.result_file
            .seek(SeekFrom::Start(0))
            .map_err(|error| error.to_string())?;
        self.result_file
            .write_all(&encoded)
            .map_err(|error| error.to_string())?;
        self.result_file
            .sync_all()
            .map_err(|error| error.to_string())?;
        self.raw_file
            .sync_all()
            .map_err(|error| error.to_string())?;
        let result_hash = sha256(&encoded);
        eprintln!(
            "PTY_ARTIFACT test={} dir={} raw_sha256={} result_sha256={}",
            self.name,
            self.dir.display(),
            sha256(&self.raw),
            result_hash,
        );
        self.finalized = true;
        if result["status"] == "passed" {
            Ok(())
        } else {
            Err(format!(
                "{}: {} (artifacts: {})",
                self.name,
                result["failures"],
                self.dir.display()
            ))
        }
    }

    fn cleanup_owned(&mut self) {
        let _ = self.poll_exit();
        // If the test failed before its shutdown signal, the existing
        // three-second read deadline plus one second is the Git bound.
        let git_deadline = self.git_child.as_ref().map(|git| {
            self.signal_at
                .map(|signal| {
                    signal
                        + self.shutdown_bound.unwrap_or_else(|| {
                            if self.name.contains("_diff_") {
                                DIFF_SHUTDOWN_DEADLINE
                            } else {
                                NORMAL_SHUTDOWN_DEADLINE
                            }
                        })
                })
                .unwrap_or(git.observed_at + DIFF_SHUTDOWN_DEADLINE)
        });
        if let Some(git) = &self.git_child {
            if git_stopped(git.pid) {
                let parent_state = if self.exit.is_some() {
                    GitParentState::ReparentedAfterProductWait {
                        product_pid: self.pid(),
                    }
                } else {
                    GitParentState::Direct
                };
                if let Ok(Some(ppid)) = signal_verified_git(git, libc::SIGCONT, parent_state) {
                    if matches!(
                        parent_state,
                        GitParentState::ReparentedAfterProductWait { .. }
                    ) {
                        self.git_reparented_ppid = Some(ppid);
                    }
                }
            }
        }
        if let Some(deadline) = git_deadline {
            while self.git_child.as_ref().is_some_and(git_identity_alive)
                && Instant::now() < deadline
            {
                let _ = self.capture();
                let _ = self.poll_exit();
                thread::sleep(Duration::from_millis(5));
            }
            // Only after the product missed its signal-to-exit bound (or
            // its Git read bound before a signal) may harness cleanup force
            // the exact Git child. Recheck PID, PPid, exe and starttime, and
            // deliver over its pidfd so numeric PID reuse cannot redirect it.
            if Instant::now() >= deadline {
                if let Some(git) = &self.git_child {
                    let parent_state = if self.exit.is_some() {
                        GitParentState::ReparentedAfterProductWait {
                            product_pid: self.pid(),
                        }
                    } else {
                        GitParentState::Direct
                    };
                    if let Ok(Some(ppid)) = signal_verified_git(git, libc::SIGKILL, parent_state) {
                        if matches!(
                            parent_state,
                            GitParentState::ReparentedAfterProductWait { .. }
                        ) {
                            self.git_reparented_ppid = Some(ppid);
                        }
                    }
                }
            }
        }
        if self.exit.is_none() {
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
                self.exit = child.wait().ok();
                if self.exit.is_some() {
                    self.exit_at = Some(Instant::now());
                }
            }
        }
        let _ = self.capture();
    }
}

impl Drop for PtyCase {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        self.cleanup_owned();
        let _ = self.raw_file.sync_all();
        let result = serde_json::json!({
            "test": self.name,
            "status": "failed_or_incomplete",
            "binary_sha256": self.binary_sha256,
            "pid": self.pid,
            "signal": self.signal_label,
            "before_slave_termios": self.before.map(TermiosEvidence::json),
            "after_slave_termios": self.after.map(TermiosEvidence::json),
            "waitpid": self.exit.map(|status| serde_json::json!({
                "classification": if status.signal().is_none() { "WIFEXITED" } else { "WIFSIGNALED" },
                "exit_code": status.code(), "term_signal": status.signal(),
            })),
            "control_sequence_balance": control_balance(&self.raw, self.scene).1,
            "raw_sha256": sha256(&self.raw),
        });
        if let Ok(encoded) = serde_json::to_vec_pretty(&result) {
            let _ = self.result_file.set_len(0);
            let _ = self.result_file.seek(SeekFrom::Start(0));
            let _ = self.result_file.write_all(&encoded);
            let _ = self.result_file.sync_all();
            eprintln!(
                "PTY_ARTIFACT test={} dir={} raw_sha256={} result_sha256={}",
                self.name,
                self.dir.display(),
                sha256(&self.raw),
                sha256(&encoded),
            );
        }
    }
}

#[derive(Debug)]
struct GitChild {
    pid: u32,
    ppid: u32,
    exe: PathBuf,
    starttime: u64,
    observed_at: Instant,
    pidfd: File,
}

fn proc_ppid(pid: u32) -> Option<u32> {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("PPid:")?.trim().parse().ok())
}

fn proc_starttime(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The parenthesized command may contain spaces; field 3 is the first
    // token after its closing parenthesis and starttime is field 22.
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

fn proc_exe(pid: u32) -> Option<PathBuf> {
    fs::canonicalize(format!("/proc/{pid}/exe")).ok()
}

fn git_stopped(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("State:")
                    .and_then(|state| state.trim().chars().next())
            })
        })
        .is_some_and(|state| state == 'T' || state == 't')
}

fn git_exact_direct_child(child: &GitChild) -> bool {
    proc_ppid(child.pid) == Some(child.ppid)
        && proc_exe(child.pid).as_ref() == Some(&child.exe)
        && proc_starttime(child.pid) == Some(child.starttime)
}

fn git_identity_alive(child: &GitChild) -> bool {
    proc_exe(child.pid).as_ref() == Some(&child.exe)
        && proc_starttime(child.pid) == Some(child.starttime)
}

fn open_pidfd(pid: u32) -> io::Result<File> {
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(descriptor as i32) })
}

#[derive(Clone, Copy)]
enum GitParentState {
    Direct,
    ReparentedAfterProductWait { product_pid: u32 },
}

fn signal_verified_git(
    child: &GitChild,
    signal: i32,
    parent_state: GitParentState,
) -> io::Result<Option<u32>> {
    let Some(current_ppid) = proc_ppid(child.pid) else {
        return Ok(None);
    };
    let expected_parent = match parent_state {
        GitParentState::Direct => current_ppid == child.ppid,
        GitParentState::ReparentedAfterProductWait { product_pid } => {
            // The caller has reaped the original product with waitpid.
            // A surviving child must be adopted by init or a subreaper;
            // accepting the old or a zero PPid would hide a stale state.
            product_pid == child.ppid && current_ppid != child.ppid && current_ppid != 0
        }
    };
    if !expected_parent || !git_identity_alive(child) {
        return Ok(None);
    }
    // The pidfd also binds delivery to the process observed above if its
    // numeric PID changes ownership between the /proc recheck and syscall.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            child.pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(current_ppid))
}

fn direct_children(parent: u32) -> Vec<u32> {
    fs::read_to_string(format!("/proc/{parent}/task/{parent}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|text| text.parse().ok())
        .collect()
}

fn find_and_stop_git_child(parent: u32) -> Result<GitChild, String> {
    let git_exe = fs::canonicalize("/usr/bin/git").map_err(|error| error.to_string())?;
    let until = Instant::now() + SCREEN_DEADLINE;
    while Instant::now() < until {
        for pid in direct_children(parent) {
            if proc_ppid(pid) != Some(parent) || proc_exe(pid).as_ref() != Some(&git_exe) {
                continue;
            }
            let Some(starttime) = proc_starttime(pid) else {
                continue;
            };
            let pidfd = open_pidfd(pid)
                .map_err(|error| format!("cannot hold verified Git child by pidfd: {error}"))?;
            let child = GitChild {
                pid,
                ppid: parent,
                exe: git_exe.clone(),
                starttime,
                observed_at: Instant::now(),
                pidfd,
            };
            if !git_exact_direct_child(&child) {
                continue;
            }
            if signal_verified_git(&child, libc::SIGSTOP, GitParentState::Direct)
                .map_err(|error| format!("cannot stop verified Git child: {error}"))?
                .is_none()
            {
                continue;
            }
            let stopped_until = (Instant::now() + Duration::from_millis(500)).min(until);
            while Instant::now() < stopped_until {
                if git_exact_direct_child(&child) && git_stopped(pid) {
                    return Ok(child);
                }
                if !git_identity_alive(&child) {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
    Err("no exact direct /usr/bin/git child entered stopped state within 10 seconds".into())
}

fn run_git(project: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(project)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "disposable git fixture {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn prepare_diff_fixture(project: &Path) -> Result<(), String> {
    run_git(project, &["init", "--quiet"])?;
    let path = project.join("signal-diff-fixture.txt");
    let mut original = String::with_capacity(12_000_000);
    for row in 0..120_000 {
        original.push_str(&format!(
            "{row:06} fixture base abcdefghijklmnopqrstuvwxyz 0123456789 abcdefghijklmnopqrstuvwxyz\n"
        ));
    }
    fs::write(&path, original.as_bytes()).map_err(|error| error.to_string())?;
    run_git(project, &["add", "--", "signal-diff-fixture.txt"])?;
    let mut modified = String::with_capacity(original.len());
    for (row, line) in original.lines().enumerate() {
        if row % 200 == 0 {
            modified.push_str(&format!("{row:06} changed {DIFF_SENTINEL}\n"));
        } else {
            modified.push_str(line);
            modified.push('\n');
        }
    }
    fs::write(&path, modified).map_err(|error| error.to_string())
}

struct HeldFixture {
    url: String,
    arrived: mpsc::Receiver<Result<(), String>>,
    release: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl HeldFixture {
    fn start() -> io::Result<Self> {
        Self::start_with_response(None)
    }

    fn start_with_response(response: Option<(&'static str, &'static str)>) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}", listener.local_addr()?);
        let (arrived_tx, arrived) = mpsc::channel();
        let release = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&release);
        let worker = thread::spawn(move || {
            loop {
                if stopping.load(Ordering::Acquire) {
                    return;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let result = read_fixture_request(&mut stream);
                        if result.is_ok()
                            && let Some((status, body)) = response
                        {
                            let header = format!(
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let sent = stream
                                .write_all(header.as_bytes())
                                .and_then(|_| stream.write_all(body.as_bytes()));
                            let _ = arrived_tx.send(sent.map_err(|error| error.to_string()));
                            return;
                        }
                        let _ = arrived_tx.send(result);
                        // Keep the response pending for the entire product
                        // shutdown observation, even after a slow startup.
                        while !stopping.load(Ordering::Acquire) {
                            thread::sleep(Duration::from_millis(5));
                        }
                        return;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => {
                        let _ = arrived_tx.send(Err(error.to_string()));
                        return;
                    }
                }
            }
        });
        Ok(Self {
            url,
            arrived,
            release,
            worker: Some(worker),
        })
    }

    fn await_request(&self) -> Result<(), String> {
        self.arrived
            .recv_timeout(SCREEN_DEADLINE)
            .map_err(|error| format!("no complete in-flight request barrier: {error}"))??;
        Ok(())
    }
}

impl Drop for HeldFixture {
    fn drop(&mut self) {
        self.release.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_fixture_request(stream: &mut TcpStream) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    let mut request = Vec::new();
    let mut part = [0_u8; 16 * 1024];
    loop {
        let n = stream.read(&mut part).map_err(|error| error.to_string())?;
        if n == 0 {
            return Err("fixture request closed before full body".into());
        }
        request.extend_from_slice(&part[..n]);
        if request.len() > 2 * 1024 * 1024 {
            return Err("fixture request exceeded 2 MiB".into());
        }
        let Some(at) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&request[..at]).map_err(|error| error.to_string())?;
        if !headers.starts_with("POST /chat/completions HTTP/1.1\r\n") {
            return Err(format!("unexpected loopback request route: {headers}"));
        }
        let size = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .ok_or("fixture request lacks Content-Length")?;
        if size > 2 * 1024 * 1024 {
            return Err("fixture request body exceeded 2 MiB".into());
        }
        if request.len() >= at + 4 + size {
            let body = &request[at + 4..at + 4 + size];
            if !body
                .windows(INFLIGHT_PROMPT.len())
                .any(|window| window == INFLIGHT_PROMPT.as_bytes())
            {
                return Err("fixture request did not contain the unique in-flight prompt".into());
            }
            return Ok(());
        }
    }
}

fn run_simple_signal(
    name: &'static str,
    scene: Scene,
    marker: &str,
    signal: i32,
    label: &'static str,
    expected_code: i32,
) {
    let mut case = PtyCase::start(name, scene, "http://127.0.0.1:1")
        .unwrap_or_else(|error| panic!("cannot create private PTY case {name}: {error}"));
    let scenario = (|| -> Result<(), String> {
        case.marker(marker, SCREEN_DEADLINE)?;
        case.send_os_signal(signal, label)?;
        case.wait_exit(NORMAL_SHUTDOWN_DEADLINE)
    })();
    case.finish(expected_code, scenario)
        .unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn linux_process_signal_cleanup_onboarding_sigint_after_screen_ready() {
    run_simple_signal(
        "linux_process_signal_cleanup_onboarding_sigint_after_screen_ready",
        Scene::Onboarding,
        ONBOARDING_MARKER,
        libc::SIGINT,
        "SIGINT",
        130,
    );
}

#[test]
fn linux_process_signal_cleanup_idle_sigint_after_screen_ready() {
    run_simple_signal(
        "linux_process_signal_cleanup_idle_sigint_after_screen_ready",
        Scene::Main,
        IDLE_MARKER,
        libc::SIGINT,
        "SIGINT",
        130,
    );
}

#[test]
fn linux_process_signal_cleanup_idle_sigterm_after_screen_ready() {
    run_simple_signal(
        "linux_process_signal_cleanup_idle_sigterm_after_screen_ready",
        Scene::Main,
        IDLE_MARKER,
        libc::SIGTERM,
        "SIGTERM",
        143,
    );
}

#[test]
fn linux_process_signal_cleanup_picker_sigint_accepts_no_choice() {
    const NAME: &str = "linux_process_signal_cleanup_picker_sigint_accepts_no_choice";
    let mut case =
        PtyCase::start(NAME, Scene::Main, "http://127.0.0.1:1").expect("private PTY case");
    let scenario = (|| -> Result<(), String> {
        case.marker(IDLE_MARKER, SCREEN_DEADLINE)?;
        case.write_key(b"/permissions\r")?;
        case.marker(PICKER_MARKER, SCREEN_DEADLINE)?;
        case.send_os_signal(libc::SIGINT, "SIGINT")?;
        case.wait_exit(NORMAL_SHUTDOWN_DEADLINE)?;
        if visible_text(case.signal_output()).contains("permissions set to") {
            return Err("picker accepted a choice after SIGINT".into());
        }
        Ok(())
    })();
    case.finish(130, scenario)
        .unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn linux_process_signal_cleanup_diff_sigterm_is_bounded_and_suppressed() {
    const NAME: &str = "linux_process_signal_cleanup_diff_sigterm_is_bounded_and_suppressed";
    let mut case =
        PtyCase::start(NAME, Scene::Main, "http://127.0.0.1:1").expect("private PTY case");
    let scenario = (|| -> Result<(), String> {
        case.marker(IDLE_MARKER, SCREEN_DEADLINE)?;
        prepare_diff_fixture(&case.project())?;
        let pid = case.pid();
        let (armed_tx, armed_rx) = mpsc::channel();
        let watcher = thread::spawn(move || {
            let _ = armed_tx.send(());
            find_and_stop_git_child(pid)
        });
        armed_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|error| format!("Git watcher did not arm: {error}"))?;
        case.write_key(b"/diff\r")?;
        let git = watcher
            .join()
            .map_err(|_| "Git watcher panicked".to_string())??;
        case.git_child = Some(git);
        case.capture().map_err(|error| error.to_string())?;
        case.send_os_signal(libc::SIGTERM, "SIGTERM")?;
        case.wait_exit(DIFF_SHUTDOWN_DEADLINE)?;
        let after = visible_text(case.signal_output());
        if after.contains("diff:") || after.contains(DIFF_SENTINEL) {
            return Err("/diff rendered result bytes after SIGTERM".into());
        }
        if case.git_child.as_ref().is_some_and(git_identity_alive) {
            return Err("verified Git child remained after product exit".into());
        }
        Ok(())
    })();
    case.finish(143, scenario)
        .unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn linux_process_signal_cleanup_inflight_sigint_uses_owned_cleanup() {
    const NAME: &str = "linux_process_signal_cleanup_inflight_sigint_uses_owned_cleanup";
    let fixture = HeldFixture::start().expect("harness-only loopback fixture");
    let mut case = PtyCase::start(NAME, Scene::Main, &fixture.url).expect("private PTY case");
    let scenario = (|| -> Result<(), String> {
        case.marker(IDLE_MARKER, SCREEN_DEADLINE)?;
        case.write_key(INFLIGHT_PROMPT.as_bytes())?;
        case.marker("INFLIGHT_ONLY_928", SCREEN_DEADLINE)?;
        case.write_key(b"\r")?;
        fixture.await_request()?;
        case.send_os_signal(libc::SIGINT, "SIGINT")?;
        case.wait_exit(NORMAL_SHUTDOWN_DEADLINE)
    })();
    case.finish(130, scenario)
        .unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn linux_process_signal_cleanup_ctrl_c_key_keeps_existing_semantics() {
    const NAME: &str = "linux_process_signal_cleanup_ctrl_c_key_keeps_existing_semantics";
    let mut case =
        PtyCase::start(NAME, Scene::Main, "http://127.0.0.1:1").expect("private PTY case");
    let scenario = (|| -> Result<(), String> {
        case.marker(IDLE_MARKER, SCREEN_DEADLINE)?;
        case.write_key(b"\x03")?;
        case.marker(CTRL_C_MARKER, SCREEN_DEADLINE)?;
        if case
            .poll_exit()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err("first Ctrl+C key exited instead of requesting confirmation".into());
        }
        case.note_key_shutdown()?;
        case.write_key(b"\x03")?;
        case.wait_exit(NORMAL_SHUTDOWN_DEADLINE)
    })();
    case.finish(0, scenario)
        .unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn linux_tui_handled_provider_error_exits_with_restored_terminal() {
    const NAME: &str = "linux_tui_handled_provider_error_exits_with_restored_terminal";
    let fixture = HeldFixture::start_with_response(Some(("400 Bad Request", "{}")))
        .expect("local synthetic provider error");
    let mut case = PtyCase::start(NAME, Scene::Main, &fixture.url).expect("private product PTY");
    let scenario = (|| -> Result<(), String> {
        case.marker(IDLE_MARKER, SCREEN_DEADLINE)?;
        let offset = case.raw.len();
        case.write_key(INFLIGHT_PROMPT.as_bytes())?;
        case.write_key(b"\r")?;
        // A delta redraw can encode blank cells as cursor movement.
        for part in ["400", "Bad", "Request"] {
            case.marker_since(offset, part, SCREEN_DEADLINE)?;
        }
        fixture.await_request()?;
        case.capture().map_err(|error| error.to_string())?;
        case.signal_label = "slash_quit_after_handled_error";
        case.signal_at = Some(Instant::now());
        case.raw_offset_at_signal = Some(case.raw.len());
        case.write_key(b"/quit\r")?;
        case.wait_exit(NORMAL_SHUTDOWN_DEADLINE)
    })();
    case.finish(0, scenario).expect(NAME);
}

#[test]
fn linux_tui_local_acceptance_commands_draft_refusal_and_clean_exit() {
    const NAME: &str = "linux_tui_local_acceptance_commands_draft_refusal_and_clean_exit";
    const REPLY: &str = "POLARIS_PTY_ACCEPTANCE_REPLY";
    const RESPONSE: &str = r#"{"choices":[{"message":{"role":"assistant","content":"POLARIS_PTY_ACCEPTANCE_REPLY \u001b]0;POLARIS_REPLY\u0007 \u001b[31m \u0001"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}"#;
    let fixture = HeldFixture::start_with_response(Some(("200 OK", RESPONSE)))
        .expect("local synthetic provider");
    let mut case = PtyCase::start_with_project(NAME, Scene::Main, &fixture.url, |project| {
        run_git(project, &["init", "--quiet"]).map_err(io::Error::other)?;
        let filename = "PTY_GIT_\t\x1b]0;POLARIS_GIT\x07.txt";
        fs::write(project.join(filename), "baseline\n")?;
        run_git(project, &["add", "--", filename]).map_err(io::Error::other)?;
        fs::write(project.join(filename), "baseline\n\x1b]0;POLARIS_DIFF\x07\x1b[31mPTY_DIFF_OUTPUT\x01\n")?;
        private_dir(&project.join(".polaris/skills"))?;
        private_dir(&project.join(".polaris/skills/fixture"))?;
        fs::write(project.join(".polaris/skills/fixture/SKILL.md"), "---\nname: fixture\ndescription: Local terminal fixture only\n---\nNo external operations.\n")
    }).expect("private product PTY");
    let scenario = (|| -> Result<(), String> {
        case.marker(IDLE_MARKER, SCREEN_DEADLINE)?;
        for (command, result) in [
            ("/help", "commands listed"),
            ("/status", "状態を表示しました"),
            ("/doctor", "診断結果"),
            ("/web-search", "Web検索の利用条件"),
            ("/models", "モデル一覧"),
            ("/pwd", "project"),
            ("/mcp", "外部MCP"),
            ("/diff", "diff:"),
        ] {
            case.capture().map_err(|error| error.to_string())?;
            let offset = case.raw.len();
            case.write_key(command.as_bytes())?;
            case.write_key(b"\r")?;
            case.marker_since(offset, result, SCREEN_DEADLINE)?;
        }
        let offset = case.raw.len();
        case.write_key(b"/permissions\r")?;
        case.marker_since(offset, "choose an approval preference", SCREEN_DEADLINE)?;
        let offset = case.raw.len();
        case.write_key(b"\x1b")?;
        case.marker_since(offset, IDLE_MARKER, SCREEN_DEADLINE)?;
        let offset = case.raw.len();
        case.write_key(b"/skills\r")?;
        case.marker_since(offset, "skills discovered in this project", SCREEN_DEADLINE)?;
        case.marker_since(offset, "Local terminal fixture only", SCREEN_DEADLINE)?;
        case.write_key(b"\x1b")?;
        case.marker(IDLE_MARKER, SCREEN_DEADLINE)?;
        for (columns, rows) in [(40, 10), (120, 40)] {
            let size = libc::winsize {
                ws_row: rows,
                ws_col: columns,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            if unsafe {
                libc::ioctl(
                    case.slave.as_ref().ok_or("no slave")?.as_raw_fd(),
                    libc::TIOCSWINSZ,
                    &size,
                )
            } != 0
            {
                return Err(io::Error::last_os_error().to_string());
            }
            thread::sleep(Duration::from_millis(100));
            case.capture().map_err(|error| error.to_string())?;
        }
        case.write_key(b"\x1b[5~\x1b[6~")?;
        case.write_key(format!("/review {INFLIGHT_PROMPT}").as_bytes())?;
        case.write_key(b"\r")?;
        // Drain redraws while the product processes the typed slash command.
        case.marker(REPLY, SCREEN_DEADLINE)?;
        fixture.await_request()?;
        let offset = case.raw.len();
        case.write_key(b"/run details\r")?;
        case.marker_since(offset, "実行状態", SCREEN_DEADLINE)?;
        // Recall an older input while retaining the independent live draft.
        // Move to the start by byte-safe repeated Left on this ASCII fixture.
        case.write_key(b"DRAFT")?;
        case.write_key(b"\x1b[A")?;
        for _ in 0..128 {
            case.write_key(b"\x1b[D")?;
        }
        case.write_key(b"/quit ")?;
        let offset = case.raw.len();
        case.write_key(b"\r")?;
        case.marker_since(offset, "送信待ち・下書き", SCREEN_DEADLINE)?;
        if case
            .poll_exit()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err("pending draft was discarded on quit".into());
        }
        case.marker_since(offset, "DRAFT", SCREEN_DEADLINE)?;
        for _ in 0..5 {
            case.write_key(b"\x7f")?;
        }
        case.capture().map_err(|error| error.to_string())?;
        case.signal_label = "slash_quit";
        case.signal_at = Some(Instant::now());
        case.raw_offset_at_signal = Some(case.raw.len());
        case.write_key(b"/quit\r")?;
        case.wait_exit(NORMAL_SHUTDOWN_DEADLINE)?;
        for injected in [
            b"\x1b]0;POLARIS_PATH".as_slice(),
            b"\x1b]0;POLARIS_STATUS".as_slice(),
            b"\x1b]0;POLARIS_GIT".as_slice(),
            b"\x1b]0;POLARIS_DIFF".as_slice(),
            b"\x1b]0;POLARIS_REPLY".as_slice(),
            b"\x1b[31mPTY_DIFF_OUTPUT".as_slice(),
            b"\x01".as_slice(),
        ] {
            if case
                .raw
                .windows(injected.len())
                .any(|bytes| bytes == injected)
            {
                return Err("injected terminal sequence reached the raw PTY".into());
            }
        }
        if !visible_text(&case.raw).contains("PTY_DIFF_OUTPUT") {
            return Err("controlled Git diff was not rendered".into());
        }
        if !visible_text(&case.raw).contains("polaris --resume ") {
            return Err("clean exit did not print the persisted session resume hint".into());
        }
        Ok(())
    })();
    case.finish(0, scenario)
        .unwrap_or_else(|error| panic!("{error}"));
}
