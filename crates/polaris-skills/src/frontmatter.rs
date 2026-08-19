//! SKILL.md frontmatter parsing. Validates only the constraints the specification lays down; adds no constraints of its own.

/// The upper bound the specification places on `name`.
const NAME_MAX: usize = 64;
/// The upper bound the specification places on `description`.
const DESCRIPTION_MAX: usize = 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SkillError {
    #[error("{skill}'s SKILL.md has no frontmatter")]
    NoFrontmatter { skill: String },
    #[error("{skill}'s SKILL.md is missing the required field {field}")]
    MissingField { skill: String, field: &'static str },
    #[error("name {name} violates the specification's naming rules")]
    InvalidName { name: String },
    #[error("name {name} does not match the parent directory name {dir}")]
    NameMismatch { name: String, dir: String },
    #[error("{skill}'s SKILL.md has a description length {len} outside the allowed range")]
    InvalidDescription { skill: String, len: usize },
    #[error("{skill}'s SKILL.md field {field} contains unsupported syntax: {detail}")]
    UnsupportedSyntax {
        skill: String,
        field: &'static str,
        detail: String,
    },
}

/// The specification's naming rule: lowercase alphanumerics and hyphens only, no leading or trailing hyphen, no consecutive hyphens, 1 to 64 characters.
fn name_is_valid(name: &str) -> bool {
    if name.is_empty() || name.chars().count() > NAME_MAX {
        return false;
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return false;
    }
    name.chars()
        .all(|c| c == '-' || c.is_ascii_digit() || c.is_ascii_lowercase())
}

/// From `start` onward, collects consecutive lines that either start with
/// whitespace or are completely empty. Once a line without indentation is
/// reached, that is the next top-level key or the terminator, so collection
/// stops there.
fn collect_indented_block<'a>(lines: &[&'a str], start: usize) -> Vec<&'a str> {
    let mut out = Vec::new();
    if start >= lines.len() {
        return out;
    }
    for line in &lines[start..] {
        if line.is_empty() || line.starts_with(' ') || line.starts_with('\t') {
            out.push(*line);
        } else {
            break;
        }
    }
    out
}

/// Strips the common leading indentation from the block's lines and joins
/// them. When `fold` is true, joins as `>` (folded), turning newlines into
/// spaces; when false, joins as `|` (literal), keeping newlines. Drops
/// trailing empty lines.
fn join_block(block_lines: &[&str], fold: bool) -> String {
    let indent = block_lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);

    let mut stripped: Vec<String> = block_lines
        .iter()
        .map(|l| {
            let l = l.strip_suffix('\r').unwrap_or(l);
            if l.len() >= indent {
                l[indent..].to_string()
            } else {
                String::new()
            }
        })
        .collect();

    while stripped.last().is_some_and(|s| s.is_empty()) {
        stripped.pop();
    }

    if fold {
        stripped.join(" ")
    } else {
        stripped.join("\n")
    }
}

/// Extracts a value from a frontmatter `key: value` line. Besides a
/// single-line plain value or quoted value, this accepts the block scalars
/// `|` (literal, keeps newlines) and `>` (folded, folds newlines into
/// spaces), as well as a plain-value multi-line continuation. Syntax that
/// goes beyond what the specification requires (block scalars with a
/// chomping indicator or an explicit indentation indicator, for example)
/// cannot be interpreted with confidence, so rather than fabricate a value
/// this returns `SkillError::UnsupportedSyntax`. Unknown keys themselves are
/// silently skipped — the specification allows optional fields.
fn field(lines: &[&str], key: &'static str, skill: &str) -> Result<Option<String>, SkillError> {
    for (i, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        let value_part = rest.trim();

        if value_part == "|" || value_part == ">" {
            let block = collect_indented_block(lines, i + 1);
            return Ok(Some(join_block(&block, value_part == ">")));
        }
        if value_part.starts_with('|') || value_part.starts_with('>') {
            // Block scalar variants such as a chomping indicator (`|-`/`|+`)
            // or an explicit indentation indicator (`|2`) are out of scope.
            // Reject rather than fabricate a value.
            return Err(SkillError::UnsupportedSyntax {
                skill: skill.to_string(),
                field: key,
                detail: value_part.to_string(),
            });
        }

        let quoted = value_part
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'));
        if let Some(v) = quoted {
            return Ok(Some(v.to_string()));
        }

        let continuation = collect_indented_block(lines, i + 1);
        if continuation.is_empty() {
            return Ok(Some(value_part.to_string()));
        }
        let folded = join_block(&continuation, true);
        let combined = if folded.is_empty() {
            value_part.to_string()
        } else {
            format!("{value_part} {folded}")
        };
        return Ok(Some(combined));
    }
    Ok(None)
}

