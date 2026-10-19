//! Explicit host memory commands. Confirmation and results stay in the TUI;
//! only confirmed record text goes to the configured loopback encoder.

use crate::{account, approver::KeyReader, persist, render, tool_memory::Encoder};
use polaris_core::{
    management_effect::{
        ManagementEffectHashV1, ManagementEffectStoreV1, ManagementOperationIdV1,
        memory_change::MemoryChangeOwnerV1,
    },
    project_mutation::open_directory_nofollow,
    rewind_workspace::WorkspaceOwner,
};
use polaris_memory::{
    MemoryStore, Record,
    management::{
        MAX_CHANGE_BYTES, MAX_CHANGE_RECORDS, MemoryChangeReceiptV1, MemoryChangeRequestV1,
        MemoryChangeV1, MemoryEmbeddingUpdateV1, validate_session_id,
    },
};
use ratatui::{Terminal, backend::Backend, text::Line};
use std::{
    cell::RefCell,
    ffi::CString,
    fs::File,
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
    time::Duration,
};

const USAGE: &str = "/memory import <JSONL> | embed <session> <id> | index <session> [--batch <1..100>] [--after <id>] | forget <session> | reconcile <operation-id> | search <query> | get <session> <id> | source <session> <id>";

enum Command {
    Import(PathBuf),
    Embed {
        session: String,
        id: String,
    },
    Index {
        session: String,
        batch: usize,
        after: Option<String>,
    },
    Forget(String),
    Reconcile(ManagementOperationIdV1),
}

fn parse(request: &str) -> Result<Command, String> {
    let (operation, extra) = request
        .split_once(char::is_whitespace)
        .unwrap_or((request, ""));
    let extra = extra.trim();
    if operation == "import" {
        if extra.is_empty() {
            return Err(USAGE.into());
        }
        return Ok(Command::Import(PathBuf::from(extra)));
    }
    let mut parts = extra.split_whitespace();
    let target = parts.next().ok_or_else(|| USAGE.to_owned())?;
    match operation {
        "embed" => {
            validate_session_id(target).map_err(|e| e.to_string())?;
            let id = parts.next().ok_or_else(|| USAGE.to_owned())?;
            if id.len() > 1024 || parts.next().is_some() {
                return Err(USAGE.into());
            }
            Ok(Command::Embed {
                session: target.into(),
                id: id.into(),
            })
        }
        "index" => {
            validate_session_id(target).map_err(|e| e.to_string())?;
            let mut batch = None;
            let mut after = None;
            while let Some(option) = parts.next() {
                let value = parts.next().ok_or_else(|| USAGE.to_owned())?;
                match option {
                    "--batch" if batch.is_none() => {
                        let number = value.parse::<usize>().map_err(|_| USAGE.to_owned())?;
                        if !(1..=MAX_CHANGE_RECORDS).contains(&number) {
                            return Err("索引は1回1〜100件で指定してください".into());
                        }
                        batch = Some(number);
                    }
                    "--after" if after.is_none() && value.len() <= 1024 => {
                        after = Some(value.to_owned())
                    }
                    _ => return Err(USAGE.into()),
                }
            }
            Ok(Command::Index {
                session: target.into(),
                batch: batch.unwrap_or(MAX_CHANGE_RECORDS),
                after,
            })
        }
        "forget" => {
            validate_session_id(target).map_err(|e| e.to_string())?;
            if parts.next().is_some() {
                return Err(USAGE.into());
            }
            Ok(Command::Forget(target.into()))
        }
        "reconcile" => {
            if parts.next().is_some() {
                return Err(USAGE.into());
            }
            Ok(Command::Reconcile(
                target
                    .parse()
                    .map_err(|_| "照合には32桁の操作IDが必要です")?,
            ))
        }
        _ => Err(USAGE.into()),
    }
}

