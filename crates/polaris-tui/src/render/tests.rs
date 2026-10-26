
use super::*;
use polaris_core::session::Session;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// Renders `messages` the same way `print_new_history` does in
/// production (`history_lines_for` then `render_history_into`), into a
/// buffer tall enough to hold every line with no clipping — the
/// one-shot print model has no "does it fit" concept the way the old
/// bounded/scrolled history pane did, so tests don't need to reason
/// about a viewport height either.
fn render_history_to_string(messages: &[polaris_provider::Message], width: u16) -> String {
    let lines = history_lines_for(messages);
    let height = (lines.len() as u16).max(1);
    let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, width, height));
    let area = buf.area;
    render_history_into(&mut buf, area, &lines);
    buf.content.iter().map(|c| c.symbol()).collect::<String>()
}

/// Renders the always-redrawn footer (suggestions/status/input/footer)
/// via a real `Frame`, the same way `lib.rs`'s per-frame `draw()` call
/// does.
fn render_footer_to_string(
    input: &str,
    status: &Status,
    header: &HeaderInfo,
    suggestions: &[&crate::slash::SlashCommand],
    selected_suggestion: usize,
    width: u16,
    height: u16,
) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| {
            render_footer(
                f,
                f.area(),
                input,
                input.len(),
                status,
                header,
                suggestions,
                selected_suggestion,
            )
        })
        .expect("draw");
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>()
}

/// Renders the footer with an explicit cursor byte-offset and returns
/// where the real terminal cursor landed.
fn render_footer_cursor_position(
    input: &str,
    cursor: usize,
    width: u16,
    height: u16,
) -> ratatui::layout::Position {
    let header = test_header();
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| render_footer(f, f.area(), input, cursor, &Status::Idle, &header, &[], 0))
        .expect("draw");
    terminal.get_cursor_position().expect("cursor position")
}

/// Regression test for the IME-preedit-shows-at-the-bottom bug: the
/// terminal cursor must land exactly after the typed text in the
/// input row, not stay wherever a prior raw write left it (which
/// `Frame::set_cursor_position` never being called defaulted to
/// "hidden, unpositioned" — see `terminal.rs`'s `try_draw`).
#[test]
fn the_cursor_lands_right_after_the_typed_input_text() {
    let backend = TestBackend::new(80, 10);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| {
            render_footer(
                f,
                f.area(),
                "hello",
                5,
                &Status::Idle,
                &test_header(),
                &[],
                0,
            );
        })
        .expect("draw");
    // "› " (width 2) + "hello" (width 5) = column 7. The input row
    // would be row 2 (status(1) + suggestions(0) + pad-above(1)) if
    // this 10-row area exactly matched the 5 rows this frame's content
    // needs — but it doesn't, so `Flex::End` shifts everything down by
    // the 5-row leftover, landing the input row at row 7.
    terminal.backend_mut().assert_cursor_position((7, 7));
}

/// Wide (CJK) characters must count as 2 columns each, or the cursor
/// — and thus the IME popup a terminal anchors to it — would land
/// short of the actual caret whenever any wide character was already
/// typed, exactly the scenario a real Japanese IME composition hits.
#[test]
fn the_cursor_accounts_for_wide_characters_already_typed() {
    let backend = TestBackend::new(80, 10);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let input = "helloこんにちは";
    terminal
        .draw(|f| {
            render_footer(
                f,
                f.area(),
                input,
                input.len(),
                &Status::Idle,
                &test_header(),
                &[],
                0,
            );
        })
        .expect("draw");
    // "› "(2) + "hello"(5) + "こんにちは"(5 chars * width 2 = 10) = 17.
    // Row 7, not 2 — see `the_cursor_lands_right_after_the_typed_input_text`'s
    // comment on this same 10-row-area/5-row-content leftover shift.
    terminal.backend_mut().assert_cursor_position((17, 7));
}

fn test_header() -> HeaderInfo<'static> {
    HeaderInfo {
        cwd: "/tmp/example",
        cwd_short: "~/example",
        effort_name: "low",
        provider_name: "openai",
        model_name: "gpt-5.4",
        usage: polaris_provider::Usage::default(),
    }
}

#[test]
fn header_history_lines_matches_render_header_into_content_and_width() {
    let header = test_header();
    let width = 60u16;

    let lines = header_history_lines(&header, width);
    assert_eq!(lines.len(), HEADER_HEIGHT as usize);

    // Top and bottom rows are a full-width box-drawing border.
    let top = lines[0].line.to_string();
    let bottom = lines[lines.len() - 1].line.to_string();
    assert!(top.starts_with('\u{250c}') && top.ends_with('\u{2510}'));
    assert!(bottom.starts_with('\u{2514}') && bottom.ends_with('\u{2518}'));
    assert_eq!(top.chars().count(), width as usize);
    assert_eq!(bottom.chars().count(), width as usize);

    // Every content row is framed with the same `│ ... │` as the border
    // rows imply, and each content row's *text* matches header_lines'
    // plain (unbordered) content exactly.
    let plain = header_lines(&header);
    for (i, plain_line) in plain.iter().enumerate() {
        let framed = lines[1 + i].line.to_string();
        assert!(framed.starts_with('\u{2502}') && framed.ends_with('\u{2502}'));
        assert!(framed.contains(&plain_line.to_string()));
        assert_eq!(framed.chars().count(), width as usize);
    }

    // No row is shaded — the header box isn't a user/assistant line.
    assert!(lines.iter().all(|hl| !hl.shaded));
}

#[test]
fn startup_title_places_the_gray_package_version_after_polaris() {
    let lines = header_history_lines(&test_header(), 60);
    let title = &lines[1].line;
    let expected = format!(" v{}", env!("CARGO_PKG_VERSION"));
    assert!(title.to_string().contains(&format!("polaris{expected}")));
    let version = title
        .spans
        .iter()
        .find(|span| span.content.as_ref() == expected.as_str())
        .expect("the framed startup title must retain the version span");
    assert_eq!(version.style.fg, Some(Color::DarkGray));
    assert!(!version.style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn composer_displays_file_name_and_places_the_caret_after_the_whole_block() {
    let input = "確認 [file](file:///tmp/%E8%B3%87%E6%96%99.txt)";
    let backend = TestBackend::new(80, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            render_footer(
                frame,
                frame.area(),
                input,
                input.len(),
                &Status::Idle,
                &test_header(),
                &[],
                0,
            )
        })
        .unwrap();
    let content = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.replace(' ', "").contains("[資料.txt]"));
    assert!(!content.contains("file:///"));
    let position =
        composer_cursor_position(ratatui::layout::Rect::new(0, 0, 80, 1), input, input.len());
    assert_eq!(position.x, 2 + "確認 [資料.txt]".width() as u16);
}

#[test]
fn a_user_message_appears_unlabeled_and_shaded() {
    // No "you:" label — the full-row gray shading (`shaded: true`) is
    // the only marker, matching codex's own unlabeled user line.
    let mut session = Session::default();
    session.push_user("Cargo.toml は何行か");

    let content = render_history_to_string(&session.messages, 60);
    assert!(!content.contains("you:"));
    assert!(content.contains("Cargo.toml"));
    let lines = history_lines_for(&session.messages);
    assert!(lines[0].shaded);
}

#[test]
fn the_suggestion_window_scrolls_to_keep_the_selection_visible() {
    // 16 real commands, more than MAX_DISPLAYED_SUGGESTIONS (8) — the
    // window must scroll to follow selection past the first page,
    // not stay pinned to the first 8 forever.
    let all: Vec<&crate::slash::SlashCommand> = crate::slash::COMMANDS.iter().collect();
    let last = all.len() - 1;
    let header = test_header();
    let content = render_footer_to_string("/", &Status::Idle, &header, &all, last, 80, 20);
    assert!(
        content.contains(all[last].name),
        "the selected (last) command's name should be visible once scrolled to: {content}"
    );
}

#[test]
fn the_suggestion_window_still_shows_the_first_page_when_selection_is_near_the_top() {
    let all: Vec<&crate::slash::SlashCommand> = crate::slash::COMMANDS.iter().collect();
    let header = test_header();
    let content = render_footer_to_string("/", &Status::Idle, &header, &all, 0, 80, 20);
    assert!(content.contains(all[0].name));
}

