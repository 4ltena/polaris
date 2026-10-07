//! Real CLI input preparation with an isolated home and a synthetic loopback model.

use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

#[path = "support/product_binary.rs"]
mod product_binary;

use polaris_core::{
    conversation_state::RawEventV2, session::Session, session_store::PersistedSession,
};
use serde_json::Value;

struct RecordedModel {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<io::Result<()>>>,
}

impl RecordedModel {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                recorded.lock().unwrap().push(read_request(&mut stream)?);
                let output = serde_json::to_vec(&serde_json::json!({
                    "choices":[{"message":{"role":"assistant","content":"fixture done"}}],
                    "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}
                }))?;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    output.len(),
                )?;
                stream.write_all(&output)?;
                stream.flush()?;
            }
            Ok(())
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn finish(mut self) -> Vec<Value> {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap().unwrap();
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for RecordedModel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<Value> {
    let mut bytes = Vec::new();
    let mut part = [0; 16 * 1024];
    loop {
        let read = stream.read(&mut part)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete request",
            ));
        }
        bytes.extend_from_slice(&part[..read]);
        if bytes.len() > 2 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fixture request too large",
            ));
        }
        if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..offset])
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if !headers.starts_with("POST /chat/completions HTTP/1.1\r\n") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected model route",
                ));
            }
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "missing content length")
                })?;
            if length > 2 * 1024 * 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fixture body too large",
                ));
            }
            if bytes.len() >= offset + 4 + length {
                return serde_json::from_slice(&bytes[offset + 4..offset + 4 + length])
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
            }
        }
    }
}

fn long_input() -> String {
    let mut paragraph = String::new();
    while polaris_core::budget::count_tokens(&paragraph) < 700 {
        paragraph.push_str("背景の条件は同じです。名前はか\u{3099}、数量は12であり、変更しない。 ");
    }
    paragraph.push_str("\r\n\r\n");
    paragraph.repeat(9)
}

fn assert_saved_and_reopened(home: &Path, project: &Path, original: &str) {
    let data_root = home.join(".polaris");
    let directories: Vec<_> = std::fs::read_dir(data_root.join("sessions-v2"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    assert_eq!(directories.len(), 1, "the CLI creates exactly one session");
    let session_id = directories[0].file_name().unwrap().to_str().unwrap();
    let raw_path = directories[0].join("raw.jsonl");
    let raw_before = std::fs::read(&raw_path).unwrap();
    let events: Vec<RawEventV2> = std::str::from_utf8(&raw_before)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let user_inputs: Vec<_> = events.iter().filter(|event| event.starts_turn).collect();
    assert_eq!(user_inputs.len(), 1);
    assert_eq!(
        user_inputs[0].message.content.as_bytes(),
        original.as_bytes()
    );
    assert!(events.iter().all(|event| {
        !event
            .message
            .content
            .starts_with("[Polaris input encoding v1]")
    }));

    let state_id = polaris_core::project::project_id(project);
    let project_id = polaris_tui::persist::project_identity(project).unwrap();
    let database = data_root
        .join("state")
        .join(&state_id)
        .join("memory.sqlite3");
    let saved = PersistedSession::open(&data_root, &database, &project_id, session_id).unwrap();
    let snapshot = saved.snapshot().unwrap();
    assert_eq!(
        snapshot.events[0].message.content.as_bytes(),
        original.as_bytes()
    );
    let mut reopened = Session::new();
    reopened.attach(saved).unwrap();
    assert_eq!(reopened.messages[0].content.as_bytes(), original.as_bytes());
    assert_eq!(
        std::fs::read(raw_path).unwrap(),
        raw_before,
        "opening the session does not rewrite raw"
    );
}

#[test]
fn real_cli_projects_only_when_enabled_and_preserves_raw_across_reopen() {
    let original = long_input();
    let input_tokens = polaris_core::budget::count_tokens(&original);
    assert!((4096..=32_000).contains(&input_tokens));
    for mode in ["repeat_refs", "off"] {
        let isolated = tempfile::tempdir().unwrap();
        let home = isolated.path().join("home");
        let project = isolated.path().join("project");
        let temporary = isolated.path().join("tmp");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(".polaris")).unwrap();
        std::fs::create_dir_all(&temporary).unwrap();
        let home = home.canonicalize().unwrap();
        let project = project.canonicalize().unwrap();
        std::fs::write(project.join(".polaris/config.toml"), format!(
            "input_preparation = \"{mode}\"\n[workflow]\nenabled = true\nmode = \"manual\"\nalways = []\nphase_skills = {{}}\n[files_md]\nauto_regenerate = false\n[decision]\nenabled = false\n"
        )).unwrap();
        let model = RecordedModel::start();
        let output = product_binary::command()
            .env_clear()
            .env("HOME", &home)
            .env("TMPDIR", &temporary)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("POLARIS_PROVIDER", "openai")
            .env("POLARIS_API_KEY", "synthetic-input-preparation-key")
            .env("POLARIS_BASE_URL", &model.url)
            .env("POLARIS_MODEL", "gpt-6-astra")
            .current_dir(&project)
            .args([
                "--sandbox",
                "read-only",
                "--approval",
                "never",
                "--max-turns",
                "2",
                "--prompt",
            ])
            .arg(&original)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let requests = model.finish();
        assert!(output.status.success(), "{mode}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("fixture done"));
        assert_eq!(
            requests.len(),
            1,
            "{mode}: no additional preparation model call"
        );
        assert_eq!(requests[0]["model"], "gpt-6-astra");
        let user_messages: Vec<_> = requests[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .collect();
        assert_eq!(user_messages.len(), 1);
        let sent = user_messages[0]["content"].as_str().unwrap();
        if mode == "repeat_refs" {
            assert!(
                sent.starts_with("[Polaris input encoding v1]\n"),
                "CLI must actually apply the projection"
            );
            let output_tokens = polaris_core::budget::count_tokens(sent);
            assert!(output_tokens * 5 <= input_tokens * 4);
            assert!(input_tokens - output_tokens >= 512);
            // Independently follow the encoded references rather than calling the encoder again.
            let json: Value = serde_json::from_str(sent.splitn(3, '\n').nth(2).unwrap()).unwrap();
            assert_eq!(json["v"], 1);
            let chunks = json["chunks"].as_array().unwrap();
            let restored: String = json["order"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| chunks[id.as_u64().unwrap() as usize].as_str().unwrap())
                .collect();
            assert_eq!(restored.as_bytes(), original.as_bytes());
        } else {
            assert_eq!(
                sent.as_bytes(),
                original.as_bytes(),
                "off must preserve the original request"
            );
        }
        assert_saved_and_reopened(&home, &project, &original);
    }
}
