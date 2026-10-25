//! Windows read-only TUI interaction, secret scrubbing and terminal restoration tests.
use super::*;
use ratatui::backend::TestBackend;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn enter() -> Event {
    key(KeyCode::Enter)
}

fn contents(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            assert!(!kind.is_symlink());
            let relative = path.strip_prefix(root).unwrap().to_owned();
            if kind.is_dir() {
                result.insert(relative, None);
                visit(root, &path, result);
            } else {
                result.insert(relative, Some(fs::read(path).unwrap()));
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

fn screen(terminal: &ratatui::Terminal<TestBackend>) -> String {
    use unicode_width::UnicodeWidthStr;

    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for row in buffer.content.chunks(usize::from(buffer.area.width).max(1)) {
        let mut column = 0;
        while column < row.len() {
            let symbol = row[column].symbol();
            text.push_str(symbol);
            column += symbol.width().max(1);
        }
        text.push('\n');
    }
    text
}

#[test]
fn windows_read_only_loop_displays_commands_without_creating_state() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".polaris")).unwrap();
    fs::write(root.path().join(".polaris/config.toml"), b"[invalid config").unwrap();
    fs::write(root.path().join("project.rs"), b"project before").unwrap();
    let before = contents(root.path());
    let mut view = ReadOnlyView::new(root.path().to_owned());
    let mut terminal = ratatui::Terminal::new(TestBackend::new(100, 32)).unwrap();
    let mut events = VecDeque::new();
    for text in [
        "/help",
        "/status",
        "/doctor",
        "/pwd",
        "/decided list",
        "/compare",
        "/quit",
    ] {
        events.push_back(Event::Paste(text.into()));
        events.push_back(enter());
    }
    run_loop(&mut terminal, &mut view, || {
        events
            .pop_front()
            .ok_or_else(|| io::Error::other("missing quit"))
    })
    .unwrap();
    assert!(events.is_empty());
    let lines = view
        .lines
        .iter()
        .map(|line| line.line.to_string())
        .collect::<Vec<_>>();
    assert!(lines.iter().any(|line| line.contains("/help")));
    assert!(
        lines
            .iter()
            .any(|line| line == "/phase — show phase availability (read-only)")
    );
    assert!(
        lines
            .iter()
            .any(|line| line == "/workflow — show workflow availability (read-only)")
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("conversation, storage: not started"))
    );
    assert!(lines.iter().any(|line| line.contains("No records")));
    assert!(lines.iter().any(|line| line.contains("folder1")));
    assert_eq!(contents(root.path()), before);
}

#[test]
fn windows_read_only_loop_preserves_mutation_and_unavailable_reader_drafts() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("journal.json"), b"journal before").unwrap();
    let before = contents(root.path());
    let mut view = ReadOnlyView::new(root.path().to_owned());
    view.history.record("earlier command");
    for text in [
        "実行してください",
        "/review",
        "/clear",
        "/init",
        "/export file.md",
        "/model",
        "/resume",
        "/sync apply",
        "/mcp list",
        "/memory search fixture",
    ] {
        view.buffer = text.into();
        view.cursor = text.chars().next().unwrap().len_utf8();
        let draft = (view.buffer.clone(), view.cursor);
        assert!(!view.event(enter()));
        assert_eq!((&view.buffer, view.cursor), (&draft.0, draft.1));
        assert!(view.lines.is_empty());
        assert!(matches!(&view.status, Status::Error(_)));
    }
    let draft = (view.buffer.clone(), view.cursor);
    assert!(!view.event(Event::Key(KeyEvent::new(
        KeyCode::Char('o'),
        KeyModifiers::CONTROL
    ))));
    assert_eq!((view.buffer.clone(), view.cursor), draft);
    assert_eq!(view.history.older(""), Some("earlier command"));
    assert_eq!(view.history.older(""), None);
    assert_eq!(contents(root.path()), before);
}

