//! File links remain ordinary Markdown in admission, drafts, and history.
//! Only the composer treats their complete spelling as one editable block.

use std::{
    ops::Range,
    path::{Path, PathBuf},
};

const PREFIX: &str = "[file](";

pub(super) struct Block {
    pub(super) range: Range<usize>,
    pub(super) label: String,
}

pub(super) fn blocks(text: &str) -> Vec<Block> {
    let mut result = Vec::new();
    let mut after = 0;
    while let Some(offset) = text[after..].find(PREFIX) {
        let start = after + offset;
        let url_start = start + PREFIX.len();
        let Some(close) = text[url_start..].find(')') else {
            break;
        };
        let end = url_start + close;
        if let Some(path) = local_url(&text[url_start..end]) {
            result.push(Block {
                range: start..end + 1,
                label: format!(
                    "[{}]",
                    path.file_name()
                        .unwrap_or(path.as_os_str())
                        .to_string_lossy()
                ),
            });
        }
        after = end + 1;
    }
    result
}

fn local_url(text: &str) -> Option<PathBuf> {
    let url = url::Url::parse(text).ok()?;
    if url.scheme() != "file"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_some_and(|host| host != "localhost")
    {
        return None;
    }
    let path = url.to_file_path().ok()?;
    if !path.is_absolute()
        || path
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(char::is_control)
    {
        return None;
    }
    Some(path)
}

fn file_block(value: &str) -> Option<String> {
    let path = if value.starts_with("file:") {
        local_url(value)?
    } else if let Some(tail) = value.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME")?).join(tail)
    } else {
        let path = Path::new(value);
        if !path.is_absolute() {
            return None;
        }
        path.to_path_buf()
    };
    if path
        .as_os_str()
        .to_string_lossy()
        .chars()
        .any(char::is_control)
    {
        return None;
    }
    let path = path.canonicalize().ok()?;
    if !path.is_file() {
        return None;
    }
    let url = url::Url::from_file_path(path)
        .ok()?
        .to_string()
        .replace('(', "%28")
        .replace(')', "%29");
    Some(format!("{PREFIX}{url})"))
}

