//! Dedicated MCP credential editing never returns entered bytes to the composer.
use super::{journal::Journal, *};
use crate::approver::SecretInputEvent;
use polaris_core::mcp::credentials::{
    McpCredentialErrorV1, McpCredentialOwnerV1, McpSecretDeleteRequestV1, McpSecretReferenceV1,
    McpSecretSetRequestV1,
};
use polaris_mcp::{secret_input::SecretInputV1, store::Store};
use ratatui::{
    crossterm::event::KeyCode,
    widgets::{Block, Borders, Paragraph},
};

fn draw<B: Backend>(
    terminal: &RefCell<Terminal<B>>,
    title: &str,
    lines: Vec<Line<'static>>,
) -> Result<(), String> {
    terminal
        .borrow_mut()
        .draw(|frame| {
            frame.render_widget(
                Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
                frame.area(),
            );
        })
        .map(|_| ())
        .map_err(|_| "MCP秘密入力画面を表示できません".into())
}

fn read_reference<B: Backend, R: KeyReader>(
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
) -> Result<Option<McpSecretReferenceV1>, String> {
    let mut reference = String::new();
    loop {
        draw(
            terminal,
            "MCP認証情報の参照名",
            vec![
                Line::from("値の入力は次の専用画面です。この画面には参照名だけ入力します。"),
                Line::from("英数字・-・_、1〜64バイト。Enter: 次へ / Esc: 取消"),
                Line::from(reference.clone()),
            ],
        )?;
        match reader.read_secret_input() {
            Ok(SecretInputEvent::Key(KeyCode::Esc)) => return Ok(None),
            Err(_) => return Err("MCP参照名の入力を取り消しました".into()),
            Ok(SecretInputEvent::Key(KeyCode::Enter)) if !reference.is_empty() => {
                return McpSecretReferenceV1::parse(reference)
                    .map(Some)
                    .map_err(|_| "MCP参照名が無効です".into());
            }
            Ok(SecretInputEvent::Key(KeyCode::Backspace)) => {
                reference.pop();
            }
            Ok(SecretInputEvent::Key(KeyCode::Char(character)))
                if reference.len() < 64
                    && (character.is_ascii_alphanumeric() || matches!(character, '-' | '_')) =>
            {
                reference.push(character);
            }
            Ok(SecretInputEvent::Paste(value)) => {
                if reference.len().saturating_add(value.len()) > 64
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                {
                    return Err("貼り付けた参照名が無効です。英数字・-・_で指定してください".into());
                }
                reference.push_str(&value);
            }
            _ => {}
        }
    }
}

pub(super) fn read_value<B: Backend, R: KeyReader>(
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
) -> Result<Option<SecretInputV1>, String> {
    let mut input = SecretInputV1::new();
    let rejection = 'input: loop {
        // The frame is identical for all values and lengths, including empty input.
        draw(
            terminal,
            "MCP認証情報の秘密入力",
            vec![
                Line::from("値と文字数は表示しません。通常の入力履歴には保存しません。"),
                Line::from("Enter: 入力完了 / Backspace: 末尾を削除 / Esc・Ctrl-C: 取消"),
            ],
        )?;
        match reader.read_secret_input() {
            Ok(SecretInputEvent::Key(KeyCode::Esc)) => return Ok(None),
            Err(_) => return Err("MCP秘密入力を取り消しました".into()),
            Ok(SecretInputEvent::Key(KeyCode::Enter)) if !input.is_empty() => {
                return Ok(Some(input));
            }
            Ok(SecretInputEvent::Key(KeyCode::Backspace)) => {
                input.pop();
            }
            Ok(SecretInputEvent::Key(KeyCode::Char(character))) if !character.is_control() => {
                if input.push(character).is_err() {
                    break "MCP秘密入力の上限を超えました";
                }
            }
            Ok(SecretInputEvent::Paste(value)) => {
                if value.chars().any(char::is_control) {
                    break "MCP秘密入力に改行・制御文字は使用できません";
                }
                for character in value.chars() {
                    if input.push(character).is_err() {
                        break 'input "MCP秘密入力の上限を超えました";
                    }
                }
            }
            _ => {}
        }
    };
    input.cancel();
    // Keep the secret reader until its submission/cancellation boundary. A
    // queued suffix must never become ordinary composer input after an error.
    loop {
        draw(
            terminal,
            "MCP認証情報の秘密入力",
            vec![
                Line::from(rejection),
                Line::from("入力を破棄しました。Enter・Esc・Ctrl-C: 閉じる"),
            ],
        )?;
        match reader.read_secret_input() {
            Ok(SecretInputEvent::Key(KeyCode::Enter | KeyCode::Esc)) | Err(_) => {
                return Err(rejection.into());
            }
            _ => {}
        }
    }
}