#[test]
fn windows_read_only_loop_scrubs_secrets_before_display_or_recall() {
    let root = tempfile::tempdir().unwrap();
    let mut view = ReadOnlyView::new(root.path().to_owned());
    let mut terminal = ratatui::Terminal::new(TestBackend::new(100, 24)).unwrap();
    assert!(!view.event(Event::Paste("/mcp secret set SYNTHETIC_VALUE".into())));
    assert_eq!(view.buffer, "/mcp secret set");
    view.draw(&mut terminal).unwrap();
    assert!(!screen(&terminal).contains("SYNTHETIC_VALUE"));
    assert!(!view.event(enter()));
    assert!(view.buffer.is_empty());
    assert!(view.history.older("").is_none());
    assert!(view.lines.is_empty());
}

#[test]
fn windows_read_only_popup_and_resize_keep_read_only_presentation() {
    let mut view = ReadOnlyView::new(PathBuf::from("C:\\project\\line\n\t\u{1b}"));
    assert!(!view.event(Event::Paste("/sta".into())));
    assert_eq!(view.suggestions().len(), 1);
    assert!(!view.event(key(KeyCode::Down)));
    assert!(!view.event(enter()));
    assert!(view.buffer.is_empty());
    assert!(
        view.lines
            .iter()
            .any(|line| line.line.to_string() == "/status")
    );
    for width in [18, 80, 120] {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(width, 32)).unwrap();
        assert!(!view.event(Event::Resize(width, 32)));
        view.draw(&mut terminal).unwrap();
        let text = screen(&terminal);
        assert!(!text.contains("Ask polaris to do anything"));
        assert!(!text.contains("/model to change"));
        assert!(!text.contains('\u{1b}'));
        if width >= 80 {
            assert!(
                text.contains("Windows browsing only"),
                "width={width}: {text:?}"
            );
            assert!(text.contains("C:\\project\\line\\n\\t"));
        }
    }
    for (name, description) in [
        ("phase", "show phase availability (read-only)"),
        ("workflow", "show workflow availability (read-only)"),
    ] {
        view.buffer = format!("/{name}");
        view.cursor = view.buffer.len();
        let suggestions = view.suggestions();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].description, description);
        let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 32)).unwrap();
        view.draw(&mut terminal).unwrap();
        assert!(screen(&terminal).contains(description));
    }
}

#[test]
fn windows_read_only_exit_obeys_key_modifiers_and_preserves_hidden_draft() {
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let mut pasted = ReadOnlyView::new(PathBuf::from("C:\\fixture"));
    assert!(!pasted.event(Event::Key(ctrl_c)));
    assert!(!pasted.event(Event::Paste("new draft".into())));
    assert!(!pasted.event(Event::Key(ctrl_c)));
    assert_eq!(pasted.buffer, "new draft");
    assert!(pasted.event(Event::Key(ctrl_c)));

    let mut view = ReadOnlyView::new(PathBuf::from("C:\\fixture"));
    view.history.record("/quit");
    assert_eq!(view.history.older("unsent draft"), Some("/quit"));
    view.buffer = "/quit".into();
    view.cursor = view.buffer.len();
    assert!(!view.event(enter()));
    assert_eq!(view.buffer, "/quit");
    assert!(!view.event(Event::Key(ctrl_c)));
    let mut released = ctrl_c;
    released.kind = KeyEventKind::Release;
    assert!(!view.event(Event::Key(released)));
    assert!(view.event(Event::Key(ctrl_c)));
}

#[test]
fn windows_read_only_terminal_acquisition_rolls_back_only_acquired_steps() {
    for failure in 0..3 {
        let step_index = Cell::new(0);
        let restored = RefCell::new(Vec::new());
        let result = enter_terminal_with(
            |_| {
                let index = step_index.get();
                step_index.set(index + 1);
                if index == failure {
                    Err(io::Error::other("acquire failed"))
                } else {
                    Ok(())
                }
            },
            || Err::<(), _>(io::Error::other("construct failed")),
            |step| {
                restored.borrow_mut().push(step);
                Ok(())
            },
        );
        assert!(result.is_err());
        let expected = match failure {
            0 => vec![],
            1 => vec![TerminalStep::Alternate],
            _ => vec![TerminalStep::Raw, TerminalStep::Alternate],
        };
        assert_eq!(*restored.borrow(), expected);
    }
}

