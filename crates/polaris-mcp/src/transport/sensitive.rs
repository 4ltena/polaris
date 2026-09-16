//! Sensitive MCP value handling at the transport boundary.

use crate::Error;
use serde_json::Value;

/// Only credentials explicitly injected into this connection, never ambient
/// provider credentials. Reject reflection before an SDK can log the message.
#[derive(Clone, Default)]
pub(super) struct SensitiveValues(Vec<String>);
impl SensitiveValues {
    pub(super) fn new(values: impl IntoIterator<Item = String>) -> Self {
        let mut result = Vec::new();
        for value in values {
            if let Some(token) = value.strip_prefix("Bearer ").filter(|v| !v.is_empty()) {
                result.push(token.to_owned());
            }
            if !value.is_empty() {
                result.push(value);
            }
        }
        Self(result)
    }
    pub(super) fn text(&self, text: &str) -> Result<(), Error> {
        if self.0.iter().any(|secret| text.contains(secret)) {
            Err(Error::Auth)
        } else {
            Ok(())
        }
    }
    pub(super) fn bytes(&self, bytes: &[u8]) -> Result<(), Error> {
        if self.0.iter().any(|secret| {
            bytes
                .windows(secret.len())
                .any(|value| value == secret.as_bytes())
        }) {
            Err(Error::Auth)
        } else {
            Ok(())
        }
    }
    pub(super) fn json_bytes(&self, bytes: &[u8]) -> Result<(), Error> {
        if self.0.is_empty() {
            return Ok(());
        }
        let value: Value = serde_json::from_slice(bytes).map_err(|_| Error::Protocol)?;
        let mut pending = vec![&value];
        while let Some(value) = pending.pop() {
            match value {
                Value::String(value) => self.text(value)?,
                Value::Object(values) => {
                    for (name, value) in values {
                        self.text(name)?;
                        pending.push(value);
                    }
                }
                Value::Array(values) => pending.extend(values),
                Value::Number(value) => self.text(&value.to_string())?,
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decoded_reflections_are_rejected_without_returning_the_value() {
        let secrets = SensitiveValues::new(["Bearer fixture-key".into()]);
        assert!(
            secrets
                .json_bytes(br#"{"result":"ordinary response"}"#)
                .is_ok()
        );
        for body in [
            br#"{"result":"fixture-key"}"#.as_slice(),
            br#"{"result":"fixture-\u006bey"}"#,
            br#"{"fixture-key":null}"#,
        ] {
            assert_eq!(secrets.json_bytes(body), Err(Error::Auth));
        }
    }
}
