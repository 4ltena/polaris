//! Typed host input, deliberately without executable, command or URL fields.
use crate::Error;
pub use crate::catalog::ProfileId as ProfileName;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Set,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub operation: Operation,
    pub profile: ProfileName,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

impl Request {
    pub fn validate(&self) -> Result<(), Error> {
        if self.target.as_ref().is_some_and(|path| {
            path.is_empty() || path.len() > 4096 || path.chars().any(char::is_control)
        }) {
            return Err(Error::Unsupported);
        }
        self.profile.entry().parser.parse(self.version.as_deref())?;
        Ok(())
    }

    pub fn normalized_version(&self) -> Result<crate::version::Constraint, Error> {
        self.validate()?;
        self.profile.entry().parser.parse(self.version.as_deref())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum DirectCommand {
    Set(Request),
    Plan(Request),
    Status,
    InspectOperation(String),
    Recover(String),
}

/// Literal command grammar, not a shell. No expansion, substitution, pipeline
/// or inferred natural-language instruction is accepted.
pub fn direct(input: &str) -> Result<DirectCommand, Error> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    if input.len() > 8192 || input.chars().any(char::is_control) {
        return Err(Error::Unsupported);
    }
    for character in input.chars() {
        if escaped {
            word.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None
            } else {
                word.push(character)
            }
        } else if character == '\'' || character == '"' {
            quote = Some(character)
        } else if character.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(character)
        }
    }
    if escaped || quote.is_some() {
        return Err(Error::Unsupported);
    }
    if !word.is_empty() {
        words.push(word);
    }
    if words.as_slice() == ["status"] {
        return Ok(DirectCommand::Status);
    }
    if words.len() == 3 && words[1] == "--operation" {
        return match words[0].as_str() {
            "status" => Ok(DirectCommand::InspectOperation(words[2].clone())),
            "recover" => Ok(DirectCommand::Recover(words[2].clone())),
            _ => Err(Error::Unsupported),
        };
    }
    if words.len() < 2 || !matches!(words[0].as_str(), "set" | "plan") {
        return Err(Error::Unsupported);
    }
    let profile = serde_json::from_value(serde_json::Value::String(words[1].clone()))
        .map_err(|_| Error::Unsupported)?;
    let mut request = Request {
        operation: Operation::Set,
        profile,
        target: None,
        version: None,
    };
    let mut tail = words[2..].chunks_exact(2);
    for option in &mut tail {
        match option[0].as_str() {
            "--path" if request.target.is_none() => request.target = Some(option[1].clone()),
            "--version" if request.version.is_none() => request.version = Some(option[1].clone()),
            _ => return Err(Error::Unsupported),
        }
    }
    if !tail.remainder().is_empty() {
        return Err(Error::Unsupported);
    }
    request.validate()?;
    Ok(if words[0] == "set" {
        DirectCommand::Set(request)
    } else {
        DirectCommand::Plan(request)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_direct_commands_preserve_paths_and_refuse_inferred_shell_steps() {
        let DirectCommand::Set(request) =
            direct("set python --version 3.12 --path 'project folder/日本語'").unwrap()
        else {
            panic!("expected set")
        };
        assert_eq!(request.target.as_deref(), Some("project folder/日本語"));
        assert_eq!(request.version.as_deref(), Some("3.12"));
        assert_eq!(direct("status"), Ok(DirectCommand::Status));
        for input in [
            "set node && echo unsafe",
            "set nextjs please",
            "set node --path",
            "set node --path 'unterminated",
            "set node --version 24 --version 22",
            "set custom",
            "set node\n--path elsewhere",
        ] {
            assert!(direct(input).is_err(), "{input}");
        }
    }

    #[test]
    fn tool_cannot_supply_shell_or_artifact_inputs() {
        let valid = r#"{"operation":"set","profile":"nextjs"}"#;
        let request: Request = serde_json::from_str(valid).unwrap();
        assert_eq!(request.validate(), Ok(()));
        for invalid in [
            r#"{"operation":"set","profile":"nextjs","command":"npm install"}"#,
            r#"{"operation":"set","profile":"nextjs","url":"https://example.test"}"#,
            r#"{"operation":"exec","profile":"nextjs"}"#,
            r#"{"operation":"set","profile":"custom"}"#,
        ] {
            assert!(serde_json::from_str::<Request>(invalid).is_err());
        }
    }

    #[test]
    fn version_validation_uses_the_selected_profile_parser() {
        let python: Request = serde_json::from_str(
            r#"{"operation":"set","profile":"python","version":">=3.12,<3.13"}"#,
        )
        .unwrap();
        assert!(python.normalized_version().is_ok());
        let rust: Request =
            serde_json::from_str(r#"{"operation":"set","profile":"rust","version":"stable"}"#)
                .unwrap();
        assert!(rust.normalized_version().is_ok());
        assert!(
            serde_json::from_str::<Request>(
                r#"{"operation":"set","profile":"nextjs","next_version":"16"}"#
            )
            .is_err()
        );
    }
}