#[test]
fn the_input_rows_gray_shading_spans_the_whole_row_not_just_the_typed_text() {
    // Matches codex's own input-box styling — confirmed against a real
    // screenshot — where the whole composer row is shaded, not just
    // the cells the prompt/typed text happen to occupy.
    let backend = TestBackend::new(60, 7);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| render_footer(f, f.area(), "hi", 2, &Status::Idle, &test_header(), &[], 0))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    // Row 2 would be the input row (status(1) + suggestions(0) +
    // pad-above(1)) if this 7-row area matched the 5 rows this
    // frame's content needs exactly — it doesn't (2 rows leftover),
    // and `Flex::End` shifts everything down by that leftover, so the
    // input row is actually row 4.
    // "› hi" ends well before column 50 on a 60-wide backend, but the
    // shading should still reach all the way to the row's right edge.
    assert_eq!(
        buffer[(2, 4)].bg,
        Color::DarkGray,
        "the prompt/typed-text cells should be shaded"
    );
    assert_eq!(
        buffer[(50, 4)].bg,
        Color::DarkGray,
        "empty space past the typed text should also be shaded, all the way to the row's edge"
    );
    assert_eq!(
        buffer[(59, 4)].bg,
        Color::DarkGray,
        "the row's last column should be shaded too"
    );
}

#[test]
fn typing_does_not_shorten_the_composer_background_on_redraw() {
    let backend = TestBackend::new(60, 7);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| render_footer(f, f.area(), "", 0, &Status::Idle, &test_header(), &[], 0))
        .expect("empty draw");
    terminal
        .draw(|f| {
            render_footer(
                f,
                f.area(),
                "typing",
                6,
                &Status::Thinking {
                    elapsed: Duration::from_secs(138),
                },
                &test_header(),
                &[],
                0,
            )
        })
        .expect("typed draw");
    let buffer = terminal.backend().buffer();
    for y in 3..=5 {
        for x in 0..60 {
            assert_eq!(
                buffer[(x, y)].bg,
                Color::DarkGray,
                "composer background changed at ({x}, {y})"
            );
        }
    }
}

#[test]
fn the_input_row_has_a_shaded_blank_row_above_and_below_it() {
    // Widens the visual weight of the input box — a single text row of
    // shading read as too thin. The blank rows carry no text, just
    // the same DarkGray fill across the whole width.
    let backend = TestBackend::new(60, 7);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| render_footer(f, f.area(), "hi", 2, &Status::Idle, &test_header(), &[], 0))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    // With this 7-row area 2 rows taller than the 5 rows this frame's
    // content needs, `Flex::End` shifts the whole block down by that
    // leftover: status(row 2) + suggestions(0) + pad-above(row 3) +
    // input(row 4) + pad-below(row 5) + footer(row 6).
    for x in [0u16, 30, 59] {
        assert_eq!(
            buffer[(x, 3)].bg,
            Color::DarkGray,
            "the row above the input line should be shaded at column {x}"
        );
        assert_eq!(
            buffer[(x, 5)].bg,
            Color::DarkGray,
            "the row below the input line should be shaded at column {x}"
        );
    }
    // Both pad rows carry no visible text.
    let row_text = |y: u16| -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    };
    assert_eq!(row_text(3).trim(), "");
    assert_eq!(row_text(5).trim(), "");
}

#[test]
fn the_status_line_shows_thinking_while_a_turn_is_in_flight() {
    let header = test_header();
    let status = Status::Thinking {
        elapsed: Duration::from_secs(3),
    };
    let content = render_footer_to_string("", &status, &header, &[], 0, 60, 6);
    assert!(content.contains("Working"));
    assert!(content.contains("3s"));
    assert!(content.contains("esc to interrupt"));
}

/// Regression test for the input box sitting well above the terminal's
/// real bottom edge whenever the suggestions popup isn't showing (the
/// common case): given an area taller than what this frame's content
/// actually needs (5 rows, no suggestions — an artificially oversized
/// area, standing in for whatever `draw_frame` might hand it), the
/// content must still anchor to the bottom rather than the top. This
/// tests `render_footer`'s own `Flex::End` behavior in isolation —
/// unrelated to how `draw_frame` sizes the area it passes in, which is
/// `footer_height`'s job (see the tests below).
#[test]
fn the_input_row_reaches_the_true_bottom_of_the_footer_reservation() {
    let oversized_area_height = 20;
    let pos = render_footer_cursor_position("", 0, 80, oversized_area_height);
    // The five visible rows this content occupies — status,
    // pad-above, input, pad-below, footer (suggestions is empty) —
    // sit flush against the bottom of the reservation. Input is the
    // third of those five, so two rows up from the very last one, not
    // stranded near the top.
    assert_eq!(pos.y, oversized_area_height - 3);
}

// `footer_height` must mirror `render_footer`'s own internal
// `suggestions_height` computation exactly (see its doc comment) —
// these pin down the exact expected totals for four representative
// suggestion counts, derived from `render_footer`'s actual formula
// (`shown + 1 + u16::from(truncated)`), not hand-guessed:
//   n=0:  suggestions_height=0                         -> 5 + 0  = 5
//   n=1:  shown=1, truncated=false -> 1+1+0=2           -> 5 + 2  = 7
//   n=8:  shown=8, truncated=false -> 8+1+0=9           -> 5 + 9  = 14
//   n=11: shown=8, truncated=true  -> 8+1+1=10          -> 5 + 10 = 15
// The `n=8` case landing on exactly 14 confirms this matches the old
// fixed `FOOTER_HEIGHT` constant's own worst-case sizing at exactly
// `MAX_DISPLAYED_SUGGESTIONS` suggestions.
#[test]
fn footer_height_with_no_suggestions_is_just_the_five_fixed_rows() {
    assert_eq!(footer_height(0), 5);
}

#[test]
fn footer_height_with_one_suggestion_adds_the_row_plus_its_border() {
    assert_eq!(footer_height(1), 7);
}

#[test]
fn footer_height_at_the_display_cap_reserves_every_row_with_no_more_row() {
    assert_eq!(footer_height(MAX_DISPLAYED_SUGGESTIONS), 14);
}

#[test]
fn footer_height_past_the_cap_adds_one_more_row_for_the_truncation_notice() {
    assert_eq!(footer_height(MAX_DISPLAYED_SUGGESTIONS + 3), 15);
}

#[test]
fn the_cursor_lands_right_after_the_prompt_prefix_when_the_buffer_is_empty() {
    let pos = render_footer_cursor_position("", 0, 60, 6);
    // A 6-row area holds 1 more row than the 5 rows this frame's
    // content actually needs (status+pad-above+input+pad-below+footer,
    // suggestions being empty) — `Flex::End` anchors that content to
    // the bottom of the area, so the leftover row lands above
    // `status_area`, shifting the input row from index 2 to index 3.
    assert_eq!(pos.y, 3);
    assert_eq!(pos.x, Line::from("\u{203a} ").width() as u16);
}

#[test]
fn the_cursor_tracks_a_mid_buffer_offset_not_always_the_end() {
    let full = render_footer_cursor_position("hello", 5, 60, 6);
    let mid = render_footer_cursor_position("hello", 2, 60, 6);
    assert_eq!(full.x - mid.x, 3);
}

#[test]
fn the_cursor_accounts_for_wide_characters_before_it() {
    // Hiragana "あ" occupies 2 terminal columns, unlike the following
    // "b" — the cursor after both must be 2 (prefix) + 2 (wide) + 1
    // columns ahead of the prefix-only position, not 2 (prefix) + 2
    // (one char each).
    let input = "\u{3042}b";
    let after_wide_char = "\u{3042}".len();
    let pos = render_footer_cursor_position(input, after_wide_char, 60, 6);
    let prefix = render_footer_cursor_position(input, 0, 60, 6);
    assert_eq!(pos.x - prefix.x, 2);
}

#[test]
fn shimmer_spans_reproduce_the_text_verbatim() {
    // The animation only ever changes color/weight — concatenating
    // the spans back together must always yield the original text,
    // never dropped or reordered characters.
    let spans = shimmer_spans("Working", Duration::from_millis(750));
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(text, "Working");
}

#[test]
fn shimmer_spans_on_empty_text_is_empty() {
    assert!(shimmer_spans("", Duration::ZERO).is_empty());
}

fn plain_history_line(text: &str) -> HistoryLine {
    HistoryLine::plain(Line::from(text.to_string()))
}

#[test]
fn a_short_line_passes_through_unwrapped() {
    let history = vec![plain_history_line("short")];
    let wrapped = wrap_history_lines(&history, 20);
    assert_eq!(wrapped.len(), 1);
    assert_eq!(wrapped[0].line.to_string(), "short");
}

#[test]
fn history_wrap_cache_reuses_reflow_while_only_the_view_moves() {
    let mut cache = HistoryWrapCache::default();
    let mut history = vec![plain_history_line(
        "one two three four five six seven eight nine ten",
    )];

    for _ in 0..100 {
        let _ = cache.lines(&history, 12);
    }
    assert_eq!(cache.rebuilds(), 1);

    history[0] = plain_history_line("changed activity row");
    cache.invalidate();
    assert_eq!(cache.lines(&history, 12)[0].line.to_string(), "changed");
    assert_eq!(cache.rebuilds(), 2);
}