fn revision(host: &LocalHost) -> Result<u64, String> {
    if !Store::exists(host.account_home()) {
        return Ok(0);
    }
    let store = Store::open(host.account_home()).map_err(|_| "MCP保存先を確認できません")?;
    McpCredentialOwnerV1::new(&store)
        .current_secret_revision()
        .map_err(|_| "MCP認証情報の版を確認できません".into())
}

fn credential_error(
    journal: &Journal<'_>,
    started: &polaris_core::management_effect::StartedManagementEffectV1,
    error: McpCredentialErrorV1,
) -> String {
    if error == McpCredentialErrorV1::Storage {
        return journal.unknown(started);
    }
    // These closed owner errors occur before a write. Record that terminal
    // result rather than leaving a known refusal as an unresolved operation.
    if let Err(error) = journal.complete(
        started,
        format!("mcp-credential-refused:{error}").as_bytes(),
    ) {
        return error;
    }
    match error {
        McpCredentialErrorV1::RevisionConflict => {
            "確認中にMCP認証情報が変更されました。保存していません"
        }
        McpCredentialErrorV1::Auth => "MCP認証情報が参照されているか、保存条件を満たしていません",
        _ => "MCP認証情報の変更が拒否されました",
    }
    .into()
}

pub(super) fn handle<B: Backend, R: KeyReader>(
    parts: &[&str],
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
    host: &LocalHost,
    journal: &Journal<'_>,
    approval_policy: ApprovalPolicy,
) -> Result<Vec<Line<'static>>, String> {
    if approval_policy == ApprovalPolicy::Never {
        return Err("現在の承認設定ではMCP認証情報を変更できません".into());
    }
    match parts {
        ["set"] => {
            let expected = revision(host)?;
            let Some(reference) = read_reference(terminal, reader)? else {
                return Ok(vec![Line::from("MCP秘密入力を取り消しました")]);
            };
            let Some(input) = read_value(terminal, reader)? else {
                return Ok(vec![Line::from("MCP秘密入力を取り消しました")]);
            };
            let prepared = journal.prepare(
                "C24",
                expected,
                format!("mcp-secret-set\0{}\0{expected}", reference.as_str()).as_bytes(),
            )?;
            if !confirm(
                terminal,
                reader,
                "MCP認証情報を保存",
                reference.as_str(),
                &format!("認証情報の版 {expected}。値は表示しません。次の実行から反映します。巻き戻しでは取り消せません。"),
            ) {
                journal.deny(&prepared)?;
                return Ok(vec![Line::from("MCP認証情報の保存を取り消しました")]);
            }
            let started = journal.start(&prepared)?;
            let store = Store::open(host.account_home()).map_err(|_| journal.unknown(&started))?;
            let changed = McpCredentialOwnerV1::new(&store)
                .set(McpSecretSetRequestV1::new(expected, reference, input))
                .map_err(|error| credential_error(journal, &started, error))?;
            journal.complete(&started, format!("mcp-secret-set:{}", changed.secret_revision).as_bytes())?;
            Ok(vec![Line::from(format!("MCP認証情報を保存しました（版 {}）。次の実行から反映します。", changed.secret_revision))])
        }
        ["delete", reference] => {
            let reference = McpSecretReferenceV1::parse(*reference)
                .map_err(|_| "MCP参照名が無効です")?;
            let store = host.store().ok_or("保存されたMCP認証情報がありません")?;
            let owner = McpCredentialOwnerV1::new(store);
            let expected = owner.current_secret_revision().map_err(|_| "MCP認証情報の版を確認できません")?;
            let exists = store.load_secrets().map_err(|_| "MCP認証情報を確認できません")?
                .is_some_and(|secrets| secrets.references().any(|name| name == reference.as_str()));
            if !exists {
                return Err("そのMCP参照名は保存されていません".into());
            }
            let prepared = journal.prepare("C25", expected, format!("mcp-secret-delete\0{}\0{expected}", reference.as_str()).as_bytes())?;
            if !confirm(terminal, reader, "MCP認証情報を削除", reference.as_str(), &format!("認証情報の版 {expected}。参照中の値は削除できません。巻き戻しでは復元できません。")) {
                journal.deny(&prepared)?;
                return Ok(vec![Line::from("MCP認証情報の削除を取り消しました")]);
            }
            let started = journal.start(&prepared)?;
            let changed = owner.delete(McpSecretDeleteRequestV1::new(expected, reference))
                .map_err(|error| credential_error(journal, &started, error))?;
            journal.complete(&started, format!("mcp-secret-delete:{}", changed.secret_revision).as_bytes())?;
            Ok(vec![Line::from(format!("MCP認証情報を削除しました（版 {}）。", changed.secret_revision))])
        }
        _ => Err("/mcp secret set は引数なしで専用入力を開きます。削除は /mcp secret delete <参照名>。値を通常入力に書かないでください。".into()),
    }
}
