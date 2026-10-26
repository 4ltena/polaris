
use super::*;
use ratatui::widgets::Paragraph;
#[test]
#[ignore = "synthetic OS-PTY fixture entered only by the scoped screen-copy runner"]
fn native_screen_copy_driver() {
    let root = std::path::PathBuf::from(
        std::env::var_os("POLARIS_TEST_COPY_DRIVER_PATH")
            .expect("scoped PTY fixture path required"),
    );
    assert!(
        root.canonicalize().unwrap().starts_with(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/verification-fixtures")
                .canonicalize()
                .unwrap()
        )
    );
    assert!(
        !std::fs::symlink_metadata(&root)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    struct Capture {
        screen: Rc<RefCell<Screen<CrosstermBackend<Stdout>>>>,
        samples: RefCell<Vec<String>>,
        root: std::path::PathBuf,
    }
    impl ScreenAccess for Capture {
        fn event(&self, event: &Event) -> io::Result<bool> {
            self.screen.borrow_mut().handle(event, |text| {
                self.samples.borrow_mut().push(text.into());
                std::fs::write(
                    self.root.join("clipboard.json"),
                    serde_json::to_vec(&*self.samples.borrow()).unwrap(),
                )
                .map_err(|_| "fixture capture failed".into())
            })
        }
        fn clear(&self) {
            let mut screen = self.screen.borrow_mut();
            screen.base = Buffer::empty(Rect::default());
            screen.painted = screen.base.clone();
            screen.selection.clear();
        }
    }
    let mut terminal = init();
    let screen = terminal.backend().screen.clone();
    let capture: Rc<dyn ScreenAccess> = Rc::new(Capture {
        screen: screen.clone(),
        samples: RefCell::new(Vec::new()),
        root: root.clone(),
    });
    CURRENT.with(|slot| *slot.borrow_mut() = Some((screen.borrow().id, Rc::downgrade(&capture))));
    let working = std::env::var_os("POLARIS_TEST_COPY_DRIVER_WORKING").is_some();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
            use futures_util::StreamExt;
            let mut stream=working.then(event::EventStream::new);
            let url="https://example.invalid/?synthetic=1";
            let rect=Rect::new(0,2,80,1);
            let mut link=crate::screen_selection::LinkClick::default();
            let mut opened=0usize;let mut barriers=0usize;let mut page=false;
            terminal.draw(|f|f.render_widget(Paragraph::new(format!("COPY_READY\nA日e\u{301}B\n{url}\nlast line")),f.area())).unwrap();
            std::fs::write(root.join("metrics.json"),serde_json::to_vec(&serde_json::json!({"ready":true,"opened":opened,"barriers":barriers})).unwrap()).unwrap();
            loop {
                let event=if let Some(stream)=&mut stream {stream.next().await.unwrap().and_then(filter_event).unwrap()} else {read_event().unwrap()};
                if crate::auth_browser::test_link_event(event.clone(),rect,url,&mut link) {opened+=1;}
                if let Event::Key(key)=event {
                    if key.code==event::KeyCode::Char('q') {break;}
                    if key.code==event::KeyCode::Char('b') {barriers+=1;}
                    if key.code==event::KeyCode::Char('p') {
                        page=true;
                        terminal.draw(|f|f.render_widget(Paragraph::new("NEW_PAGE\nsecond\n"),f.area())).unwrap();
                    }
                }
                std::fs::write(root.join("metrics.json"),serde_json::to_vec(&serde_json::json!({"ready":true,"opened":opened,"barriers":barriers,"page":page})).unwrap()).unwrap();
            }
        });
    restore();
}