#[test]
fn temporary_history_suffix_reuses_the_cached_prefix_even_after_draw_failure() {
    let mut cache = HistoryWrapCache::default();
    let mut history = vec![plain_history_line("earlier wrapped conversation"); 2_000];
    let rows = cache.lines(&history, 12).len();
    let extra = plain_history_line("new input with several wrapped rows");
    let extra_rows = wrap_history_lines(std::slice::from_ref(&extra), 12).len();
    let result = cache.with_appended(&mut history, vec![extra], |preview, cache| {
        assert_eq!(cache.lines(preview, 12).len(), rows + extra_rows);
        assert_eq!(cache.rebuilds(), 1, "preview must not reflow the prefix");
        Err::<(), _>("draw failed")
    });
    assert_eq!(result, Err("draw failed"));
    assert_eq!(history.len(), 2_000);
    assert_eq!(cache.lines(&history, 12).len(), rows);
    assert_eq!(cache.rebuilds(), 1, "cleanup must retain the cached prefix");
}

#[test]
fn temporary_history_suffix_keeps_the_resized_prefix_cache() {
    let mut cache = HistoryWrapCache::default();
    let mut history = vec![plain_history_line(
        "earlier conversation with wide text 日本語",
    )];
    let _ = cache.lines(&history, 60);
    cache.with_appended(
        &mut history,
        vec![plain_history_line("preview 日本語 with wrapping")],
        |preview, cache| {
            assert!(cache.lines(preview, 12).len() > 2);
            assert_eq!(cache.rebuilds(), 2);
        },
    );
    assert_eq!(history.len(), 1);
    let actual = cache
        .lines(&history, 12)
        .iter()
        .map(|row| row.line.to_string())
        .collect::<Vec<_>>();
    let expected = wrap_history_lines(&history, 12)
        .iter()
        .map(|row| row.line.to_string())
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    assert_eq!(
        cache.rebuilds(),
        2,
        "removing preview must not reflow again"
    );
}

#[test]
fn a_long_line_wraps_at_a_word_boundary() {
    let history = vec![plain_history_line("one two three four")];
    // Width 9 fits "one two" (7) but not "one two three" (13).
    let wrapped = wrap_history_lines(&history, 9);
    let texts: Vec<String> = wrapped.iter().map(|l| l.line.to_string()).collect();
    assert_eq!(texts, vec!["one two", "three", "four"]);
    assert!(
        texts.iter().all(|t| t.chars().count() <= 9),
        "no row should exceed the given width: {texts:?}"
    );
}

#[test]
fn a_single_word_longer_than_the_width_is_hard_broken() {
    let history = vec![plain_history_line("supercalifragilistic")];
    let wrapped = wrap_history_lines(&history, 5);
    let texts: Vec<String> = wrapped.iter().map(|l| l.line.to_string()).collect();
    assert_eq!(texts.join(""), "supercalifragilistic");
    assert!(texts.iter().all(|t| t.chars().count() <= 5));
    assert!(texts.len() > 1);
}

#[test]
fn wrapping_preserves_the_shaded_flag_on_every_row() {
    let history = vec![HistoryLine {
        line: Line::from("one two three four five".to_string()),
        shaded: true,
    }];
    let wrapped = wrap_history_lines(&history, 8);
    assert!(wrapped.len() > 1, "the line should have actually wrapped");
    assert!(wrapped.iter().all(|l| l.shaded));
}

#[test]
fn wrapping_preserves_per_span_style_across_the_break() {
    let line = Line::from(vec![
        Span::styled("bold word ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("plain word"),
    ]);
    let history = vec![HistoryLine::plain(line)];
    let wrapped = wrap_history_lines(&history, 10);
    assert_eq!(wrapped.len(), 2);
    assert!(
        wrapped[0]
            .line
            .spans
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::BOLD)),
        "the first row (\"bold word\") should stay bold"
    );
    assert!(
        wrapped[1]
            .line
            .spans
            .iter()
            .all(|s| !s.style.add_modifier.contains(Modifier::BOLD)),
        "the second row (\"plain word\") should not be bold"
    );
}

#[test]
fn a_zero_width_is_a_no_op() {
    let history = vec![plain_history_line("anything at all")];
    let wrapped = wrap_history_lines(&history, 0);
    assert_eq!(wrapped.len(), 1);
    assert_eq!(wrapped[0].line.to_string(), "anything at all");
}

#[test]
fn an_already_narrow_history_of_many_lines_is_unaffected() {
    let history = vec![plain_history_line("a"), plain_history_line("b")];
    let wrapped = wrap_history_lines(&history, 40);
    assert_eq!(wrapped.len(), 2);
}

#[test]
fn the_window_shows_everything_when_history_is_shorter_than_the_viewport() {
    assert_eq!(visible_history_window(3, 0, 10), 0..3);
}

#[test]
fn the_window_shows_the_last_n_lines_when_scroll_offset_is_zero() {
    assert_eq!(visible_history_window(100, 0, 10), 90..100);
}

#[test]
fn a_positive_scroll_offset_shifts_the_window_up() {
    assert_eq!(visible_history_window(100, 5, 10), 85..95);
}

#[test]
fn scroll_offset_is_clamped_at_the_oldest_line() {
    // Can't scroll further back than showing line 0 at the window's top.
    assert_eq!(visible_history_window(100, 1000, 10), 0..10);
}

#[test]
fn a_zero_height_window_is_always_empty() {
    assert_eq!(visible_history_window(50, 0, 0), 50..50);
}

#[test]
fn history_longer_than_the_old_screen_height_is_never_clipped() {
    // The one-shot print model (`insert_before`, see `lib.rs`) has no
    // bounded viewport to scroll within — every message is printed
    // once, unbounded, into the terminal's own real scrollback. This
    // replaces the old test of the same name's intent
    // ("does the newest message stay visible"), which assumed a fixed
    // history pane height that no longer exists: the *point* of this
    // whole feature is that history isn't clipped by screen height at
    // all anymore, so the correct assertion is now the opposite of
    // the old one — the earliest message is still present, not
    // scrolled away.
    let mut session = Session::default();
    for i in 0..30 {
        session.push_user(&format!("message number {i}"));
    }

    let content = render_history_to_string(&session.messages, 60);
    assert!(content.contains("message number 29"));
    assert!(content.contains("message number 0 "));
}

#[test]
fn the_resume_picker_shows_no_saved_conversations_when_there_are_none() {
    let backend = TestBackend::new(60, 14);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| render_resume_picker(f, &[], 0, 0))
        .expect("draw");

    let content = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(content.contains("no saved conversations"));
}

#[test]
fn the_resume_picker_lists_directories_and_previews() {
    let now = 1_705_311_000_000u128;
    let groups = vec![crate::sessions::DirGroup {
        cwd: "/tmp/example".to_string(),
        sessions: vec![crate::sessions::SessionSummary {
            id: "s1".to_string(),
            path: std::path::PathBuf::new(),
            cwd: "/tmp/example".to_string(),
            started_at_millis: now - 42_000,
            message_count: 3,
            preview: "what does this repo do?".to_string(),
            kind: crate::sessions::SessionKind::Legacy,
        }],
    }];

    let backend = TestBackend::new(80, 14);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| render_resume_picker(f, &groups, 0, now))
        .expect("draw");

    let content = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(content.contains("/tmp/example"));
    // Relative, not absolute — matches codex's own resume-picker
    // convention ("42s ago"), verified against its real source.
    assert!(content.contains("42s ago"));
    assert!(content.contains("3 message"));
    assert!(content.contains("what does this repo do?"));
    assert!(content.contains("enter to resume"));
    assert!(content.contains("esc to cancel"));
}

#[test]
fn the_resume_picker_keeps_the_selected_session_visible_on_a_short_terminal() {
    let now = 1_705_311_000_000u128;
    let cwd = "/tmp/eight\ninjected\t\u{1b}[31m";
    let groups = vec![crate::sessions::DirGroup {
        cwd: cwd.to_string(),
        sessions: (0..8)
            .map(|i| crate::sessions::SessionSummary {
                id: format!("s{i}"),
                path: std::path::PathBuf::new(),
                cwd: cwd.to_string(),
                started_at_millis: now - 42_000,
                message_count: 1,
                preview: if i == 7 {
                    format!("conversation {i}\nforged\t\u{1b}[31m")
                } else {
                    format!("conversation {i}")
                },
                kind: crate::sessions::SessionKind::Legacy,
            })
            .collect(),
    }];
    let mut terminal = Terminal::new(TestBackend::new(70, 8)).expect("terminal");
    terminal
        .draw(|f| render_resume_picker(f, &groups, 7, now))
        .expect("draw");

    let content = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        content.contains("/tmp/eight\\ninjected\\t�[31m"),
        "selected group must remain visible on one line: {content:?}"
    );
    assert!(content.contains("❯ 42s ago  1 message(s)  conversation 7\\nforged"));
    assert!(content.contains("\\t�[31m"));
    assert!(!content.chars().any(|c| matches!(c, '\n' | '\t' | '\u{1b}')));
    assert!(content.contains("enter to resume"));
}