// Decode quoting and backslash escaping as data. No shell, variable expansion,
// command substitution, file body, or network operation is involved.
fn path_words(text: &str) -> Option<Vec<String>> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for c in text.chars() {
        if escaped {
            word.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if c == delimiter {
                quote = None;
            } else {
                word.push(c);
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c.is_whitespace() {
            if !word.is_empty() {
                result.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
        if result.len() > 64 {
            return None;
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if !word.is_empty() {
        result.push(word);
    }
    (!result.is_empty() && result.len() <= 64).then_some(result)
}

fn file_list(text: &str) -> Option<String> {
    path_words(text)?
        .iter()
        .map(|word| file_block(word))
        .collect::<Option<Vec<_>>>()
        .map(|links| links.join(" "))
}

pub(super) fn insert_paste(
    buffer: &mut String,
    cursor: &mut usize,
    text: &str,
) -> Result<(), &'static str> {
    let limit = polaris_core::session_settings::MAX_DRAFT_BYTES;
    if text.len() > limit {
        return Err("Pasted content exceeds the input limit");
    }
    let insertion = file_list(text).unwrap_or_else(|| text.to_owned());
    if buffer.len().saturating_add(insertion.len()) > limit {
        return Err("Pasted content exceeds the input limit");
    }
    if let Some(block) = blocks(buffer)
        .iter()
        .find(|block| block.range.start < *cursor && *cursor < block.range.end)
    {
        *cursor = block.range.end;
    }
    buffer.insert_str(*cursor, &insertion);
    *cursor += insertion.len();
    Ok(())
}

/// Terminals can send a drop as a burst of ordinary characters rather than a
/// Paste event. After the burst is quiet, recognize its trailing absolute paths.
/// A manually entered existing absolute path has the same terminal spelling.
pub(super) fn normalize_tail(buffer: &mut String, cursor: &mut usize) -> bool {
    let Some((start, end, links)) = tail_link(buffer, *cursor) else {
        return false;
    };
    let end_of_links = start + links.len();
    buffer.replace_range(start..end, &links);
    *cursor = end_of_links;
    true
}

fn tail_link(buffer: &str, cursor: usize) -> Option<(usize, usize, String)> {
    if buffer.len() > polaris_core::session_settings::MAX_DRAFT_BYTES {
        return None;
    }
    let end = cursor;
    let candidates = buffer[..end]
        .char_indices()
        .filter(|(index, c)| {
            matches!(*c, '/' | '~' | '\'' | '"' | 'f')
                && (*index == 0 || buffer[..*index].ends_with(char::is_whitespace))
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    for start in candidates {
        let Some(links) = file_list(&buffer[start..end]) else {
            continue;
        };
        if start + links.len() + buffer.len() - end
            > polaris_core::session_settings::MAX_DRAFT_BYTES
        {
            return None;
        }
        return Some((start, end, links));
    }
    None
}

pub(super) fn may_have_tail(buffer: &str, cursor: usize) -> bool {
    tail_link(buffer, cursor).is_some()
}

pub(super) fn displayed_prefix(text: &str, cursor: usize) -> String {
    let mut rendered = String::new();
    let mut after = 0;
    for block in blocks(text) {
        if cursor <= block.range.start {
            break;
        }
        rendered.push_str(&text[after..block.range.start]);
        if cursor < block.range.end {
            return rendered;
        }
        rendered.push_str(&block.label);
        after = block.range.end;
    }
    rendered.push_str(&text[after..cursor]);
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{InputAction, apply_key};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn dropped_unicode_and_quoted_files_are_individual_atomic_blocks() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("資料 (1).txt");
        let b = root.path().join("two.txt");
        std::fs::write(&a, "not included").unwrap();
        std::fs::write(&b, "not included").unwrap();
        let mut buffer = "確認 ".to_owned();
        let mut cursor = buffer.len();
        insert_paste(
            &mut buffer,
            &mut cursor,
            &format!("'{}' {}", a.display(), b.display()),
        )
        .unwrap();
        assert_eq!(blocks(&buffer).len(), 2);
        assert_eq!(
            displayed_prefix(&buffer, cursor),
            "確認 [資料 (1).txt] [two.txt]"
        );
        assert!(!buffer.contains("not included"));
        apply_key(
            &mut buffer,
            &mut cursor,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(blocks(&buffer).len(), 1);
        assert_eq!(displayed_prefix(&buffer, cursor), "確認 [資料 (1).txt] ");
    }

    #[test]
    fn raw_terminal_drop_and_restored_text_keep_the_link_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("my file.txt");
        std::fs::write(&path, "synthetic").unwrap();
        let mut buffer = format!("read {} ", path.display().to_string().replace(' ', "\\ "));
        let mut cursor = buffer.len();
        assert!(normalize_tail(&mut buffer, &mut cursor));
        assert_eq!(blocks(&buffer).len(), 1);
        let submitted = match apply_key(
            &mut buffer,
            &mut cursor,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ) {
            InputAction::Submit(text) => text,
            _ => panic!("submission"),
        };
        // Reopening a draft/history spelling needs no separate block registry.
        buffer = submitted;
        cursor = buffer.trim_end().len();
        apply_key(
            &mut buffer,
            &mut cursor,
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
        );
        assert_eq!(&buffer[..cursor], "read ");
        apply_key(
            &mut buffer,
            &mut cursor,
            KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
        );
        assert_eq!(buffer, "read ");
    }

    #[test]
    fn an_unbracketed_drop_in_the_middle_is_ready_for_one_backspace() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        std::fs::write(&path, "synthetic").unwrap();
        let prefix = format!("before {} ", path.display());
        let mut buffer = format!("{prefix} after");
        let mut cursor = prefix.len();
        assert!(normalize_tail(&mut buffer, &mut cursor));
        assert_eq!(crate::file_input::blocks(&buffer).len(), 1);
        apply_key(
            &mut buffer,
            &mut cursor,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(buffer, "before  after");
        assert_eq!(cursor, "before ".len());
    }

    #[test]
    fn prose_urls_missing_files_and_unbalanced_quoting_stay_plain_text() {
        for text in [
            "ordinary text",
            "https://example.invalid/file",
            "file://remote.invalid/file",
            "/missing/file",
            "'/missing/file",
        ] {
            let mut buffer = String::new();
            let mut cursor = 0;
            insert_paste(&mut buffer, &mut cursor, text).unwrap();
            assert_eq!(buffer, text);
            assert!(blocks(&buffer).is_empty());
        }
        let mut buffer = "unchanged".to_owned();
        let mut cursor = buffer.len();
        assert!(
            insert_paste(
                &mut buffer,
                &mut cursor,
                &"x".repeat(polaris_core::session_settings::MAX_DRAFT_BYTES + 1)
            )
            .is_err()
        );
        assert_eq!(buffer, "unchanged");
    }
}
