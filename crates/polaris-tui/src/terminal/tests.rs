use super::*;
use event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent};
use ratatui::{backend::TestBackend, widgets::Paragraph};
fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    })
}
#[test]
fn replacing_a_terminal_clears_old_copy_source_and_old_drop_keeps_new_owner() {
    let mut old = test_terminal(30, 4);
    old.backend().activate(false);
    old.draw(|f| f.render_widget(Paragraph::new("old-ephemeral-url"), f.area()))
        .unwrap();
    let mut new = test_terminal(30, 4);
    new.backend().activate(false);
    assert!(old.backend().screen.borrow().base.area.is_empty());
    new.draw(|f| f.render_widget(Paragraph::new("new visible page"), f.area()))
        .unwrap();
    let id = new.backend().screen.borrow().id;
    drop(old);
    assert!(CURRENT.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|(current, _)| *current == id)
    }));
    assert!(test_copy_all(&mut new).contains("new visible page"));
    assert!(!test_copy_all(&mut new).contains("old-ephemeral"));
    drop(new);
    assert!(CURRENT.with(|slot| slot.borrow().is_none()));
}
#[test]
fn copy_failure_is_static_and_never_aborts_the_current_ui_owner() {
    let mut terminal = test_terminal(30, 4);
    terminal
        .draw(|f| f.render_widget(Paragraph::new("visible synthetic"), f.area()))
        .unwrap();
    let mut screen = terminal.backend_mut().screen.borrow_mut();
    screen
        .handle(
            &mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            |_| panic!(),
        )
        .unwrap();
    assert!(
        screen
            .handle(
                &mouse(MouseEventKind::Up(MouseButton::Left), 7, 0),
                |text| {
                    assert_eq!(text, "visible");
                    Err("do not echo user clipboard data".into())
                }
            )
            .unwrap()
    );
    assert_eq!(
        screen.notice.map(|(message, _)| message),
        Some("Copy failed")
    );
}