#[test]
fn the_rewind_picker_names_the_message_and_requires_preview() {
    let now = 1_705_311_000_000u128;
    let rows = vec![RewindPickerRow {
        boundary_id: "checkpoint-1".into(),
        user_message: "first line\nsecond line".into(),
        recorded_at_millis: now - 42_000,
    }];
    let backend = TestBackend::new(90, 12);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| render_rewind_picker(f, &rows, 0, now))
        .expect("draw");

    let content = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(content.contains("42s ago"));
    assert!(content.contains("first line second line"));
    assert!(content.contains("enter to preview"));
    assert!(content.contains("esc to cancel"));
}

#[test]
fn empty_rewind_picker_explains_the_reason_at_narrow_and_wide_widths() {
    let reason = "Interrupted turns cannot be rewound (save boundary incomplete)\ncontrol\x1b[31m";
    for width in [42, 100] {
        let mut terminal = Terminal::new(TestBackend::new(width, 16)).unwrap();
        terminal
            .draw(|frame| render_rewind_picker_with_reason(frame, &[], 0, 0, reason))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let content = (1..buffer.area.height - 1)
            .flat_map(|y| (1..buffer.area.width - 1).map(move |x| buffer[(x, y)].symbol()))
            .collect::<String>();
        let compact = content.split_whitespace().collect::<String>();
        assert!(
            compact.contains("Interruptedturnscannotberewound(saveboundaryincomplete)"),
            "width={width}: {content:?}"
        );
        assert!(compact.contains("control�[31m"), "{content:?}");
        assert!(content.contains("any key to close"));
        assert!(!content.contains("enter to preview"));
        assert!(!content.contains('\x1b'));
    }
}

#[test]
fn the_rewind_preview_warns_about_file_changes_and_requires_a_second_action() {
    let backend = TestBackend::new(110, 18);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| {
            render_rewind_preview(
                f,
                &RewindPreviewView {
                    user_message: "edit the package".into(),
                    changed_paths: vec!["src/main.rs".into()],
                    recovery_location: "/private/recovery".into(),
                },
            )
        })
        .expect("draw");
    let content = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(content.contains("working tree will change"));
    assert!(content.contains("src/main.rs"));
    assert!(content.contains("/private/recovery"));
    assert!(content.contains("press r to restore"));
}

#[test]
fn the_model_picker_marks_the_current_model_and_highlights_the_selection() {
    assert_eq!(MODEL_CATALOG, &["gpt-6-astra", "gpt-6.1-sol", "gpt-6-luna"]);
    let backend = TestBackend::new(70, 14);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| render_model_picker(f, "gpt-6.1-sol", false, 1))
        .expect("draw");

    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    assert!(
        rows.iter()
            .any(|r| r.contains("gpt-6.1-sol (recommended, current)"))
    );
    assert!(rows.iter().any(|r| r.contains("gpt-6-astra")));
    assert!(rows.iter().any(|r| r.contains("gpt-6-luna")));
    assert!(rows.iter().any(|r| r.contains("Local model")));
    assert!(!rows.iter().any(|r| r.contains("terra")));
    assert!(rows.iter().any(|r| r.contains("enter to confirm")));
}

#[test]
fn the_model_picker_marks_a_local_selection_separately() {
    let backend = TestBackend::new(70, 14);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| render_model_picker(f, "qwen3", true, MODEL_CATALOG.len()))
        .expect("draw");
    let content = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("gpt-6.1-sol (recommended)"));
    assert!(content.contains("Local model (current)"));
}

#[test]
fn the_model_picker_keeps_the_selected_option_visible_on_a_short_terminal() {
    let mut terminal = Terminal::new(TestBackend::new(70, 8)).expect("terminal");
    for (selected, expected) in [
        (MODEL_CATALOG.len(), "❯ 4. Local model"),
        (MODEL_CATALOG.len() - 1, "❯ 3. gpt-6-luna"),
    ] {
        terminal
            .draw(|f| render_model_picker(f, "gpt-6-astra", false, selected))
            .expect("draw");
        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            content.contains(expected),
            "selected option must remain visible: {content:?}"
        );
        assert!(content.contains("enter to confirm"));
    }
}

#[test]
fn the_local_model_picker_lists_connected_runtime_models() {
    let backend = TestBackend::new(80, 12);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let models = vec![
        "Ollama · qwen3:4b".to_string(),
        "LM Studio · gemma".to_string(),
    ];
    terminal
        .draw(|f| render_local_model_picker(f, &models, Some("Ollama · qwen3:4b"), 1))
        .expect("draw");
    let content = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(content.contains("Ollama · qwen3:4b (current)"));
    assert!(content.contains("LM Studio · gemma"));
}

#[test]
fn the_local_model_picker_keeps_the_selected_model_visible_on_a_short_terminal() {
    let models = (0..8)
        .map(|i| {
            if i == 7 {
                format!("Ollama · local-{i}\nforged\t\u{1b}[31m")
            } else {
                format!("Ollama · local-{i}")
            }
        })
        .collect::<Vec<_>>();
    let mut terminal = Terminal::new(TestBackend::new(80, 8)).expect("terminal");
    terminal
        .draw(|f| render_local_model_picker(f, &models, None, 7))
        .expect("draw");

    let content = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        content.contains("❯ 8. Ollama · local-7\\nforged\\t�[31m"),
        "selected model must remain visible on one line: {content:?}"
    );
    assert!(!content.chars().any(|c| matches!(c, '\n' | '\t' | '\u{1b}')));
    assert!(content.contains("enter to confirm"));
}

#[test]
fn effort_wire_value_leaves_already_valid_names_unchanged() {
    assert_eq!(effort_wire_value("low"), "low");
    assert_eq!(effort_wire_value("medium"), "medium");
    assert_eq!(effort_wire_value("high"), "high");
}

#[test]
fn effort_wire_value_renames_extra_high_and_sends_max_as_itself() {
    assert_eq!(effort_wire_value("extra high"), "xhigh");
    // `max` is a real effort on `/responses`, which is the endpoint
    // polaris sends to. Folding it into `xhigh` made the top two rows
    // of the picker ask for the same thing.
    assert_eq!(effort_wire_value("max"), "max");
}

#[test]
fn effort_display_name_keeps_the_cli_default_human_readable() {
    assert_eq!(effort_display_name("xhigh"), DEFAULT_EFFORT);
}

#[test]
fn the_skills_picker_lists_every_skill_with_its_description() {
    let skills = vec![
        polaris_skills::Skill {
            name: "reviewer".to_string(),
            description: "reviews code for bugs".to_string(),
            body: String::new(),
            path: std::path::PathBuf::new(),
        },
        polaris_skills::Skill {
            name: "planner".to_string(),
            description: "breaks work into steps".to_string(),
            body: String::new(),
            path: std::path::PathBuf::new(),
        },
    ];
    let backend = TestBackend::new(70, 14);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| render_skills_picker(f, &skills, 0))
        .expect("draw");

    let content = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(content.contains("reviewer"));
    assert!(content.contains("reviews code for bugs"));
    assert!(content.contains("planner"));
    assert!(content.contains("breaks work into steps"));
}

#[test]
fn the_skills_picker_keeps_the_selected_skill_visible_on_a_short_screen() {
    let skills = (0..8)
        .map(|index| polaris_skills::Skill {
            name: format!("skill-{index}"),
            description: format!("description-{index}"),
            body: String::new(),
            path: std::path::PathBuf::new(),
        })
        .collect::<Vec<_>>();
    let backend = TestBackend::new(70, 8);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_skills_picker(frame, &skills, 7))
        .expect("draw");

    let rows = terminal
        .backend()
        .buffer()
        .content
        .chunks(70)
        .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();
    assert!(
        rows.iter().any(|row| row.contains("❯ skill-7")),
        "selected skill is not visible: {rows:?}"
    );
}

#[test]
fn the_skills_picker_shows_a_message_when_there_are_none() {
    let backend = TestBackend::new(70, 14);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| render_skills_picker(f, &[], 0))
        .expect("draw");

    let content = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(content.contains("no skills discovered"));
}