/// The root loop invokes this before any conversation/model dispatch. It owns
/// the terminal, existing workspace owner, configured encoder and active ID.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle<B: Backend, R: KeyReader>(
    request: &str,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    workspace: &WorkspaceOwner,
    effects: &ManagementEffectStoreV1,
    cwd: &Path,
    state_dir: &Path,
    active_session_id: Option<&str>,
    embedding: Option<(String, String)>,
    queue_busy: bool,
) -> Result<Vec<Line<'static>>, String> {
    let request = request.trim();
    if request.is_empty() {
        return Ok(vec![Line::from(USAGE)]);
    }
    if request.len() > 4096 || request.chars().any(char::is_control) {
        return Err("メモリ操作は4096バイト以内の1行で指定してください".into());
    }
    let operation = request.split_whitespace().next().unwrap_or("");
    let data_root = state_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("メモリ保存先が不正です")?;
    if matches!(operation, "search" | "get" | "source") {
        return crate::memory_read_lines(cwd, state_dir, &data_root.join("sessions"), request);
    }
    let command = parse(request)?;
    if queue_busy {
        return Err("送信待ちの入力があるためメモリ変更を開始できません".into());
    }
    let project = persist::project_identity(cwd).map_err(|e| e.to_string())?;
    if workspace.project_root() != Path::new(&project) || effects.state_dir() != state_dir {
        return Err("メモリ操作のプロジェクトと保存先が一致しません".into());
    }
    let lease = workspace
        .try_mutation()
        .map_err(|_| "実行中の操作があるためメモリ変更を開始できません")?;
    workspace
        .require_quiescent_with_lease(&lease)
        .map_err(|e| e.to_string())?;
    if let Some(effect) =
        polaris_core::sync::composite::C10CompositeBlockerV1::inspect_with_mutation(
            cwd, state_dir, &lease,
        )
        .map_err(|error| error.to_string())?
    {
        if !matches!(command, Command::Reconcile(operation)
            if effect.request.inventory.encoded() != "C10"
                && effect.request.operation_id == operation)
        {
            return Err("未解決の管理操作があります。先に照合してください".into());
        }
    }
    let owner = MemoryChangeOwnerV1 {
        effects,
        lease: &lease,
        project_root: cwd,
        data_root,
        active_session: active_session_id,
    };
    let mut store = owner.open_store()?;
    let revision = store
        .project_revision(&project)
        .map_err(|e| e.to_string())?;
    match command {
        Command::Reconcile(operation) => {
            if !account::confirm(
                terminal,
                reader,
                "メモリ変更結果の照合",
                &operation.as_hex(),
                "保存済みreceiptと対象を照合します。変更や削除を再実行しません。",
            ) {
                return Ok(cancelled());
            }
            let receipt = owner.reconcile(&store, operation)?;
            Ok(receipt_lines(&receipt, true))
        }
        Command::Import(path) => {
            let source = if path.is_absolute() {
                path
            } else {
                Path::new(&project).join(path)
            };
            let records = read_import(&source, &project)?;
            let count = records.len();
            let change = MemoryChangeV1::Import { records };
            let prepared = owner.prepare(&store, new_request(&project, revision, change)?)?;
            let accepted = account::confirm(
                terminal,
                reader,
                "C14 記録の取り込み",
                &render::sanitize(&source.display().to_string()),
                &format!(
                    "対象プロジェクト: {project}\n{count}件を索引へ保存します。本文をモデルへ送信しません。"
                ),
            );
            let receipt = owner.apply(&mut store, &prepared, accepted)?;
            Ok(receipt
                .map(|r| receipt_lines(&r, false))
                .unwrap_or_else(cancelled))
        }
        Command::Forget(session) => {
            let prepared = owner.prepare(
                &store,
                new_request(
                    &project,
                    revision,
                    MemoryChangeV1::Forget {
                        session_id: session.clone(),
                    },
                )?,
            )?;
            let accepted = account::confirm(
                terminal,
                reader,
                "C20 セッションの記憶削除",
                &session,
                &format!(
                    "対象プロジェクト: {project}\nこのセッションの本文・ベクトル・検証済み保管原文を削除します。再取り込みは禁止されます。元に戻せません。"
                ),
            );
            let receipt = owner.apply(&mut store, &prepared, accepted)?;
            Ok(receipt
                .map(|r| receipt_lines(&r, false))
                .unwrap_or_else(cancelled))
        }
        Command::Embed { session, id } => {
            let records = vec![
                store
                    .get(&project, &session, &id)
                    .map_err(|e| e.to_string())?
                    .ok_or("指定した記録がありません")?,
            ];
            embed_records(
                &owner, &mut store, &project, revision, records, false, embedding, terminal, reader,
            )
            .await
        }
        Command::Index {
            session,
            batch,
            after,
        } => {
            let records = store
                .records_page(&project, &session, after.as_deref(), batch)
                .map_err(|e| e.to_string())?;
            if records.is_empty() {
                return Err("索引対象の記録がありません".into());
            }
            let next = records.last().map(|r| r.id.clone()).unwrap_or_default();
            let mut lines = embed_records(
                &owner, &mut store, &project, revision, records, true, embedding, terminal, reader,
            )
            .await?;
            if lines
                .first()
                .is_some_and(|line| line.to_string().contains("保存しました"))
            {
                lines.push(Line::from(render::sanitize(&format!(
                    "続きは /memory index {session} --batch {batch} --after {next}"
                ))));
            }
            Ok(lines)
        }
    }
}