/// Parses the frontmatter and returns `(name, description, body)`.
pub fn parse(text: &str, dir_name: &str) -> Result<(String, String, String), SkillError> {
    let rest = text
        .strip_prefix("---")
        .ok_or_else(|| SkillError::NoFrontmatter {
            skill: dir_name.to_string(),
        })?;
    let rest = rest.trim_start_matches(['\r', '\n']);

    // If `rest` starts directly with `---`, the opening and closing
    // delimiters are adjacent — empty frontmatter. Otherwise, look for the
    // next `\n---` as the terminator.
    let (front, after_close) = if let Some(after) = rest.strip_prefix("---") {
        ("", after)
    } else {
        let end = rest
            .find("\n---")
            .ok_or_else(|| SkillError::NoFrontmatter {
                skill: dir_name.to_string(),
            })?;
        (&rest[..end], &rest[end + 4..])
    };

    // Even when the closing delimiter is written with more than three
    // hyphens, as in `----`, this drops only the extra hyphens directly
    // following the delimiter line. Body content always begins after a
    // newline, so this trim_start_matches never eats into the body.
    let after_close = after_close.trim_start_matches('-');
    let body = after_close.trim_start_matches(['\r', '\n']).to_string();

    let front_lines: Vec<&str> = front.lines().collect();

    let name = field(&front_lines, "name", dir_name)?.ok_or_else(|| SkillError::MissingField {
        skill: dir_name.to_string(),
        field: "name",
    })?;
    let description =
        field(&front_lines, "description", dir_name)?.ok_or_else(|| SkillError::MissingField {
            skill: dir_name.to_string(),
            field: "description",
        })?;

    if !name_is_valid(&name) {
        return Err(SkillError::InvalidName { name });
    }
    if name != dir_name {
        return Err(SkillError::NameMismatch {
            name,
            dir: dir_name.to_string(),
        });
    }
    let len = description.chars().count();
    if len == 0 || len > DESCRIPTION_MAX {
        return Err(SkillError::InvalidDescription {
            skill: dir_name.to_string(),
            len,
        });
    }

    Ok((name, description, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "---\nname: git-commit\ndescription: Creates a commit.\n---\n\nBody text.\n";

    #[test]
    fn parses_name_description_and_body() {
        let (name, desc, body) = parse(GOOD, "git-commit").expect("should be readable");
        assert_eq!(name, "git-commit");
        assert_eq!(desc, "Creates a commit.");
        assert_eq!(body.trim(), "Body text.");
    }

    #[test]
    fn rejects_a_name_that_does_not_match_the_directory() {
        let err = parse(GOOD, "other-dir")
            .expect_err("mismatch with the parent directory name should be rejected");
        assert!(matches!(err, SkillError::NameMismatch { .. }));
    }

    #[test]
    fn rejects_names_violating_the_specification() {
        for bad in [
            "Git-Commit",
            "-lead",
            "trail-",
            "double--hyphen",
            "under_score",
        ] {
            let text = format!("---\nname: {bad}\ndescription: x\n---\nbody\n");
            assert!(parse(&text, bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn rejects_an_empty_or_oversized_description() {
        let empty = "---\nname: a\ndescription: \"\"\n---\nbody\n";
        assert!(
            parse(empty, "a").is_err(),
            "an empty description should be rejected"
        );

        let long = format!(
            "---\nname: a\ndescription: \"{}\"\n---\nbody\n",
            "x".repeat(1025)
        );
        assert!(
            parse(&long, "a").is_err(),
            "a description over 1024 characters should be rejected"
        );
    }

    #[test]
    fn rejects_a_file_without_frontmatter() {
        assert!(parse("# just a heading\n", "a").is_err());
    }

    #[test]
    fn accepts_optional_fields_without_complaint() {
        let text =
            "---\nname: a\ndescription: x\nlicense: MIT\nallowed-tools: read bash\n---\nbody\n";
        let (name, _, _) = parse(text, "a").expect("optional fields should be allowed");
        assert_eq!(name, "a");
    }

    #[test]
    fn parses_a_literal_block_scalar_description() {
        let text = "---\nname: a\ndescription: |\n  line one\n  line two\n---\nbody\n";
        let (_, desc, _) = parse(text, "a").expect("a | block scalar should be readable");
        assert_eq!(desc, "line one\nline two");
    }

    #[test]
    fn parses_a_folded_block_scalar_description() {
        let text = "---\nname: a\ndescription: >\n  folded\n  text\n---\nbody\n";
        let (_, desc, _) = parse(text, "a").expect("a > block scalar should be readable");
        assert_eq!(desc, "folded text");
    }

    #[test]
    fn parses_a_plain_multiline_continuation() {
        let text = "---\nname: a\ndescription: this continues\n  on the next line\n---\nbody\n";
        let (_, desc, _) =
            parse(text, "a").expect("a plain multi-line continuation should be readable");
        assert_eq!(desc, "this continues on the next line");
    }

    #[test]
    fn refuses_a_construct_it_does_not_support_instead_of_guessing() {
        // `|-` is a block scalar with a chomping indicator and is out of
        // scope. Always signal it with an error rather than silently let it
        // through.
        let text = "---\nname: a\ndescription: |-\n  text\n---\nbody\n";
        let err = parse(text, "a")
            .expect_err("unsupported syntax should be rejected, not silently accepted");
        assert!(matches!(err, SkillError::UnsupportedSyntax { .. }));
    }

    #[test]
    fn frontmatter_errors_name_the_failing_skill() {
        let no_fm = parse("no frontmatter here\n", "my-skill").expect_err("should be rejected");
        assert!(matches!(no_fm, SkillError::NoFrontmatter { ref skill } if skill == "my-skill"));
        assert!(no_fm.to_string().contains("my-skill"));

        let missing = parse("---\ndescription: x\n---\nbody\n", "my-skill")
            .expect_err("should be rejected when name is missing");
        assert!(matches!(
            missing,
            SkillError::MissingField { ref skill, field } if skill == "my-skill" && field == "name"
        ));
        assert!(missing.to_string().contains("my-skill"));

        let bad_len = parse(
            "---\nname: my-skill\ndescription: \"\"\n---\nbody\n",
            "my-skill",
        )
        .expect_err("an empty description should be rejected");
        assert!(matches!(
            bad_len,
            SkillError::InvalidDescription { ref skill, .. } if skill == "my-skill"
        ));
        assert!(bad_len.to_string().contains("my-skill"));
    }

    #[test]
    fn empty_frontmatter_reports_missing_field_not_no_frontmatter() {
        let err = parse("---\n---\nbody\n", "a").expect_err("empty frontmatter should be rejected");
        assert!(matches!(err, SkillError::MissingField { .. }));
    }

    #[test]
    fn a_wide_closing_delimiter_does_not_leak_a_hyphen_into_the_body() {
        let text = "---\nname: a\ndescription: x\n----\nbody\n";
        let (_, _, body) =
            parse(text, "a").expect("a wide closing delimiter should also be readable");
        assert_eq!(body.trim(), "body");
    }
}