#[test]
fn the_permissions_picker_marks_the_currently_active_policy() {
    let backend = TestBackend::new(70, 14);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let sandbox = polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[])
        .expect("sandbox");
    terminal
        .draw(|f| {
            render_permissions_picker(
                f,
                polaris_core::approval::ApprovalPolicy::Always,
                polaris_core::approval::ApprovalPolicy::Always,
                &sandbox,
                0,
            )
        })
        .expect("draw");

    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    assert!(rows.iter().any(|r| r.contains("always (current)")));
    assert!(!rows.iter().any(|r| r.contains("never (current)")));
    assert!(rows.iter().any(|r| r.contains("startup --approval always")));
    assert!(rows.iter().any(|r| r.contains("--sandbox read-only")));
    assert!(
        rows.iter()
            .any(|r| r.contains("--approval never (restart)"))
    );
}

#[test]
fn the_permissions_picker_highlights_whichever_option_is_selected() {
    let backend = TestBackend::new(70, 14);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let sandbox = polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[])
        .expect("sandbox");
    terminal
        .draw(|f| {
            render_permissions_picker(
                f,
                polaris_core::approval::ApprovalPolicy::Never,
                polaris_core::approval::ApprovalPolicy::Never,
                &sandbox,
                2,
            )
        })
        .expect("draw");

    let buffer = terminal.backend().buffer();
    let find_title_color = |needle: &str| {
        for y in 0..buffer.area.height {
            let cells: Vec<&str> = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            let row = cells.concat();
            if let Some(byte_col) = row.find(needle) {
                // `byte_col` is a byte offset into the concatenated
                // row string; since every cell here is ASCII, that's
                // also the cell/column index.
                return buffer[(byte_col as u16, y)].fg;
            }
        }
        panic!("row containing {needle:?} not found");
    };
    // The highlighted row (index 2, "always") is cyan; an
    // unselected row isn't.
    assert_eq!(find_title_color("always"), Color::Cyan);
    assert_ne!(find_title_color("never"), Color::Cyan);
}

#[test]
fn the_permissions_picker_shows_the_fixed_write_root() {
    let root = tempfile::tempdir().expect("root");
    let sandbox = polaris_sandbox::SandboxPolicy::new(
        polaris_sandbox::SandboxMode::WorkspaceWrite,
        &[root.path().to_path_buf()],
    )
    .expect("sandbox");
    let backend = TestBackend::new(120, 14);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| {
            render_permissions_picker(
                f,
                polaris_core::approval::ApprovalPolicy::OnRequest,
                polaris_core::approval::ApprovalPolicy::OnRequest,
                &sandbox,
                1,
            )
        })
        .expect("draw");

    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    assert!(rows.iter().any(|row| row.contains(&sandbox.describe())));
    assert!(
        rows.iter()
            .any(|row| row.contains("--sandbox workspace-write"))
    );
    assert!(rows.iter().any(|row| row.contains("on request (current)")));
    assert!(rows.iter().any(|row| row.contains("--approval always")));
}

#[test]
fn the_permissions_picker_keeps_startup_limits_visible_in_short_terminals() {
    let root = tempfile::tempdir().expect("root");
    let sandbox = polaris_sandbox::SandboxPolicy::new(
        polaris_sandbox::SandboxMode::WorkspaceWrite,
        &[root.path().to_path_buf()],
    )
    .expect("sandbox");
    for height in [8, 10, 13] {
        let mut terminal = Terminal::new(TestBackend::new(40, height)).expect("terminal");
        terminal
            .draw(|f| {
                render_permissions_picker(
                    f,
                    polaris_core::approval::ApprovalPolicy::Never,
                    polaris_core::approval::ApprovalPolicy::Never,
                    &sandbox,
                    1,
                )
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        assert!(
            rows.iter()
                .any(|row| row.contains("startup --approval never"))
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("startup --sandbox workspace-write"))
        );
        assert!(rows.iter().any(|row| row.contains("never (current)")));
        assert!(
            rows.iter()
                .any(|row| row.contains("--approval on-request (restart)"))
        );
    }
    let mut terminal = Terminal::new(TestBackend::new(35, 7)).expect("terminal");
    terminal
        .draw(|f| {
            render_permissions_picker(
                f,
                polaris_core::approval::ApprovalPolicy::Never,
                polaris_core::approval::ApprovalPolicy::Never,
                &sandbox,
                1,
            )
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let content: String = (0..buffer.area.height)
        .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol()))
        .collect();
    assert!(content.contains("8 rows; esc to cancel"));
}

#[test]
fn the_permissions_picker_escapes_newlines_in_write_roots() {
    let temp = tempfile::tempdir().expect("temp");
    let root = temp.path().join("root\nfake approval: always");
    std::fs::create_dir(&root).expect("root");
    let sandbox =
        polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::WorkspaceWrite, &[root])
            .expect("sandbox");
    let mut terminal = Terminal::new(TestBackend::new(160, 14)).expect("terminal");
    terminal
        .draw(|f| {
            render_permissions_picker(
                f,
                polaris_core::approval::ApprovalPolicy::OnRequest,
                polaris_core::approval::ApprovalPolicy::OnRequest,
                &sandbox,
                1,
            )
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    assert!(rows.iter().any(|row| row.contains("root\\nfake approval")));
    assert!(
        !rows
            .iter()
            .any(|row| row.trim_start().starts_with("fake approval"))
    );
}

#[test]
fn the_approval_modal_shows_the_reason() {
    let backend = TestBackend::new(60, 14);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| render_approval_modal(f, "writing to src/main.rs"))
        .expect("draw");

    let content = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(content.contains("writing to src/main.rs"));
}

#[test]
fn a_message_with_a_raw_escape_byte_does_not_reach_the_terminal_buffer() {
    let mut session = Session::default();
    session.push_assistant("\x1b[31mfake red\x1b[0m", Vec::new());

    let content = render_history_to_string(&session.messages, 60);
    assert!(!content.chars().any(|c| c == '\u{1b}'));
    // The rest of the text should still be visible, just with the
    // control bytes neutralized rather than the whole message dropped.
    assert!(content.contains("fake red"));
}

/// Concatenates every span of every line back into one string, the
/// way the terminal would print them.
fn live_print_text(event: &AgentEvent) -> String {
    format_event_for_live_print(event)
        .iter()
        .flat_map(|l| l.line.spans.iter())
        .map(|s| s.content.as_ref())
        .collect()
}

#[test]
fn a_tool_detail_with_a_raw_escape_byte_does_not_reach_the_terminal_buffer() {
    // Tool activity no longer goes through `history_lines_for` (it's
    // printed live instead), so the escape-smuggling guard that used
    // to be checked on a `Role::Tool` message now belongs on the
    // live-print path.
    let text = live_print_text(&AgentEvent::ToolStarted {
        name: "bash".to_string(),
        detail: "\x1b[31mfake\x1b[0m".to_string(),
    });
    assert!(!text.chars().any(|c| c == '\u{1b}'));
    // The rest of the text should still be visible, just with the
    // control bytes neutralized rather than the whole detail being
    // dropped.
    assert!(text.contains("fake"));
}

#[test]
fn a_tool_started_event_renders_as_a_bullet_line() {
    let lines = format_event_for_live_print(&AgentEvent::ToolStarted {
        name: "bash".to_string(),
        detail: "{\"command\":\"ls\"}".to_string(),
    });
    let text: String = lines[0]
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("⏺"));
    assert!(text.contains("bash"));
    assert!(text.contains("ls"));
}

#[test]
fn workflow_resolution_reports_phase_skills_manifest_and_changed_skills() {
    let text = live_print_text(&AgentEvent::WorkflowResolved {
        phase: "implement".into(),
        skill_ids: vec!["build".into(), "test".into()],
        changed_skill_ids: vec!["test".into()],
        manifest_hash: "abc123".into(),
    });
    assert!(text.contains("phase=implement"));
    assert!(text.contains("skills=build, test"));
    assert!(text.contains("manifest=abc123"));
    assert!(text.contains("changed=test"));
}

#[test]
fn file_tool_activity_shows_only_the_target_path() {
    let lines = format_event_for_live_print(&AgentEvent::ToolStarted {
        name: "read".to_string(),
        detail: "{\"path\":\"a.txt\",\"limit\":50}".to_string(),
    });
    let text: String = lines[0]
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(text, "⏺ read(a.txt)");

    for name in ["write", "edit"] {
        let text = live_print_text(&AgentEvent::ToolStarted {
            name: name.to_string(),
            detail: r#"{"path":"src/main.rs","content":"hidden","old":"also hidden"}"#.to_string(),
        });
        assert_eq!(text, format!("⏺ {name}(src/main.rs)"));
        assert!(!text.contains("hidden"));
    }
}