fn new_request(
    project: &str,
    revision: u64,
    change: MemoryChangeV1,
) -> Result<MemoryChangeRequestV1, String> {
    Ok(MemoryChangeRequestV1 {
        operation_id: ManagementOperationIdV1::random()
            .map_err(|e| e.to_string())?
            .as_hex(),
        project_id: project.into(),
        expected_revision: revision,
        change,
    })
}
fn cancelled() -> Vec<Line<'static>> {
    vec![Line::from("メモリ操作を取り消しました")]
}
fn receipt_lines(receipt: &MemoryChangeReceiptV1, reconciled: bool) -> Vec<Line<'static>> {
    let result = if reconciled {
        "保存済みの変更結果を照合しました"
    } else if receipt.kind == polaris_memory::management::MemoryChangeKindV1::Forget {
        "対象セッションの記憶と検証済み保管原文を削除しました"
    } else {
        "記録の変更を保存しました"
    };
    vec![
        Line::from(format!(
            "{result}。記録{}件、v2索引削除{}件。",
            receipt.records_changed, receipt.conversation_rows_deleted
        )),
        Line::from(format!(
            "操作ID: {} / revision {} → {}",
            receipt.operation_id, receipt.before_revision, receipt.after_revision
        )),
    ]
}

fn read_import(path: &Path, project: &str) -> Result<Vec<Record>, String> {
    let parent = path.parent().ok_or("取り込み元が不正です")?;
    let directory = open_directory_nofollow(parent)
        .map_err(|_| "取り込み元の親ディレクトリをnofollowで確認できません")?;
    let name = CString::new(path.file_name().ok_or("取り込み元が不正です")?.as_bytes())
        .map_err(|_| "取り込み元が不正です")?;
    let protection = polaris_auth::protection::with_protected_paths(Clone::clone)
        .map_err(|_| "保護されたファイルの一覧を確認できません")?;
    if protection.paths().contains(path) {
        return Err("保護されたファイルは取り込めません".into());
    }
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err("取り込み元を通常ファイルとして開けません".into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file()
        || meta.len() > MAX_CHANGE_BYTES as u64
        || protection
            .identities()
            .contains(&polaris_auth::protection::ProtectedIdentity {
                dev: meta.dev(),
                ino: meta.ino(),
            })
    {
        return Err("取り込み元が保護対象または8 MiBを超えています".into());
    }
    let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            named.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err("取り込み元を再確認できません".into());
    }
    let named = unsafe { named.assume_init() };
    if named.st_dev as u64 != meta.dev()
        || named.st_ino as u64 != meta.ino()
        || named.st_mode & libc::S_IFMT != libc::S_IFREG
    {
        return Err("取り込み元が変更されました".into());
    }
    let mut contents = String::new();
    file.take(MAX_CHANGE_BYTES as u64 + 1)
        .read_to_string(&mut contents)
        .map_err(|_| "取り込み元をUTF-8 JSONLとして読めません")?;
    if contents.len() > MAX_CHANGE_BYTES {
        return Err("取り込み元が8 MiBを超えています".into());
    }
    let mut records = Vec::new();
    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        if records.len() >= MAX_CHANGE_RECORDS {
            return Err("取り込みは1回100件までです".into());
        }
        let record: Record =
            serde_json::from_str(line).map_err(|_| "取り込み元に不正な記録JSONがあります")?;
        if record.project_id != project {
            return Err("別プロジェクトの記録は取り込めません".into());
        }
        records.push(record);
    }
    if records.is_empty() {
        return Err("取り込み元に記録がありません".into());
    }
    Ok(records)
}

