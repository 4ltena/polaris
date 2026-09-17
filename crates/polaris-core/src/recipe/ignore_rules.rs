//! M7c-3c: which `.gitignore` rules a setting command may add.
//!
//! Only additions, and only ones that cannot change what an existing line
//! already decided. Existing lines, their order, and negations are left
//! exactly as they are: a rule that would re-ignore something a negation
//! deliberately un-ignores is withheld rather than appended and explained
//! away afterwards.
/// A rule that was not added, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withheld {
    pub rule: String,
    pub reason: WithheldReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WithheldReason {
    /// The same line is already there.
    AlreadyPresent,
    /// A negation would stop meaning what it means today. The line is kept
    /// so a person can see what the collision was.
    ContradictsNegation { line: String },
}

/// What to append and what to leave out. `existing` is the file as it
/// stands; `wanted` is the fixed list from the recipe's profile, in the
/// order the recipe names them.
pub fn plan_additions<'a>(existing: &str, wanted: &[&'a str]) -> (Vec<&'a str>, Vec<Withheld>) {
    let lines: Vec<&str> = existing.lines().map(str::trim).collect();
    // A comment cannot also be a negation: a line starting with `#` never
    // starts with `!`, so stripping the `!` is the whole check.
    let negations: Vec<&str> = lines
        .iter()
        .filter_map(|line| line.strip_prefix('!'))
        .filter(|body| !body.is_empty())
        .collect();
    let mut add = Vec::new();
    let mut withheld = Vec::new();
    for rule in wanted {
        if lines.contains(rule) {
            withheld.push(Withheld {
                rule: (*rule).to_owned(),
                reason: WithheldReason::AlreadyPresent,
            });
            continue;
        }
        match negations.iter().find(|body| covers(rule, body)) {
            Some(body) => withheld.push(Withheld {
                rule: (*rule).to_owned(),
                reason: WithheldReason::ContradictsNegation {
                    line: format!("!{body}"),
                },
            }),
            None => add.push(*rule),
        }
    }
    (add, withheld)
}

/// Whether adding `rule` would re-ignore what `negated` un-ignores.
///
/// Deliberately generous about what counts as a collision. Working out
/// exactly what a gitignore pattern matches means implementing gitignore;
/// getting that subtly wrong would silently re-ignore a file someone asked
/// to keep. Withholding a rule costs an untidy ignore file, which a person
/// can see and fix.
fn covers(rule: &str, negated: &str) -> bool {
    let trim = |held: &str| {
        held.trim_start_matches('/')
            .trim_end_matches('/')
            .to_owned()
    };
    let (rule, negated) = (trim(rule), trim(negated));
    if rule == negated {
        return true;
    }
    // `dist` against `dist/keep.txt`: the directory the rule names contains
    // the path the negation names.
    if negated.starts_with(&format!("{rule}/")) {
        return true;
    }
    // `**/coverage` against `coverage/report`, and the reverse nesting.
    match rule.rsplit('/').next() {
        Some(last) if !last.is_empty() && last != rule => {
            negated == last || negated.starts_with(&format!("{last}/"))
        }
        _ => false,
    }
}

/// Appends the rules to the project's `.gitignore`, in the order given.
///
/// Only appends: existing lines keep their text and their order, because
/// their order is what decides which rule wins. A file that did not end in
/// a newline gets one first, so the last existing rule is not merged with
/// the first new one.
pub fn append(project_root: &std::path::Path, rules: &[&str]) -> std::io::Result<()> {
    use std::io::Write;
    if rules.is_empty() {
        return Ok(());
    }
    let path = project_root.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    for rule in rules {
        writeln!(file, "{rule}")?;
    }
    file.sync_all()
}

#[cfg(test)]
mod tests;