#[test]
fn spawn_tool_activity_pairs_stable_names_with_declared_agent_types() {
    let text = live_print_text(&AgentEvent::ToolStarted {
        name: "spawn".to_string(),
        detail: serde_json::json!({
            "tasks": [
                {"type": "file-reader", "task": "private long prompt"},
                {"task": "another private prompt"}
            ]
        })
        .to_string(),
    });
    assert_eq!(text, "⏺ spawn(Sirius (file-reader), Canopus (auto))");
    assert!(!text.contains("prompt"));
}

#[test]
fn a_tool_started_event_with_no_arguments_renders_empty_parens() {
    let lines = format_event_for_live_print(&AgentEvent::ToolStarted {
        name: "list_skills".to_string(),
        detail: "{}".to_string(),
    });
    let text: String = lines[0]
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(text, "⏺ list_skills()");
}

#[test]
fn non_object_tool_call_detail_falls_back_to_the_raw_text() {
    let lines = format_event_for_live_print(&AgentEvent::ToolStarted {
        name: "weird".to_string(),
        detail: "not json".to_string(),
    });
    let text: String = lines[0]
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(text, "⏺ weird(not json)");
}

#[test]
fn a_tool_finished_event_reports_success_or_failure() {
    let ok = live_print_text(&AgentEvent::ToolFinished {
        name: "bash".to_string(),
        detail: "hello".to_string(),
        ok: true,
        result: String::new(),
        diff: None,
    });
    assert!(ok.contains("done"));
    let failed = live_print_text(&AgentEvent::ToolFinished {
        name: "bash".to_string(),
        detail: "boom".to_string(),
        ok: false,
        result: String::new(),
        diff: None,
    });
    assert!(failed.contains("failed"));
}

#[test]
fn a_tool_finished_event_previews_its_result_text() {
    let text = live_print_text(&AgentEvent::ToolFinished {
        name: "read".to_string(),
        detail: "{\"path\":\"a.txt\"}".to_string(),
        ok: true,
        result: "     1\thello from the file".to_string(),
        diff: None,
    });
    assert!(text.contains("done"));
    assert!(text.contains("hello from the file"));
    // タブは展開されてから切り詰められる
    assert!(!text.contains('\t'));
}

#[test]
fn a_long_tool_result_preview_is_truncated_at_the_shared_budget() {
    let text = live_print_text(&AgentEvent::ToolFinished {
        name: "bash".to_string(),
        detail: "{}".to_string(),
        ok: true,
        result: "x".repeat(TOOL_RESULT_PREVIEW_CHARS * 3),
        diff: None,
    });
    assert!(text.contains(&"x".repeat(TOOL_RESULT_PREVIEW_CHARS)));
    assert!(!text.contains(&"x".repeat(TOOL_RESULT_PREVIEW_CHARS + 1)));
    assert!(text.contains("..."));
}

#[test]
fn a_failed_tool_previews_its_error_message() {
    let text = live_print_text(&AgentEvent::ToolFinished {
        name: "write".to_string(),
        detail: "{}".to_string(),
        ok: false,
        result: "permission denied".to_string(),
        diff: None,
    });
    assert!(text.contains("failed"));
    assert!(text.contains("permission denied"));
}

#[test]
fn a_tool_result_preview_row_carries_no_background_and_a_tinted_foreground() {
    // A tool result preview covers arbitrary command output (e.g. a
    // `bash` "git commit"), not just source code read via `read` — a
    // DarkGray background applied indiscriminately to every preview
    // line looked like unwanted shading bleeding outside the input
    // prompt. Distinguished by foreground color instead (#989DCC).
    let lines = format_event_for_live_print(&AgentEvent::ToolFinished {
        name: "read".to_string(),
        detail: "{}".to_string(),
        ok: true,
        result: "fn main() {}".to_string(),
        diff: None,
    });
    let preview_line = lines
        .iter()
        .find(|l| l.line.spans.iter().any(|s| s.content.contains("fn main")))
        .expect("the result preview row should be present");
    assert!(
        preview_line.line.spans.iter().all(|s| s.style.bg.is_none()),
        "the preview row should carry no background tint"
    );
    assert!(
        preview_line
            .line
            .spans
            .iter()
            .all(|s| s.style.fg == Some(CODE_TEXT_COLOR)),
        "the preview row's text should use the tinted foreground color"
    );
}

#[test]
fn an_empty_tool_result_adds_no_preview_row() {
    let lines = format_event_for_live_print(&AgentEvent::ToolFinished {
        name: "write".to_string(),
        detail: "{}".to_string(),
        ok: true,
        result: String::new(),
        diff: None,
    });
    assert_eq!(lines.len(), 1);
}

#[test]
fn a_multiline_tool_result_preview_is_split_into_rows_and_capped() {
    let lines = format_event_for_live_print(&AgentEvent::ToolFinished {
        name: "bash".to_string(),
        detail: "{}".to_string(),
        ok: true,
        result: (0..50)
            .map(|i| format!("row {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
        diff: None,
    });
    for l in &lines {
        let text: String = l.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains('\n'), "a row still carries a newline");
    }
    // done行 + プレビュー(最大SPAWN_TASK_PREVIEW_LINES行 + 省略行)
    assert!(lines.len() <= 1 + TOOL_RESULT_PREVIEW_LINES + 1);
}

#[test]
fn a_tool_finished_event_with_a_diff_renders_added_and_removed_lines_with_a_header() {
    let diff = polaris_core::Diff {
        is_new_file: false,
        hunks: vec![polaris_core::DiffHunk {
            lines: vec![
                polaris_core::DiffLine::Context("unchanged".to_string()),
                polaris_core::DiffLine::Removed("old line".to_string()),
                polaris_core::DiffLine::Added("new line".to_string()),
            ],
        }],
        added: 1,
        removed: 1,
    };
    let event = AgentEvent::ToolFinished {
        name: "write".to_string(),
        detail: "{\"path\":\"a.txt\"}".to_string(),
        ok: true,
        result: String::new(),
        diff: Some(diff),
    };
    let joined = live_print_text(&event);
    assert!(joined.contains("Added 1 lines, removed 1 lines"));
    assert!(joined.contains("old line"));
    assert!(joined.contains("new line"));
}

#[test]
fn a_diff_longer_than_the_cap_is_truncated_with_a_notice() {
    let many_added: Vec<polaris_core::DiffLine> = (0..100)
        .map(|i| polaris_core::DiffLine::Added(format!("line {i}")))
        .collect();
    let diff = polaris_core::Diff {
        is_new_file: true,
        hunks: vec![polaris_core::DiffHunk { lines: many_added }],
        added: 100,
        removed: 0,
    };
    let event = AgentEvent::ToolFinished {
        name: "write".to_string(),
        detail: "{\"path\":\"big.txt\"}".to_string(),
        ok: true,
        result: String::new(),
        diff: Some(diff),
    };
    let lines = format_event_for_live_print(&event);
    // Header row + done/failed row + up to MAX_DIFF_LINES_SHOWN diff
    // rows + one elision row — well under the 100 lines the diff
    // itself carries.
    assert!(lines.len() < 100);
    let joined = live_print_text(&event);
    assert!(joined.contains("omitted"));
}

#[test]
fn a_spawn_started_event_mentions_backgrounded_agent() {
    let joined = live_print_text(&AgentEvent::SpawnStarted {
        display_name: "Sirius".to_string(),
        agent_type: "file-inspector".to_string(),
        task: "inspect agent.rs".to_string(),
    });
    assert!(joined.contains("Sirius"));
    assert!(joined.contains("file-inspector"));
    assert!(joined.contains("starting"));
}

#[test]
fn a_spawn_started_event_never_exposes_its_task_prompt() {
    let lines = format_event_for_live_print(&AgentEvent::SpawnStarted {
        display_name: "Sirius".to_string(),
        agent_type: "file-inspector".to_string(),
        task: "first line\nsecond line\nthird line".to_string(),
    });
    let rows: Vec<String> = lines
        .iter()
        .map(|l| l.line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();

    assert!(rows.iter().all(|r| !r.contains('\n')), "{rows:?}");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], "⏺ Sirius (file-inspector)");
    assert_eq!(rows[1], "  → starting");
    assert!(!rows.join(" ").contains("first line"));
}

#[test]
fn a_spawn_activity_is_a_single_dimmed_tool_row() {
    let lines = format_event_for_live_print(&AgentEvent::SpawnActivity {
        display_name: "Sirius".to_string(),
        agent_type: "file-reader".to_string(),
        name: "read".to_string(),
        detail: r#"{"path":"docs/filemap.md","limit":20}"#.to_string(),
    });
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].line.to_string(), "  → read(docs/filemap.md)");
    assert!(
        lines[0]
            .line
            .spans
            .iter()
            .all(|span| span.style.add_modifier.contains(Modifier::DIM))
    );
}

