//! Profile-specific, bounded release selectors. Parsing has no I/O: channel
//! resolution chooses only from the already pinned profile candidate order.
use crate::Error;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Release {
    pub fn exact(value: &str) -> Result<Self, Error> {
        let parts = numbers(value)?;
        if parts.len() != 3 {
            return Err(Error::Unsupported);
        }
        Ok(Self {
            major: parts[0],
            minor: parts[1],
            patch: parts[2],
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Parser {
    JsRelease,
    PythonRelease,
    RustToolchain,
    GoRelease,
}

impl Parser {
    pub fn id(self) -> &'static str {
        match self {
            Self::JsRelease => "js-release/1",
            Self::PythonRelease => "python-release/1",
            Self::RustToolchain => "rust-toolchain/1",
            Self::GoRelease => "go-release/1",
        }
    }

    pub fn parse(self, input: Option<&str>) -> Result<Constraint, Error> {
        let Some(input) = input else {
            return Ok(Constraint {
                parser: self,
                selector: Selector::Default,
            });
        };
        if input.is_empty()
            || input.len() > 128
            || input.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(Error::Unsupported);
        }
        let selector = match self {
            Self::JsRelease if input.starts_with('^') || input.starts_with('~') => {
                let lower = Release::exact(&input[1..])?;
                let upper = if input.starts_with('~') {
                    Release {
                        minor: next(lower.minor)?,
                        patch: 0,
                        ..lower
                    }
                } else if lower.major != 0 {
                    Release {
                        major: next(lower.major)?,
                        minor: 0,
                        patch: 0,
                    }
                } else if lower.minor != 0 {
                    Release {
                        major: 0,
                        minor: next(lower.minor)?,
                        patch: 0,
                    }
                } else {
                    Release {
                        patch: next(lower.patch)?,
                        ..lower
                    }
                };
                Selector::Range { lower, upper }
            }
            Self::JsRelease | Self::PythonRelease if input.starts_with(">=") => {
                let (lower, upper) = input.split_once(",<").ok_or(Error::Unsupported)?;
                let lower = endpoint(&lower[2..], self == Self::PythonRelease)?;
                let upper = endpoint(upper, self == Self::PythonRelease)?;
                if lower >= upper {
                    return Err(Error::Unsupported);
                }
                Selector::Range { lower, upper }
            }
            Self::RustToolchain if input == "stable" => Selector::Default,
            Self::RustToolchain => Selector::Exact(Release::exact(input)?),
            _ => {
                let input = if self == Self::GoRelease {
                    input.strip_prefix("go").unwrap_or(input)
                } else {
                    input
                };
                let parts = numbers(input)?;
                match parts.as_slice() {
                    [major] if self == Self::JsRelease => Selector::Major(*major),
                    [major, minor] => Selector::Minor {
                        major: *major,
                        minor: *minor,
                    },
                    [major, minor, patch] => Selector::Exact(Release {
                        major: *major,
                        minor: *minor,
                        patch: *patch,
                    }),
                    _ => return Err(Error::Unsupported),
                }
            }
        };
        Ok(Constraint {
            parser: self,
            selector,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum Selector {
    Default,
    Exact(Release),
    Major(u32),
    Minor { major: u32, minor: u32 },
    Range { lower: Release, upper: Release },
}

/// The normalized form, including its parser identity, is part of consent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Constraint {
    parser: Parser,
    selector: Selector,
}

impl Constraint {
    pub fn parser(&self) -> Parser {
        self.parser
    }
    pub fn is_exact(&self) -> bool {
        matches!(self.selector, Selector::Exact(_))
    }
    pub fn matches(&self, release: Release) -> bool {
        match self.selector {
            Selector::Default => true,
            Selector::Exact(expected) => release == expected,
            Selector::Major(major) => release.major == major,
            Selector::Minor { major, minor } => (release.major, release.minor) == (major, minor),
            Selector::Range { lower, upper } => release >= lower && release < upper,
        }
    }

    pub fn primary_fallback_allowed(
        &self,
        before: Release,
        after: Release,
        new_project: bool,
    ) -> bool {
        if !new_project || self.is_exact() || !self.matches(after) {
            return false;
        }
        match self.parser {
            Parser::JsRelease => before.major == after.major,
            Parser::PythonRelease => (before.major, before.minor) == (after.major, after.minor),
            Parser::RustToolchain | Parser::GoRelease => false,
        }
    }

    /// Reject forged normalized forms when loading saved plans.
    pub fn validate(&self) -> Result<(), Error> {
        match (&self.parser, &self.selector) {
            (_, Selector::Default | Selector::Exact(_)) => Ok(()),
            (Parser::JsRelease, Selector::Major(_)) => Ok(()),
            (
                Parser::JsRelease | Parser::PythonRelease | Parser::GoRelease,
                Selector::Minor { .. },
            ) => Ok(()),
            (Parser::JsRelease | Parser::PythonRelease, Selector::Range { lower, upper })
                if lower < upper =>
            {
                Ok(())
            }
            _ => Err(Error::Unsupported),
        }
    }
}

fn next(value: u32) -> Result<u32, Error> {
    value.checked_add(1).ok_or(Error::Unsupported)
}

fn numbers(value: &str) -> Result<Vec<u32>, Error> {
    if value.is_empty() || value.len() > 32 {
        return Err(Error::Unsupported);
    }
    let parts = value
        .split('.')
        .map(|part| {
            if part.is_empty()
                || part.len() > 10
                || (part.len() > 1 && part.starts_with('0'))
                || !part.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(Error::Unsupported);
            }
            part.parse::<u32>().map_err(|_| Error::Unsupported)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parts.is_empty() || parts.len() > 3 {
        return Err(Error::Unsupported);
    }
    Ok(parts)
}

fn endpoint(value: &str, allow_minor: bool) -> Result<Release, Error> {
    let parts = numbers(value)?;
    match parts.as_slice() {
        [major, minor] if allow_minor => Ok(Release {
            major: *major,
            minor: *minor,
            patch: 0,
        }),
        [major, minor, patch] => Ok(Release {
            major: *major,
            minor: *minor,
            patch: *patch,
        }),
        _ => Err(Error::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn release(value: &str) -> Release {
        Release::exact(value).unwrap()
    }

    #[test]
    fn minor_and_exact_are_not_caret_aliases() {
        for parser in [Parser::JsRelease, Parser::PythonRelease, Parser::GoRelease] {
            let minor = parser.parse(Some("3.12")).unwrap();
            assert!(minor.matches(release("3.12.99")));
            assert!(!minor.matches(release("3.13.0")));
            let exact = parser.parse(Some("3.12.1")).unwrap();
            assert!(exact.is_exact());
            assert!(!exact.matches(release("3.12.2")));
        }
    }

    #[test]
    fn python_range_is_bounded_and_unsupported_pep440_is_rejected() {
        let range = Parser::PythonRelease.parse(Some(">=3.12,<3.13")).unwrap();
        assert!(range.matches(release("3.12.4")));
        assert!(!range.matches(release("3.13.0")));
        for input in [
            ">=3.12,<3.12",
            "3.12rc1",
            "~=3.12",
            "1!3.12",
            "3.12.1+local",
            "3.12.1.post1",
        ] {
            assert!(Parser::PythonRelease.parse(Some(input)).is_err());
        }
    }

    #[test]
    fn rust_channel_and_go_prefix_normalize_without_expanding_support() {
        assert_eq!(
            Parser::RustToolchain.parse(None),
            Parser::RustToolchain.parse(Some("stable"))
        );
        assert!(
            Parser::RustToolchain
                .parse(Some("nightly-2026-10-25"))
                .is_err()
        );
        assert_eq!(
            Parser::GoRelease.parse(Some("go1.25.1")),
            Parser::GoRelease.parse(Some("1.25.1"))
        );
        assert!(Parser::GoRelease.parse(Some("^1.25.1")).is_err());
    }

    #[test]
    fn javascript_caret_zero_rules_and_fallback_line_are_preserved() {
        let range = Parser::JsRelease.parse(Some("^0.2.3")).unwrap();
        assert!(range.matches(release("0.2.9")));
        assert!(!range.matches(release("0.3.0")));
        assert!(!range.primary_fallback_allowed(release("0.2.3"), release("0.2.4"), false));
        assert!(
            !Parser::RustToolchain
                .parse(None)
                .unwrap()
                .primary_fallback_allowed(release("1.90.0"), release("1.91.0"), true)
        );
    }
}
