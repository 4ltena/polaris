//! The accessibility driver for macOS, through System Events.
//!
//! This reaches the accessibility tree with `osascript` rather than linking
//! the Carbon `AXUIElement` APIs. That costs a process per call and returns
//! coarser information, and it buys not adding an Objective-C binding to a
//! workspace that has none. [`super::Driver`] is where a linked driver
//! would go instead, and nothing outside this file would change.
//!
//! Every value that reaches AppleScript is a string literal built by
//! [`quote`]. The application name comes from the model and the element
//! labels come from the interface, so both are untrusted: a label
//! containing a quote must end up as a quote inside a string, never as the
//! end of one.
//!
//! ponytail: one `osascript` per call, no session reuse. If a real task
//! needs dozens of steps, hold one `osascript` open and speak to it over
//! stdin instead.

use super::{Candidate, Driver};
use std::process::Command;

/// An AppleScript string literal holding exactly `value`.
///
/// AppleScript escapes only the backslash and the double quote inside a
/// literal, and a newline is written as `\n`. Control characters are
/// dropped rather than encoded: an element label has no use for them, and
/// leaving them in is how a literal stops being one.
pub(crate) fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' => out.push_str("\\n"),
            ch if (ch as u32) < 0x20 => {}
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn run(script: &str) -> Result<String, String> {
    let out = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|error| format!("Cannot start osascript: {error}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Whether this machine has granted the accessibility permission.
pub fn permitted() -> bool {
    run("tell application \"System Events\" to return UI elements enabled")
        .map(|value| value == "true")
        .unwrap_or(false)
}

pub struct MacDriver;

impl Driver for MacDriver {
    /// The labelled controls of the application's front window.
    ///
    /// Role and label are fetched as two lists rather than one interleaved
    /// string, so a label containing the separator cannot invent an extra
    /// candidate. A mismatch in length is an error rather than a guess.
    fn candidates(&self, app: &str) -> Result<Vec<Candidate>, String> {
        let app = quote(app);
        let roles = run(&format!(
            "tell application \"System Events\" to tell process {app} \
             to get role of every UI element of front window"
        ))?;
        let labels = run(&format!(
            "tell application \"System Events\" to tell process {app} \
             to get name of every UI element of front window"
        ))?;
        let split = |value: &str| {
            value
                .split(", ")
                .map(str::trim)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        };
        let (roles, labels) = (split(&roles), split(&labels));
        if roles.len() != labels.len() {
            return Err("Role and name counts do not match".into());
        }
        Ok(roles
            .into_iter()
            .zip(labels)
            .filter(|(_, label)| !label.is_empty() && label != "missing value")
            .map(|(role, label)| Candidate { label, role })
            .collect())
    }

    fn activate(&self, app: &str, element: &Candidate) -> Result<(), String> {
        let (app, label) = (quote(app), quote(&element.label));
        run(&format!(
            "tell application \"System Events\" to tell process {app} \
             to perform action \"AXPress\" of (first UI element of front window \
             whose name is {label})"
        ))
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A label that tries to end the literal and append its own command
    /// stays one literal. This is the interface's text reaching a script,
    /// which is where prompt injection would arrive if it could.
    #[test]
    fn a_label_cannot_escape_its_string_literal() {
        let hostile = "OK\" & (do shell script \"rm -rf ~\") & \"";
        let quoted = quote(hostile);
        assert!(quoted.starts_with('"') && quoted.ends_with('"'));
        let bare = quoted
            .char_indices()
            .filter(|&(at, ch)| ch == '"' && (at == 0 || quoted.as_bytes()[at - 1] != b'\\'))
            .count();
        assert_eq!(bare, 2, "the literal was ended early: {quoted}");
        assert_eq!(quote("a\\b"), "\"a\\\\b\"");
        assert_eq!(quote("line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(quote("bell\u{7}"), "\"bell\"");
    }
}