#[test]
fn a_spawn_finished_event_names_the_agent_type_and_its_outcome() {
    let lines = format_event_for_live_print(&AgentEvent::SpawnFinished {
        display_name: "Sirius".to_string(),
        agent_type: "file-inspector".to_string(),
        ok: true,
    });
    let joined: String = lines[0]
        .line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(joined.contains("file-inspector"));
    assert!(joined.contains("Sirius"));
    assert!(joined.contains("done"));
    assert_eq!(joined, "⏺ Sirius (file-inspector): done");
    assert_eq!(lines[0].line.spans[0].style.fg, Some(Color::Green));
    assert_eq!(lines[0].line.spans[1].style.fg, Some(Color::White));
}

#[test]
fn a_history_compacted_event_reports_before_and_after_counts() {
    let joined = live_print_text(&AgentEvent::HistoryCompacted {
        messages_before: 12,
        messages_after: 3,
        tokens_before: 48_201,
        tokens_after: 2_103,
    });
    assert!(joined.contains("12"));
    assert!(joined.contains('3'));
    assert!(joined.contains("48"));
    assert!(joined.contains("2,103") || joined.contains("2103"));
}

#[test]
fn ordinary_text_renders_unaffected_by_sanitization() {
    let mut session = Session::default();
    session.push_user("plain ascii and 日本語 text, nothing weird here.");

    let content = render_history_to_string(&session.messages, 60);
    assert!(content.contains("plain ascii and"));
    assert!(content.contains("nothing weird here."));
}

#[test]
fn an_approval_reason_with_a_control_character_is_sanitized() {
    let backend = TestBackend::new(60, 14);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| render_approval_modal(f, "writing to \x1b]0;pwned\x07src/main.rs"))
        .expect("draw");

    let content = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(!content.chars().any(|c| c == '\u{1b}'));
    assert!(content.contains("src/main.rs"));
}

#[test]
fn a_status_error_with_a_raw_escape_byte_does_not_reach_the_terminal_buffer() {
    let header = test_header();
    let status = Status::Error("\x1b[31mfake\x1b[0m".to_string());

    let content = render_footer_to_string("", &status, &header, &[], 0, 60, 6);
    assert!(!content.chars().any(|c| c == '\u{1b}'));
    assert!(content.contains("error: "));
    assert!(content.contains("fake"));
}

#[test]
fn a_multi_line_reply_renders_as_multiple_lines_not_one_clipped_line() {
    let mut session = Session::default();
    session.push_assistant(
        "first paragraph\nsecond paragraph\nthird paragraph",
        Vec::new(),
    );

    let lines = history_lines_for(&session.messages);
    let mut buf =
        ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 60, lines.len() as u16));
    let area = buf.area;
    render_history_into(&mut buf, area, &lines);
    // Every physical line of the message must land on its own row of
    // the rendered buffer, not be squashed onto a single row.
    let rows: Vec<String> = (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();

    assert!(rows.iter().any(|r| r.contains("polaris: first paragraph")));
    assert!(rows.iter().any(|r| r.contains("second paragraph")));
    assert!(rows.iter().any(|r| r.contains("third paragraph")));
    // The label prefix should not repeat on continuation lines.
    assert!(!rows.iter().any(|r| r.contains("polaris: second paragraph")));
}

#[test]
fn the_footer_shows_the_model_and_effort_together() {
    let header = HeaderInfo {
        cwd: "/tmp/example",
        cwd_short: "~/example",
        effort_name: "high",
        provider_name: "openai",
        model_name: "gpt-5.6-sol",
        usage: polaris_provider::Usage::default(),
    };
    let content = render_footer_to_string("", &Status::Idle, &header, &[], 0, 80, 3);
    // Matches the verified screenshot's footer shape: "{model}
    // {effort} · {cwd}".
    assert!(content.contains("gpt-5.6-sol high"));
    assert!(content.contains("~/example"));
}

#[test]
fn tool_calls_and_tool_results_are_left_out_of_the_batch_printed_history() {
    // Tool activity is printed live from the `AgentEvent` stream
    // while the turn runs (`format_event_for_live_print`), so the
    // post-turn batch print must not repeat it — only the user's and
    // the assistant's own text belongs here.
    let mut session = Session::default();
    session.push_user("what's in a.txt?");
    session.push_assistant_tool_calls(
        "",
        vec![polaris_provider::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "a.txt"}),
        }],
        Vec::new(),
    );
    session.push_tool_result("c1", "hello from the tool");
    session.push_assistant("a.txt contains a greeting", Vec::new());

    let content = render_history_to_string(&session.messages, 60);
    assert!(
        !content.contains("read"),
        "the tool name must not be reprinted: {content}"
    );
    assert!(
        !content.contains("hello from the tool"),
        "the tool result must not be reprinted: {content}"
    );
    assert!(content.contains("what's in a.txt?"));
    assert!(content.contains("a.txt contains a greeting"));
}

#[test]
fn an_assistant_message_carrying_only_tool_calls_produces_no_history_lines() {
    let mut session = Session::default();
    session.push_assistant_tool_calls(
        "",
        vec![polaris_provider::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "a.txt"}),
        }],
        Vec::new(),
    );
    session.push_tool_result("c1", "1\tfirst line\n2\tsecond line\n");

    assert!(history_lines_for(&session.messages).is_empty());
}

#[test]
fn a_long_tool_detail_is_truncated_in_the_live_print() {
    let long_body: String = "x".repeat(500);
    let text = live_print_text(&AgentEvent::ToolStarted {
        name: "read".to_string(),
        detail: long_body.clone(),
    });
    // The full 500-character body must not appear verbatim; only a
    // prefix of it should.
    assert!(!text.contains(&long_body));
    assert!(text.contains(&"x".repeat(TOOL_RESULT_PREVIEW_CHARS)));
}

#[test]
fn bold_text_is_rendered_with_the_bold_modifier() {
    let mut session = Session::default();
    session.push_assistant("this is **bold** text", Vec::new());

    let lines = history_lines_for(&session.messages);
    let mut buf =
        ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 60, lines.len() as u16));
    let area = buf.area;
    render_history_into(&mut buf, area, &lines);
    let bold_cell = (0..buf.area.width)
        .flat_map(|x| (0..buf.area.height).map(move |y| (x, y)))
        .find(|&(x, y)| {
            buf[(x, y)].symbol() == "b" && {
                let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
                row.contains("bold")
            }
        });
    let (x, y) = bold_cell.expect("the word 'bold' should appear somewhere");
    assert!(
        buf[(x, y)]
            .modifier
            .contains(ratatui::style::Modifier::BOLD),
        "the 'b' in 'bold' should carry the BOLD modifier"
    );
}

#[test]
fn a_fenced_code_block_uses_the_code_text_color_not_a_background() {
    let mut session = Session::default();
    session.push_assistant("```\nfn main() {}\n```", Vec::new());

    let lines = history_lines_for(&session.messages);
    let code_line = lines
        .iter()
        .find(|l| l.line.spans.iter().any(|s| s.content.contains("fn main")))
        .expect("the code-block content row should be present");
    assert!(
        code_line.line.spans.iter().all(|s| s.style.bg.is_none()),
        "the code-block row should carry no background tint"
    );
    assert!(
        code_line
            .line
            .spans
            .iter()
            .all(|s| s.style.fg == Some(CODE_TEXT_COLOR)),
        "the code-block row's text should use the code text color"
    );
}

#[test]
fn inline_code_and_surrounding_text_both_render_without_the_backticks() {
    let mut session = Session::default();
    session.push_assistant("run `cargo test` now", Vec::new());

    let content = render_history_to_string(&session.messages, 60);
    assert!(content.contains("cargo test"));
    assert!(!content.contains('`'));
}

#[test]
fn inline_code_uses_the_code_text_color_not_a_background() {
    let spans = format_inline("run `git commit` now", Color::Reset);
    let code_span = spans
        .iter()
        .find(|s| s.content.contains("git commit"))
        .expect("the inline code span should be present");
    assert!(
        code_span.style.bg.is_none(),
        "inline code should carry no background tint"
    );
    assert_eq!(code_span.style.fg, Some(CODE_TEXT_COLOR));
}

#[test]
fn a_level_1_heading_strips_the_hash_and_renders_bold() {
    let mut session = Session::default();
    session.push_assistant("# Title", Vec::new());

    let lines = history_lines_for(&session.messages);
    assert_eq!(lines.len(), 1);
    let text: String = lines[0]
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(text, "polaris: Title");
    assert!(
        lines[0]
            .line
            .spans
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::BOLD)),
        "every span of a heading line should be bold"
    );
}