#[test]
fn windows_read_only_loop_errors_restore_terminal_and_keep_primary_error() {
    for fail_draw in [false, true] {
        let restored = RefCell::new(Vec::new());
        let (mut guard, ()) = enter_terminal_with(|_| Ok(()), || Ok(()), |_| Ok(())).unwrap();
        guard.mouse = true;
        guard.cursor = true;
        let mut view = ReadOnlyView::new(PathBuf::from("C:\\fixture"));
        let read_called = Cell::new(false);
        let operation = run_loop_with_draw(
            &mut view,
            |_| {
                if fail_draw {
                    Err("draw failed".into())
                } else {
                    Ok(())
                }
            },
            || {
                read_called.set(true);
                Err(io::Error::other("read failed"))
            },
        );
        assert_eq!(read_called.get(), !fail_draw);
        let result = linux_finish_guarded_terminal(operation, &mut guard, |step| {
            restored.borrow_mut().push(step);
            Err(io::Error::other("restore failed"))
        });
        let error = result.unwrap_err();
        assert!(error.contains(if fail_draw {
            "draw failed"
        } else {
            "read failed"
        }));
        assert!(error.contains("restoration failed"));
        assert_eq!(
            *restored.borrow(),
            vec![
                TerminalStep::Cursor,
                TerminalStep::Mouse,
                TerminalStep::Raw,
                TerminalStep::Alternate,
            ]
        );
        guard
            .restore_with(|_| panic!("cleanup must not repeat"))
            .unwrap();
    }
}

#[test]
fn windows_read_only_normal_exit_restores_the_acquired_terminal() {
    let restored = RefCell::new(Vec::new());
    let (mut guard, ()) = enter_terminal_with(|_| Ok(()), || Ok(()), |_| Ok(())).unwrap();
    let mut terminal = ratatui::Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut view = ReadOnlyView::new(PathBuf::from("C:\\fixture"));
    let mut events = VecDeque::from([Event::Paste("/quit".into()), enter()]);
    let result = run_loop(&mut terminal, &mut view, || {
        events
            .pop_front()
            .ok_or_else(|| io::Error::other("missing quit"))
    });
    linux_finish_guarded_terminal(result, &mut guard, |step| {
        restored.borrow_mut().push(step);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        *restored.borrow(),
        vec![TerminalStep::Raw, TerminalStep::Alternate]
    );
}

#[test]
fn windows_read_only_status_echo_renders_windows_paths_and_controls() {
    for (command, expected) in [
        (r"/status C:\", r"/status C:\"),
        (
            r"/status C:\work\src\main.rs",
            r"/status C:\work\src\main.rs",
        ),
        (r"/status \\server\share\dir", r"/status \\server\share\dir"),
        (
            r"/status D:\日本語 空白\folder",
            r"/status D:\日本語 空白\folder",
        ),
        ("/status before\rafter", "/status before�after"),
        ("/status before\nafter", r"/status before\nafter"),
        ("/status before\tafter", r"/status before\tafter"),
        ("/status before\u{1b}[2Jafter", "/status before�[2Jafter"),
    ] {
        let mut view = ReadOnlyView::new(PathBuf::from(r"C:\synthetic\readonly"));
        assert!(!view.event(Event::Paste(command.into())));
        assert_eq!(view.buffer, command);
        assert!(!view.event(enter()));
        assert!(view.buffer.is_empty());
        assert_eq!(view.cursor, 0);
        assert_eq!(view.lines[0].line.to_string(), expected, "{command:?}");
        assert!(view.lines[0].shaded);
        assert_eq!(view.lines[1].line.to_string(), "Windows browsing only");

        let mut terminal = ratatui::Terminal::new(TestBackend::new(160, 32)).unwrap();
        view.draw(&mut terminal).unwrap();
        let rendered = screen(&terminal);
        assert!(
            rendered.lines().any(|line| line.trim_end() == expected),
            "{command:?}: {rendered:?}",
        );
        for control in ['\r', '\t', '\u{1b}'] {
            assert!(!rendered.contains(control), "{command:?}: {rendered:?}");
        }
        assert_eq!(view.history.older(""), Some(command));
    }
}