#[test]
fn displayed_copy_notice_can_itself_be_selected_without_copying_underlying_text() {
    let mut terminal = test_terminal(30, 4);
    terminal
        .draw(|f| f.render_widget(Paragraph::new("body"), f.area()))
        .unwrap();
    let mut screen = terminal.backend_mut().screen.borrow_mut();
    screen.notice = Some(("Copy failed", Instant::now()));
    screen.paint(true).unwrap();
    let mut copied = String::new();
    screen
        .handle(
            &mouse(MouseEventKind::Down(MouseButton::Left), 19, 3),
            |_| panic!(),
        )
        .unwrap();
    screen
        .handle(
            &mouse(MouseEventKind::Up(MouseButton::Left), 30, 3),
            |text| {
                copied = text.into();
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(copied, "Copy failed");
}

#[test]
fn wide_transitions_and_overlay_restore_keep_base_physical_screen_and_copy_consistent() {
    let mut terminal = test_terminal(16, 4);
    terminal
        .draw(|f| f.render_widget(Paragraph::new("aa"), f.area()))
        .unwrap();
    terminal
        .draw(|f| f.render_widget(Paragraph::new("日"), f.area()))
        .unwrap();
    assert_eq!(test_copy_all(&mut terminal).trim(), "日");
    terminal.backend_mut().screen.borrow_mut().notice = None;
    terminal
        .draw(|f| f.render_widget(Paragraph::new("abc"), f.area()))
        .unwrap();
    assert_eq!(test_copy_all(&mut terminal).trim(), "abc");
    let mut s = terminal.backend_mut().screen.borrow_mut();
    s.notice = None;
    s.selection.clear();
    s.paint(true).unwrap();
    s.inner.set_cursor_position(Position::new(8, 2)).unwrap();
    s.handle(
        &mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
        |_| panic!(),
    )
    .unwrap();
    s.handle(
        &mouse(MouseEventKind::Drag(MouseButton::Left), 3, 0),
        |_| panic!(),
    )
    .unwrap();
    assert!(
        s.inner.buffer()[(2, 0)]
            .modifier
            .contains(ratatui::style::Modifier::REVERSED)
    );
    s.handle(
        &mouse(MouseEventKind::Drag(MouseButton::Left), 1, 0),
        |_| panic!(),
    )
    .unwrap();
    assert!(
        !s.inner.buffer()[(2, 0)]
            .modifier
            .contains(ratatui::style::Modifier::REVERSED)
    );
    assert_eq!(s.inner.get_cursor_position().unwrap(), Position::new(8, 2));
    drop(s);
    terminal
        .draw(|f| f.render_widget(Paragraph::new("XYZ"), f.area()))
        .unwrap();
    let mut s = terminal.backend_mut().screen.borrow_mut();
    let mut copied = String::new();
    s.handle(
        &mouse(MouseEventKind::Up(MouseButton::Left), 2, 0),
        |text| {
            copied = text.into();
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(copied, "XY");
    s.handle(
        &Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
        |_| panic!(),
    )
    .unwrap();
    assert_eq!(s.inner.buffer(), &s.base);
}
#[test]
fn resize_clear_and_new_page_do_not_copy_old_text() {
    let mut terminal = test_terminal(20, 4);
    terminal
        .draw(|f| f.render_widget(Paragraph::new("old-private-url"), f.area()))
        .unwrap();
    assert!(test_copy_all(&mut terminal).contains("old-private-url"));
    terminal
        .backend_mut()
        .screen
        .borrow_mut()
        .inner
        .resize(10, 4);
    terminal.resize(Rect::new(0, 0, 10, 4)).unwrap();
    terminal
        .draw(|f| f.render_widget(Paragraph::new("new"), f.area()))
        .unwrap();
    let text = test_copy_all(&mut terminal);
    assert!(text.contains("new"));
    assert!(!text.contains("old-private"));
    terminal.clear().unwrap();
    assert!(test_copy_all(&mut terminal).trim().is_empty());
}
#[test]
fn actual_onboarding_picker_approval_and_main_frames_are_copyable() {
    type Case = (
        &'static str,
        Box<dyn FnMut(&mut ratatui::Frame)>,
        &'static str,
    );
    let cases: Vec<Case> = vec![
        (
            "onboarding",
            Box::new(|f| {
                crate::onboarding::render_choice_screen(f, crate::onboarding::Choice::ChatGpt)
            }),
            "Sign in with ChatGPT",
        ),
        (
            "registered-onboarding",
            Box::new(crate::onboarding::render_registered_wait),
            "Authorize in your browser",
        ),
        (
            "api-key",
            Box::new(|f| crate::onboarding::render_api_key_prompt(f, 12)),
            "************",
        ),
        (
            "model",
            Box::new(|f| crate::render::render_model_picker(f, "gpt-6.1-sol", false, 0)),
            "gpt-6.1-sol",
        ),
        (
            "resume",
            Box::new(|f| crate::render::render_resume_picker(f, &[], 0, 0)),
            "no saved conversations",
        ),
        (
            "approval",
            Box::new(|f| crate::render::render_approval_modal(f, "synthetic operation 日本語")),
            "synthetic operation 日本語",
        ),
        (
            "management",
            Box::new(|f| {
                crate::render::render_management_confirmation(
                    f,
                    "operation",
                    "target",
                    "synthetic detail",
                );
            }),
            "synthetic detail",
        ),
        (
            "main",
            Box::new(|f| {
                let history = vec![crate::render::HistoryLine {
                    line: ratatui::text::Line::from("history 日本語 /help /status"),
                    shaded: false,
                }];
                let mut cache = crate::render::HistoryWrapCache::default();
                crate::draw_frame(
                    f,
                    &history,
                    &mut cache,
                    0,
                    "input 日本語",
                    0,
                    &crate::Status::Notice("working status".into()),
                    &crate::render::HeaderInfo {
                        provider_name: "fixture",
                        model_name: "fixture-model",
                        usage: Default::default(),
                        cwd: "/synthetic",
                        cwd_short: "/synthetic",
                        effort_name: "low",
                    },
                    &[],
                    0,
                    &None,
                );
            }),
            "input 日本語",
        ),
    ];
    for (name, mut draw, marker) in cases {
        let mut terminal = test_terminal(120, 30);
        terminal.draw(|f| draw(f)).unwrap();
        let copied = test_copy_all(&mut terminal);
        assert!(copied.contains(marker), "{name}: {copied}");
        assert!(!copied.contains("not-rendered-secret-canary"));
    }
    let mut terminal = test_terminal(120, 30);
    let url = "https://auth.openai.com/oauth/authorize?state=synthetic&code_challenge=dummy";
    crate::auth_browser::draw_link(&mut terminal, url, "Waiting", 0).unwrap();
    assert!(test_copy_all(&mut terminal).contains(url));
    crate::auth_browser::draw_link(&mut terminal, "", "Finishing sign-in...", 0).unwrap();
    assert!(!test_copy_all(&mut terminal).contains("code_challenge"));
}
struct TestAccess {
    screen: Rc<RefCell<Screen<TestBackend>>>,
    copied: Rc<RefCell<Vec<String>>>,
}
impl ScreenAccess for TestAccess {
    fn event(&self, event: &Event) -> io::Result<bool> {
        self.screen.borrow_mut().handle(event, |text| {
            self.copied.borrow_mut().push(text.into());
            Ok(())
        })
    }
    fn clear(&self) {
        let size = self.screen.borrow().inner.size().unwrap();
        self.screen.borrow_mut().reset(size);
    }
}
#[tokio::test]
async fn idle_and_working_stream_reducers_copy_immediately_without_a_management_owner() {
    use futures_util::StreamExt;
    let mut terminal = test_terminal(24, 4);
    terminal
        .draw(|f| f.render_widget(Paragraph::new("visible 日本語"), f.area()))
        .unwrap();
    let screen = terminal.backend().screen.clone();
    let copied = Rc::new(RefCell::new(Vec::new()));
    let access: Rc<dyn ScreenAccess> = Rc::new(TestAccess {
        screen: screen.clone(),
        copied: copied.clone(),
    });
    CURRENT.with(|slot| *slot.borrow_mut() = Some((screen.borrow().id, Rc::downgrade(&access))));
    let events = vec![
        mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
        mouse(MouseEventKind::Drag(MouseButton::Left), 7, 0),
        mouse(MouseEventKind::Up(MouseButton::Left), 7, 0),
    ];
    for event in events.clone() {
        assert!(matches!(filter_event(event).unwrap(), Event::FocusGained));
    }
    assert_eq!(copied.borrow().as_slice(), ["visible"]);
    let mut stream = futures_util::stream::iter(events.into_iter().map(Ok::<_, io::Error>));
    while let Some(event) = stream.next().await {
        assert!(matches!(
            event.and_then(filter_event).unwrap(),
            Event::FocusGained
        ));
    }
    assert_eq!(copied.borrow().as_slice(), ["visible", "visible"]);
    assert!(matches!(
        filter_event(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )))
        .unwrap(),
        Event::Key(_)
    ));
    let old = screen.borrow().id;
    CURRENT.with(|slot| *slot.borrow_mut() = None);
    drop(access);
    drop(terminal);
    assert!(
        CURRENT.with(|slot| slot.borrow().is_none()),
        "old terminal {old} retained"
    );
}
#[test]
fn shared_filter_routes_only_modified_complete_clicks_to_auth_and_finder() {
    let mut terminal = test_terminal(120, 20);
    let copied = Rc::new(RefCell::new(Vec::new()));
    let screen = terminal.backend().screen.clone();
    let access: Rc<dyn ScreenAccess> = Rc::new(TestAccess {
        screen: screen.clone(),
        copied: copied.clone(),
    });
    CURRENT.with(|slot| *slot.borrow_mut() = Some((screen.borrow().id, Rc::downgrade(&access))));
    let url = "https://example.invalid/authorize?state=synthetic";
    let rect = crate::auth_browser::draw_link(&mut terminal, url, "Waiting", 0).unwrap();
    for modifiers in [
        KeyModifiers::NONE,
        KeyModifiers::CONTROL,
        KeyModifiers::SUPER,
    ] {
        for drag in [false, true] {
            let mut click = crate::screen_selection::LinkClick::default();
            let mut count = 0;
            let mut event = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x,
                row: rect.y,
                modifiers,
            };
            count += usize::from(crate::auth_browser::test_link_event(
                filter_event(Event::Mouse(event)).unwrap(),
                rect,
                url,
                &mut click,
            ));
            assert_eq!(count, 0, "press must not launch");
            if drag {
                event.kind = MouseEventKind::Drag(MouseButton::Left);
                count += usize::from(crate::auth_browser::test_link_event(
                    filter_event(Event::Mouse(event)).unwrap(),
                    rect,
                    url,
                    &mut click,
                ));
            }
            event.kind = MouseEventKind::Up(MouseButton::Left);
            count += usize::from(crate::auth_browser::test_link_event(
                filter_event(Event::Mouse(event)).unwrap(),
                rect,
                url,
                &mut click,
            ));
            assert_eq!(
                count,
                usize::from(crate::screen_selection::link_modifier(modifiers) && !drag)
            );
            assert!(!crate::auth_browser::test_link_event(
                filter_event(Event::Mouse(event)).unwrap(),
                rect,
                url,
                &mut click
            ));
        }
    }
    let mut blank_gesture = crate::screen_selection::LinkClick::default();
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let event = MouseEvent {
            kind,
            column: rect.right() - 1,
            row: rect.bottom() - 1,
            modifiers: KeyModifiers::CONTROL,
        };
        assert!(!crate::auth_browser::test_link_event(
            filter_event(Event::Mouse(event)).unwrap(),
            rect,
            url,
            &mut blank_gesture
        ));
    }
    #[cfg(target_os = "macos")]
    {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("target.txt"), "synthetic").unwrap();
        let history = vec![crate::render::HistoryLine {
            line: ratatui::text::Line::from("target.txt"),
            shaded: false,
        }];
        terminal
            .draw(|f| f.render_widget(Paragraph::new("target.txt"), f.area()))
            .unwrap();
        let area = Rect::new(0, 0, 120, 20);
        for modifiers in [
            KeyModifiers::NONE,
            KeyModifiers::CONTROL,
            KeyModifiers::SUPER,
        ] {
            for drag in [false, true] {
                let mut click = crate::file_reveal::FileClick::default();
                let mut count = 0;
                let mut event = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 1,
                    row: 0,
                    modifiers,
                };
                let down = filter_event(Event::Mouse(event)).unwrap();
                if let Event::Mouse(mouse) = down {
                    assert!(click.mouse(mouse, &history, root.path(), area, 0).is_none());
                }
                if drag {
                    event.kind = MouseEventKind::Drag(MouseButton::Left);
                    if let Event::Mouse(mouse) = filter_event(Event::Mouse(event)).unwrap() {
                        assert!(click.mouse(mouse, &history, root.path(), area, 0).is_none());
                    }
                }
                event.kind = MouseEventKind::Up(MouseButton::Left);
                if let Event::Mouse(mouse) = filter_event(Event::Mouse(event)).unwrap() {
                    count +=
                        usize::from(click.mouse(mouse, &history, root.path(), area, 0).is_some());
                }
                assert_eq!(
                    count,
                    usize::from(crate::screen_selection::link_modifier(modifiers) && !drag)
                );
            }
        }
    }
    assert!(
        copied.borrow().is_empty(),
        "link gestures must not cause a clipboard write"
    );
    CURRENT.with(|slot| *slot.borrow_mut() = None);
}