#[test]
fn heading_levels_2_through_6_all_strip_their_hashes() {
    let mut session = Session::default();
    for level in 2..=6 {
        session.push_assistant(&format!("{} h{level}", "#".repeat(level)), Vec::new());
    }

    let lines = history_lines_for(&session.messages);
    for (i, line) in lines.iter().enumerate() {
        let text: String = line.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, format!("polaris: h{}", i + 2));
    }
}

#[test]
fn a_bold_span_inside_a_heading_is_still_bold_and_the_markers_are_gone() {
    let mut session = Session::default();
    session.push_assistant("## has **emphasis** inside", Vec::new());

    let lines = history_lines_for(&session.messages);
    let text: String = lines[0]
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(text, "polaris: has emphasis inside");
    assert!(!text.contains('*'));
    assert!(
        lines[0]
            .line
            .spans
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::BOLD))
    );
}

#[test]
fn a_hash_without_a_following_space_is_not_a_heading() {
    let mut session = Session::default();
    session.push_assistant("#nothashheading", Vec::new());

    let lines = history_lines_for(&session.messages);
    let text: String = lines[0]
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(text, "polaris: #nothashheading");
    assert!(
        !lines[0]
            .line
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
    );
}

#[test]
fn a_hash_inside_a_code_block_is_not_treated_as_a_heading() {
    let mut session = Session::default();
    session.push_assistant("```\n# not a heading\n```", Vec::new());

    let lines = history_lines_for(&session.messages);
    let inside = lines
        .iter()
        .find(|l| {
            l.line
                .spans
                .iter()
                .any(|s| s.content.contains("not a heading"))
        })
        .expect("the code-block line should be present");
    let text: String = inside
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(text, "# not a heading");
}

#[test]
fn a_top_level_bullet_renders_with_a_bullet_marker_and_no_dash() {
    let mut session = Session::default();
    session.push_assistant("- first item", Vec::new());

    let lines = history_lines_for(&session.messages);
    let text: String = lines[0]
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(text, "polaris: \u{2022} first item");
}

#[test]
fn a_star_bullet_also_renders_with_a_bullet_marker() {
    let mut session = Session::default();
    session.push_assistant("* first item", Vec::new());

    let lines = history_lines_for(&session.messages);
    let text: String = lines[0]
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(text, "polaris: \u{2022} first item");
}

#[test]
fn a_nested_bullet_keeps_its_leading_indentation() {
    let mut session = Session::default();
    session.push_assistant("top\n  - nested item", Vec::new());

    let lines = history_lines_for(&session.messages);
    assert_eq!(lines.len(), 2);
    let text: String = lines[1]
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(text, "  \u{2022} nested item");
}

#[test]
fn a_dash_inside_a_code_block_is_not_treated_as_a_bullet() {
    let mut session = Session::default();
    session.push_assistant("```\n- not a bullet\n```", Vec::new());

    let lines = history_lines_for(&session.messages);
    let inside = lines
        .iter()
        .find(|l| l.line.spans.iter().any(|s| s.content.contains("bullet")))
        .expect("the code-block line should be present");
    let text: String = inside
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(text, "- not a bullet");
}

#[test]
fn user_and_assistant_lines_are_distinguished_by_shading_not_color() {
    // Both roles render in the terminal's own default foreground — no
    // tinted fg competes with the user line's full-row gray background
    // as a second, redundant "this is different" cue. The shading
    // itself (`HistoryLine::shaded`, checked directly on the built
    // lines rather than the rendered buffer) is what distinguishes
    // them.
    let mut session = Session::default();
    session.push_user("hello");
    session.push_assistant("hi there", Vec::new());

    let lines = history_lines_for(&session.messages);
    let mut buf =
        ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 60, lines.len() as u16));
    let area = buf.area;
    render_history_into(&mut buf, area, &lines);
    let find_row_color = |needle: &str| {
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            if row.contains(needle) {
                return buf[(0, y)].fg;
            }
        }
        panic!("row containing {needle:?} not found");
    };
    assert_eq!(find_row_color("hello"), find_row_color("hi there"));
    assert!(lines.iter().any(|l| l.shaded));
    assert!(lines.iter().any(|l| !l.shaded));
}

#[test]
fn apply_selection_highlight_colors_only_the_selected_cells() {
    use crate::selection::{Selection, TextPos};
    let wrapped = vec![HistoryLine::plain(Line::from("hello world"))];
    let area = ratatui::layout::Rect::new(0, 0, 20, 5);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    render_history_into(&mut buf, area, &wrapped);

    let sel = Selection {
        anchor: TextPos { line: 0, col: 2 },
        cursor: TextPos { line: 0, col: 7 },
        dragging: false,
    };
    apply_selection_highlight(&mut buf, area, 0..1, &wrapped, &sel);

    for x in 0..2 {
        assert_ne!(
            buf[(x, 0)].bg,
            SELECTION_HIGHLIGHT_COLOR,
            "column {x} should not be highlighted"
        );
    }
    for x in 2..7 {
        assert_eq!(
            buf[(x, 0)].bg,
            SELECTION_HIGHLIGHT_COLOR,
            "column {x} should be highlighted"
        );
    }
    for x in 7..20 {
        assert_ne!(
            buf[(x, 0)].bg,
            SELECTION_HIGHLIGHT_COLOR,
            "column {x} should not be highlighted"
        );
    }
}

#[test]
fn apply_selection_highlight_ignores_lines_outside_the_visible_window() {
    use crate::selection::{Selection, TextPos};
    let wrapped = vec![
        HistoryLine::plain(Line::from("scrolled off the top")),
        HistoryLine::plain(Line::from("visible line")),
    ];
    let area = ratatui::layout::Rect::new(0, 0, 30, 5);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    // window is 1..2 -- only wrapped[1] is on screen, at row 0.
    render_history_into(&mut buf, area, &wrapped[1..2]);

    let sel = Selection {
        anchor: TextPos { line: 0, col: 0 },
        cursor: TextPos { line: 0, col: 5 },
        dragging: false,
    };
    apply_selection_highlight(&mut buf, area, 1..2, &wrapped, &sel);

    for x in 0..30 {
        assert_ne!(
            buf[(x, 0)].bg,
            SELECTION_HIGHLIGHT_COLOR,
            "column {x} should not be highlighted — line 0 is scrolled off"
        );
    }
}

#[test]
fn apply_selection_highlight_preserves_existing_colors() {
    use crate::selection::{Selection, TextPos};
    let wrapped = vec![HistoryLine::plain(Line::from(Span::styled(
        "colored text",
        Style::default().fg(Color::Red),
    )))];
    let area = ratatui::layout::Rect::new(0, 0, 20, 5);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    render_history_into(&mut buf, area, &wrapped);

    let sel = Selection {
        anchor: TextPos { line: 0, col: 0 },
        cursor: TextPos { line: 0, col: 7 },
        dragging: false,
    };
    apply_selection_highlight(&mut buf, area, 0..1, &wrapped, &sel);

    assert_eq!(
        buf[(0, 0)].fg,
        Color::Red,
        "the red fg color must survive the highlight"
    );
    assert_eq!(buf[(0, 0)].bg, SELECTION_HIGHLIGHT_COLOR);
}

#[test]
fn sanitize_replaces_c0_controls_and_del_but_keeps_newline_and_tab() {
    let input = "a\x00b\x1bc\x7fd\ne\tf";
    let sanitized = sanitize(input);
    assert!(!sanitized.contains('\u{0}'));
    assert!(!sanitized.contains('\u{1b}'));
    assert!(!sanitized.contains('\u{7f}'));
    assert!(sanitized.contains('\n'));
    assert!(sanitized.contains('\t'));
    assert_eq!(sanitized, "a\u{fffd}b\u{fffd}c\u{fffd}d\ne\tf");
}

#[test]
fn transcript_viewer_includes_tool_messages_and_sanitizes_control_bytes() {
    let mut assistant = polaris_provider::Message::assistant("確認しました");
    assistant.tool_calls.push(polaris_provider::ToolCall {
        id: "call-1".into(),
        name: "read".into(),
        arguments: serde_json::json!({"path": "src/main.rs"}),
    });
    let messages = vec![
        polaris_provider::Message::user("質問\u{1b}[2J"),
        assistant,
        polaris_provider::Message::tool_result("call-1", "file contents"),
    ];
    let text = transcript_lines_for(&messages)
        .into_iter()
        .map(|line| line.line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("user\n質問"));
    assert!(text.contains("→ read({\"path\":\"src/main.rs\"})"));
    assert!(text.contains("tool\nfile contents"));
    assert!(!text.contains('\u{1b}'));
}
