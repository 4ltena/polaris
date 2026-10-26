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
        .map_err(|_| "Cannot display the MCP secret input screen".into())
}

fn read_reference<B: Backend, R: KeyReader>(
    terminal: &RefCell<Terminal<B>>,
    reader: &mut R,
) -> Result<Option<McpSecretReferenceV1>, String> {
    let mut reference = String::new();
    loop {
        draw(
            terminal,
            "MCP credential reference name",
            vec![
                Line::from(
                    "Enter only the reference name here. The value is entered on the next dedicated screen.",
                ),
                Line::from("1–64 bytes: letters, digits, -, _. Enter: next / Esc: cancel"),
                Line::from(reference.clone()),
            ],
        )?;
        match reader.read_secret_input() {
            Ok(SecretInputEvent::Key(KeyCode::Esc)) => return Ok(None),
            Err(_) => return Err("MCP reference name input cancelled".into()),
            Ok(SecretInputEvent::Key(KeyCode::Enter)) if !reference.is_empty() => {
                return McpSecretReferenceV1::parse(reference)
                    .map(Some)
                    .map_err(|_| "Invalid MCP reference name".into());
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
                    return Err(
                        "Invalid pasted reference name. Use letters, digits, -, or _".into(),
                    );
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
            "MCP credential secret input",
            vec![
                Line::from(
                    "The value and character count are hidden. Nothing is saved to normal input history.",
                ),
                Line::from(
                    "Enter: finish / Backspace: delete last character / Esc or Ctrl-C: cancel",
                ),
            ],
        )?;
        match reader.read_secret_input() {
            Ok(SecretInputEvent::Key(KeyCode::Esc)) => return Ok(None),
            Err(_) => return Err("MCP secret input cancelled".into()),
            Ok(SecretInputEvent::Key(KeyCode::Enter)) if !input.is_empty() => {
                return Ok(Some(input));
            }
            Ok(SecretInputEvent::Key(KeyCode::Backspace)) => {
                input.pop();
            }
            Ok(SecretInputEvent::Key(KeyCode::Char(character))) if !character.is_control() => {
                if input.push(character).is_err() {
                    break "MCP secret input exceeds the limit";
                }
            }
            Ok(SecretInputEvent::Paste(value)) => {
                if value.chars().any(char::is_control) {
                    break "MCP secret input cannot contain newlines or control characters";
                }
                for character in value.chars() {
                    if input.push(character).is_err() {
                        break 'input "MCP secret input exceeds the limit";
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
            "MCP credential secret input",
            vec![
                Line::from(rejection),
                Line::from("Input discarded. Enter, Esc, or Ctrl-C: close"),
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
    let store = Store::open(host.account_home()).map_err(|_| "Cannot verify MCP storage")?;
    McpCredentialOwnerV1::new(&store)
        .current_secret_revision()
        .map_err(|_| "Cannot verify the MCP credential revision".into())
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
            "MCP credentials changed during confirmation. Nothing was saved"
        }
        McpCredentialErrorV1::Auth => {
            "MCP credentials are referenced or do not meet storage requirements"
        }
        _ => "MCP credential change denied",
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
        return Err("Current approval settings do not permit MCP credential changes".into());
    }
    match parts {
        ["set"] => {
            let expected = revision(host)?;
            let Some(reference) = read_reference(terminal, reader)? else {
                return Ok(vec![Line::from("MCP secret input cancelled")]);
            };
            let Some(input) = read_value(terminal, reader)? else {
                return Ok(vec![Line::from("MCP secret input cancelled")]);
            };
            let prepared = journal.prepare(
                "C24",
                expected,
                format!("mcp-secret-set\0{}\0{expected}", reference.as_str()).as_bytes(),
            )?;
            if !confirm(
                terminal,
                reader,
                "Save MCP credentials",
                reference.as_str(),
                &format!("Credential revision {expected}. The value is hidden. Effective from the next run; rewind cannot undo this."),
            ) {
                journal.deny(&prepared)?;
                return Ok(vec![Line::from("MCP credential save cancelled")]);
            }
            let started = journal.start(&prepared)?;
            let store = Store::open(host.account_home()).map_err(|_| journal.unknown(&started))?;
            let changed = McpCredentialOwnerV1::new(&store)
                .set(McpSecretSetRequestV1::new(expected, reference, input))
                .map_err(|error| credential_error(journal, &started, error))?;
            journal.complete(&started, format!("mcp-secret-set:{}", changed.secret_revision).as_bytes())?;
            Ok(vec![Line::from(format!("MCP credentials saved (revision {}). Effective from the next run.", changed.secret_revision))])
        }
        ["delete", reference] => {
            let reference = McpSecretReferenceV1::parse(*reference)
                .map_err(|_| "Invalid MCP reference name")?;
            let store = host.store().ok_or("No saved MCP credentials")?;
            let owner = McpCredentialOwnerV1::new(store);
            let expected = owner.current_secret_revision().map_err(|_| "Cannot verify the MCP credential revision")?;
            let exists = store.load_secrets().map_err(|_| "Cannot verify MCP credentials")?
                .is_some_and(|secrets| secrets.references().any(|name| name == reference.as_str()));
            if !exists {
                return Err("That MCP reference name is not saved".into());
            }
            let prepared = journal.prepare("C25", expected, format!("mcp-secret-delete\0{}\0{expected}", reference.as_str()).as_bytes())?;
            if !confirm(terminal, reader, "Delete MCP credentials", reference.as_str(), &format!("Credential revision {expected}. Referenced values cannot be deleted. Rewind cannot restore this.")) {
                journal.deny(&prepared)?;
                return Ok(vec![Line::from("MCP credential deletion cancelled")]);
            }
            let started = journal.start(&prepared)?;
            let changed = owner.delete(McpSecretDeleteRequestV1::new(expected, reference))
                .map_err(|error| credential_error(journal, &started, error))?;
            journal.complete(&started, format!("mcp-secret-delete:{}", changed.secret_revision).as_bytes())?;
            Ok(vec![Line::from(format!("MCP credentials deleted (revision {}).", changed.secret_revision))])
        }
        _ => Err("/mcp secret set opens dedicated input without arguments. Delete with /mcp secret delete <reference>. Do not type values into normal input.".into()),
    }
}

#[cfg(test)]
mod screen_copy_tests {
    use super::*;
    struct Keys(std::collections::VecDeque<KeyCode>);
    impl KeyReader for Keys {
        fn read_key(&mut self) -> std::io::Result<KeyCode> {
            Ok(self.0.pop_front().unwrap())
        }
    }
    #[test]
    fn typed_secret_never_reaches_visible_screen_copy() {
        let canary = "not-rendered-secret-canary";
        let terminal = RefCell::new(crate::terminal::test_terminal(120, 15));
        let mut keys = Keys(
            canary
                .chars()
                .map(KeyCode::Char)
                .chain([KeyCode::Esc])
                .collect(),
        );
        assert!(read_value(&terminal, &mut keys).unwrap().is_none());
        let copied = crate::terminal::test_copy_all(&mut terminal.borrow_mut());
        assert!(copied.contains("The value and character count are hidden"));
        assert!(!copied.contains(canary));
        assert!(!copied.contains(&canary.len().to_string()));
    }
}