#[test]
fn cursor_protocol_uses_save_and_restore_without_a_cursor_query() {
    let mut bytes = Vec::new();
    {
        let mut backend = CrosstermBackend::new(&mut bytes);
        backend.save_position().unwrap();
        backend.restore_position(()).unwrap();
    }
    assert_eq!(bytes.as_slice(), b"\x1b7\x1b8");
}
#[test]
fn production_runtime_factories_and_sync_async_readers_use_the_common_path() {
    for (name, source) in [
        ("main", include_str!("../lib.rs")),
        ("model", include_str!("../model_menu.rs")),
        ("status", include_str!("../status_view.rs")),
        ("onboarding", include_str!("../onboarding.rs")),
        ("approver", include_str!("../approver.rs")),
        ("account", include_str!("../account.rs")),
        ("env", include_str!("../environment_commands.rs")),
        ("auth", include_str!("../auth_browser.rs")),
        ("windows", include_str!("../windows_read_only.rs")),
    ] {
        assert!(
            !source.contains("ratatui::crossterm::event::read("),
            "raw reader: {name}"
        );
        assert!(
            !source.contains("event::read()"),
            "raw alias reader: {name}"
        );
        assert!(!source.contains("ratatui::init("), "unwrapped init: {name}");
        assert!(
            !source.contains("ratatui::init_with_options("),
            "unwrapped init options: {name}"
        );
    }
    assert!(include_str!("../lib.rs").contains("event.and_then(crate::terminal::filter_event)"));
}