#[allow(clippy::too_many_arguments)]
async fn embed_records<B: Backend, R: KeyReader>(
    owner: &MemoryChangeOwnerV1<'_>,
    store: &mut MemoryStore,
    project: &str,
    revision: u64,
    records: Vec<Record>,
    index: bool,
    configured: Option<(String, String)>,
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
) -> Result<Vec<Line<'static>>, String> {
    let (url,model)=configured.ok_or("埋め込みURLとモデルが設定されていません。--tool-memory-embedding-urlと--tool-memory-embedding-modelを指定して起動してください")?;
    let encoder = Encoder::new(&url, model.clone()).map_err(|e| e.to_string())?;
    let total = records
        .iter()
        .try_fold(0usize, |total, r| total.checked_add(r.text.len()))
        .ok_or("埋め込み本文が上限を超えました")?;
    if records.is_empty() || records.len() > MAX_CHANGE_RECORDS || total > MAX_CHANGE_BYTES {
        return Err("埋め込み本文は1回100件・8 MiBまでです".into());
    }
    let frozen_hashes: Vec<_> = records
        .iter()
        .map(|r| ManagementEffectHashV1::digest(r.text.as_bytes()))
        .collect();
    let target = if index {
        records[0].session_id.clone()
    } else {
        format!("{} / {}", records[0].session_id, records[0].id)
    };
    if !account::confirm(
        terminal,
        reader,
        if index {
            "C19 セッションの索引作成"
        } else {
            "C18 記録の埋め込み"
        },
        &target,
        &format!(
            "対象プロジェクト: {project}\n{}件・{total} bytesを設定済みループバックAPIへ送信します。モデル: {}\n取得したベクトルを保存します。通信の合計上限は120秒です。",
            records.len(),
            render::sanitize(&model)
        ),
    ) {
        return Ok(cancelled());
    }
    let updates = tokio::time::timeout(Duration::from_secs(120), async {
        let mut updates = Vec::with_capacity(records.len());
        for record in &records {
            updates.push(MemoryEmbeddingUpdateV1 {
                session_id: record.session_id.clone(),
                id: record.id.clone(),
                embedding: encoder
                    .encode(&record.text)
                    .await
                    .map_err(|e| e.to_string())?,
            });
        }
        Ok::<_, String>(updates)
    })
    .await
    .map_err(|_| "通信が120秒を超えました。メモリDBは変更していません")?
    .map_err(|e| format!("埋め込みを取得できません。メモリDBは変更していません: {e}"))?;
    if store.project_revision(project).map_err(|e| e.to_string())? != revision {
        return Err("記憶が更新されたため埋め込みを保存しません。新しい明示操作が必要です".into());
    }
    for (record, hash) in records.iter().zip(frozen_hashes) {
        let current = store
            .get(project, &record.session_id, &record.id)
            .map_err(|e| e.to_string())?
            .ok_or("対象の記録が変わりました")?;
        if current != *record || ManagementEffectHashV1::digest(current.text.as_bytes()) != hash {
            return Err("対象の記録が変わったため埋め込みを保存しません".into());
        }
    }
    let change = if index {
        MemoryChangeV1::Index { updates }
    } else {
        MemoryChangeV1::Embed { updates }
    };
    // Confirmation authorizes this frozen batch. Encoding is a read; the effect
    // is prepared before the atomic local mutation, with its actual vectors.
    let prepared = owner.prepare(store, new_request(project, revision, change)?)?;
    let receipt = owner
        .apply(store, &prepared, true)?
        .ok_or("メモリ操作が取り消されました")?;
    Ok(receipt_lines(&receipt, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, crossterm::event::KeyCode};
    use std::{
        collections::VecDeque,
        io::{Read, Write},
        net::TcpListener,
    };

    struct Reader(VecDeque<KeyCode>);
    impl KeyReader for Reader {
        fn read_key(&mut self) -> std::io::Result<KeyCode> {
            Ok(self.0.pop_front().unwrap_or(KeyCode::Esc))
        }
    }
    fn reader(key: KeyCode) -> Reader {
        Reader(VecDeque::from([key]))
    }
    fn record(project: &Path, session: &str, id: &str) -> Record {
        Record {
            project_id: project.to_str().unwrap().into(),
            session_id: session.into(),
            id: id.into(),
            role: "user".into(),
            timestamp: "2026-10-04".into(),
            source: "explicit-fixture".into(),
            text: "historical text only".into(),
        }
    }
    fn fixture(path: &Path, record: &Record) {
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string(record).unwrap()),
        )
        .unwrap();
    }
    fn loopback_encoder() -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let boundary = loop {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                    assert!(bytes.len() < 8192);
                    if bytes.ends_with(b"\r\n\r\n") {
                        break bytes.len();
                    }
                };
                let header = String::from_utf8_lossy(&bytes[..boundary]);
                let length = header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                let mut body = vec![0; length];
                socket.read_exact(&mut body).unwrap();
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(request["model"], "fixture-local");
                assert_eq!(request["input"], "historical text only");
                let body = r#"{"data":[{"embedding":[1.0,2.0]}]}"#;
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                socket.flush().unwrap();
            }
        });
        (format!("http://{address}/embeddings"), thread)
    }

    #[tokio::test]
    async fn m3c_memory_tui_operations_use_owner_confirmation_and_refusals() {
        const CHILD: &str = "POLARIS_M3C_MEMORY_TUI_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
            let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["memory_commands::tests::m3c_memory_tui_operations_use_owner_confirmation_and_refusals","--exact","--nocapture"]).env(CHILD,"1").env("HOME",home.path()).env_remove("POLARIS_DATA_DIR").output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        polaris_auth::protection::use_trusted_home(&home).unwrap();
        let project = home.join("project");
        std::fs::create_dir(&project).unwrap();
        let state = polaris_core::project::state_dir(&project).unwrap();
        let workspace = WorkspaceOwner::for_project(
            &project,
            polaris_core::rewind_workspace::RewindLimits::default(),
        )
        .unwrap();
        let lease = workspace.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        drop(lease);
        let terminal = RefCell::new(Terminal::new(TestBackend::new(140, 28)).unwrap());
        let source = project.join("records.jsonl");
        fixture(&source, &record(&project, "stored", "one"));
        let import = format!("import {}", source.display());
        let lines = handle(
            &import,
            &terminal,
            &mut reader(KeyCode::Char('y')),
            &workspace,
            &effects,
            &project,
            &state,
            Some("active"),
            None,
            false,
        )
        .await
        .unwrap();
        assert!(lines[0].to_string().contains("保存しました"));
        let store = MemoryStore::open_read_only(state.join("memory.sqlite3")).unwrap();
        assert!(
            store
                .get(project.to_str().unwrap(), "stored", "one")
                .unwrap()
                .is_some()
        );
        let before = store.project_revision(project.to_str().unwrap()).unwrap();
        let rejected = project.join("foreign.jsonl");
        fixture(&rejected, &record(&home, "stored", "foreign"));
        assert!(
            handle(
                &format!("import {}", rejected.display()),
                &terminal,
                &mut reader(KeyCode::Char('y')),
                &workspace,
                &effects,
                &project,
                &state,
                Some("active"),
                None,
                false
            )
            .await
            .is_err()
        );
        let linked = project.join("linked.jsonl");
        std::os::unix::fs::symlink(&source, &linked).unwrap();
        assert!(
            handle(
                &format!("import {}", linked.display()),
                &terminal,
                &mut reader(KeyCode::Char('y')),
                &workspace,
                &effects,
                &project,
                &state,
                Some("active"),
                None,
                false
            )
            .await
            .is_err()
        );
        assert_eq!(
            store.project_revision(project.to_str().unwrap()).unwrap(),
            before
        );
        assert!(
            handle(
                "embed stored one",
                &terminal,
                &mut reader(KeyCode::Char('y')),
                &workspace,
                &effects,
                &project,
                &state,
                Some("active"),
                None,
                false
            )
            .await
            .is_err()
        );
        let (url, server) = loopback_encoder();
        let configured = Some((url, "fixture-local".into()));
        let cancelled = handle(
            "embed stored one",
            &terminal,
            &mut reader(KeyCode::Esc),
            &workspace,
            &effects,
            &project,
            &state,
            Some("active"),
            configured.clone(),
            false,
        )
        .await
        .unwrap();
        assert!(cancelled[0].to_string().contains("取り消"));
        assert!(
            !store
                .has_embedding(project.to_str().unwrap(), "stored", "one", "fixture-local")
                .unwrap()
        );
        let embedded = handle(
            "embed stored one",
            &terminal,
            &mut reader(KeyCode::Char('y')),
            &workspace,
            &effects,
            &project,
            &state,
            Some("active"),
            configured.clone(),
            false,
        )
        .await
        .unwrap();
        assert!(embedded[0].to_string().contains("保存しました"));
        assert!(
            store
                .has_embedding(project.to_str().unwrap(), "stored", "one", "fixture-local")
                .unwrap()
        );
        let indexed = handle(
            "index stored --batch 1",
            &terminal,
            &mut reader(KeyCode::Char('y')),
            &workspace,
            &effects,
            &project,
            &state,
            Some("active"),
            configured.clone(),
            false,
        )
        .await
        .unwrap();
        assert!(indexed[0].to_string().contains("保存しました"));
        server.join().unwrap();
        assert!(
            handle(
                "index missing --batch 1",
                &terminal,
                &mut reader(KeyCode::Char('y')),
                &workspace,
                &effects,
                &project,
                &state,
                Some("active"),
                configured,
                false
            )
            .await
            .is_err()
        );
        assert!(
            handle(
                "forget active",
                &terminal,
                &mut reader(KeyCode::Char('y')),
                &workspace,
                &effects,
                &project,
                &state,
                Some("active"),
                None,
                false
            )
            .await
            .is_err()
        );
        let cancelled = handle(
            "forget stored",
            &terminal,
            &mut reader(KeyCode::Esc),
            &workspace,
            &effects,
            &project,
            &state,
            Some("active"),
            None,
            false,
        )
        .await
        .unwrap();
        assert!(cancelled[0].to_string().contains("取り消"));
        assert!(
            !store
                .is_session_forgotten(project.to_str().unwrap(), "stored")
                .unwrap()
        );
        let forgotten = handle(
            "forget stored",
            &terminal,
            &mut reader(KeyCode::Char('y')),
            &workspace,
            &effects,
            &project,
            &state,
            Some("active"),
            None,
            false,
        )
        .await
        .unwrap();
        assert!(forgotten[0].to_string().contains("削除しました"));
        assert!(
            store
                .is_session_forgotten(project.to_str().unwrap(), "stored")
                .unwrap()
        );
        assert!(
            store
                .get(project.to_str().unwrap(), "stored", "one")
                .unwrap()
                .is_none()
        );
        assert!(effects.blocker().unwrap().is_none());
        let screen = {
            let term = terminal.borrow();
            let buffer = term.backend().buffer();
            (0..buffer.area.height)
                .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol()))
                .collect::<String>()
        };
        assert!(!screen.contains("historical text only"));
        assert!(!screen.contains('\x1b'));
    }
}
