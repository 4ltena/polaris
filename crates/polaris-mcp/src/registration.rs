//! Validated host registrations, without SDK types or secret values.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Error;

pub const MAX_SERVERS: usize = 32;
pub const MAX_STORE_BYTES: usize = 1024 * 1024;
pub const MAX_JSON_BYTES: usize = 256 * 1024;
pub const MAX_JSON_DEPTH: usize = 64;
pub const MAX_JSON_NODES: usize = 16_384;

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub schema_version: u32,
    pub revision: u64,
    pub servers: Vec<Server>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub server_id: String,
    /// Opaque host-validated project identities, never workspace paths.
    pub enabled_projects: BTreeSet<String>,
    pub transport: Destination,
    #[serde(default)]
    pub protocol_profile: ProtocolProfile,
    pub capabilities: Capabilities,
    #[serde(default)]
    pub timeouts: Timeouts,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Destination {
    Stdio {
        command: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        sandbox_profile: String,
        secret_env: BTreeMap<String, String>,
        /// Host-derived install trees for a command outside the runtime roots.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        readable_roots: Vec<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        executable_sha256: Option<String>,
        /// A script run by the pinned interpreter; it must also appear in args.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        script: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        script_sha256: Option<String>,
    },
    StreamableHttp {
        url: String,
        secret_headers: BTreeMap<String, String>,
    },
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolProfile {
    #[default]
    #[serde(rename = "discover_2026")]
    Discover2026,
    #[serde(rename = "auto_2025_compat")]
    Auto2025Compat,
    /// stdio only: an app server that still speaks 2024-11-05.
    #[serde(rename = "auto_2024_compat")]
    Auto2024Compat,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub catalog_metadata: bool,
    pub resource_read: bool,
    pub prompt_selection: bool,
    pub tool_call: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Timeouts {
    pub catalog_ttl_seconds: u32,
    pub connect_seconds: u32,
    pub catalog_seconds: u32,
    pub operation_seconds: u32,
    pub connect_retries: u8,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            catalog_ttl_seconds: 300,
            connect_seconds: 10,
            catalog_seconds: 30,
            operation_seconds: 120,
            connect_retries: 2,
        }
    }
}

pub(crate) fn valid_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn normalized_path(path: &Path) -> bool {
    path.is_absolute()
        && path.as_os_str().len() <= 16_384
        && !path.as_os_str().as_encoded_bytes().contains(&0)
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str()
}

impl Registry {
    pub fn validate(&self) -> Result<(), Error> {
        if self.schema_version != 1 || self.revision == 0 || self.servers.len() > MAX_SERVERS {
            return Err(Error::InvalidRegistration);
        }
        let mut ids = BTreeSet::new();
        for server in &self.servers {
            if !ids.insert(&server.server_id) {
                return Err(Error::InvalidRegistration);
            }
            server.validate()?;
        }
        bounded_json(self, MAX_STORE_BYTES)?;
        Ok(())
    }
}

impl Server {
    pub fn validate(&self) -> Result<(), Error> {
        let t = self.timeouts;
        if !valid_id(&self.server_id)
            || self
                .enabled_projects
                .iter()
                .any(|p| p.is_empty() || p.len() > 128 || p.contains('\0'))
            || !(30..=3600).contains(&t.catalog_ttl_seconds)
            || !(1..=60).contains(&t.connect_seconds)
            || !(5..=120).contains(&t.catalog_seconds)
            || !(1..=900).contains(&t.operation_seconds)
            || t.connect_retries > 5
        {
            return Err(Error::InvalidRegistration);
        }
        match &self.transport {
            Destination::Stdio {
                command,
                args,
                cwd,
                sandbox_profile,
                secret_env,
                readable_roots,
                executable_sha256,
                script,
                script_sha256,
            } => {
                let digest_invalid = |digest: &Option<String>| {
                    digest.as_ref().is_some_and(|digest| {
                        digest.len() != 64
                            || !digest
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    })
                };
                if !normalized_path(command)
                    || !normalized_path(cwd)
                    || readable_roots.len() > 8
                    || !readable_roots.iter().all(|root| normalized_path(root))
                    || digest_invalid(executable_sha256)
                    || digest_invalid(script_sha256)
                    || (!readable_roots.is_empty() && executable_sha256.is_none())
                    || !readable_roots.iter().all(|root| {
                        command.starts_with(root)
                            || script
                                .as_ref()
                                .is_some_and(|script| script.starts_with(root))
                    })
                    || script.is_some() != script_sha256.is_some()
                    || script.as_ref().is_some_and(|script| {
                        !normalized_path(script)
                            || !args.iter().any(|arg| Path::new(arg) == script.as_path())
                    })
                    || !matches!(
                        sandbox_profile.as_str(),
                        "read-only" | "loopback" | "workspace-write" | "full-access"
                    )
                    || args.iter().any(|a| a.as_bytes().contains(&0))
                    || secret_env.iter().any(|(name, reference)| {
                        !valid_id(reference)
                            || name.is_empty()
                            || !name.bytes().enumerate().all(|(i, b)| {
                                b == b'_'
                                    || b.is_ascii_alphabetic()
                                    || (i > 0 && b.is_ascii_digit())
                            })
                    })
                {
                    return Err(Error::InvalidRegistration);
                }
            }
            Destination::StreamableHttp {
                url,
                secret_headers,
            } => {
                if normalize_http_url(url)? != *url
                    || self.protocol_profile == ProtocolProfile::Auto2024Compat
                {
                    return Err(Error::InvalidRegistration);
                }
                // Lowercase names prevent duplicate logical headers. Framing and
                // protocol negotiation headers are controlled by the transport.
                for (name, reference) in secret_headers {
                    if !valid_id(reference)
                        || name.is_empty()
                        || !name
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                        || matches!(
                            name.as_str(),
                            "host"
                                | "content-length"
                                | "transfer-encoding"
                                | "connection"
                                | "content-type"
                                | "accept"
                                | "mcp-protocol-version"
                                | "mcp-session-id"
                        )
                    {
                        return Err(Error::InvalidRegistration);
                    }
                }
            }
        }
        bounded_json(self, MAX_JSON_BYTES)?;
        Ok(())
    }

    /// Bind all destination, protocol and sandbox fields without leaking them.
    pub fn destination_hash(&self) -> Result<String, Error> {
        self.validate()?;
        canonical_hash(
            &serde_json::json!({"transport": self.transport, "protocol": self.protocol_profile}),
        )
    }
}

/// Registration URLs have no query: credentials cannot hide in an opaque query.
pub fn normalize_http_url(input: &str) -> Result<String, Error> {
    let parsed = url::Url::parse(input).map_err(|_| Error::InvalidRegistration)?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || parsed.query().is_some()
        || parsed.host().is_none()
    {
        return Err(Error::InvalidRegistration);
    }
    let loopback = match parsed.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
        return Err(Error::InvalidRegistration);
    }
    Ok(parsed.to_string())
}

/// Bound traversal before sorting keys or serializing untrusted JSON.
pub fn canonical_hash(value: &serde_json::Value) -> Result<String, Error> {
    let mut pending = vec![(value, 0)];
    let mut count = 0;
    while let Some((value, depth)) = pending.pop() {
        count += 1;
        if depth > MAX_JSON_DEPTH || count > MAX_JSON_NODES {
            return Err(Error::Limit);
        }
        match value {
            serde_json::Value::Object(map) => {
                if map.len() > MAX_JSON_NODES - count {
                    return Err(Error::Limit);
                }
                pending.extend(map.values().map(|v| (v, depth + 1)));
            }
            serde_json::Value::Array(values) => {
                if values.len() > MAX_JSON_NODES - count {
                    return Err(Error::Limit);
                }
                pending.extend(values.iter().map(|v| (v, depth + 1)));
            }
            _ => {}
        }
    }
    // Check encoded bytes before cloning/sorting potentially huge string values.
    bounded_json(value, MAX_JSON_BYTES)?;
    fn canonical(
        value: &serde_json::Value,
        depth: usize,
        nodes: &mut usize,
    ) -> Result<serde_json::Value, Error> {
        *nodes += 1;
        if depth > MAX_JSON_DEPTH || *nodes > MAX_JSON_NODES {
            return Err(Error::Limit);
        }
        Ok(match value {
            serde_json::Value::Object(map) => {
                let mut sorted = serde_json::Map::new();
                for (key, value) in map.iter().collect::<BTreeMap<_, _>>() {
                    sorted.insert(key.clone(), canonical(value, depth + 1, nodes)?);
                }
                serde_json::Value::Object(sorted)
            }
            serde_json::Value::Array(values) => serde_json::Value::Array(
                values
                    .iter()
                    .map(|v| canonical(v, depth + 1, nodes))
                    .collect::<Result<_, _>>()?,
            ),
            other => other.clone(),
        })
    }
    let bytes = bounded_json(&canonical(value, 0, &mut 0)?, MAX_JSON_BYTES)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// Serialize into a bounded buffer. Callers must bound recursive JSON first.
pub fn bounded_json(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, Error> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for Bounded {
        fn write(&mut self, value: &[u8]) -> std::io::Result<usize> {
            if value.len() > self.limit - self.bytes.len() {
                return Err(std::io::ErrorKind::FileTooLarge.into());
            }
            self.bytes.extend_from_slice(value);
            Ok(value.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Bounded {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| Error::Limit)?;
    Ok(writer.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server {
            server_id: "fixture".into(),
            enabled_projects: BTreeSet::from(["host-project-1".into()]),
            transport: Destination::Stdio {
                command: "/usr/bin/fixture".into(),
                args: vec!["--mcp".into()],
                cwd: "/tmp".into(),
                sandbox_profile: "read-only".into(),
                secret_env: BTreeMap::new(),
                readable_roots: vec![],
                executable_sha256: None,
                script: None,
                script_sha256: None,
            },
            protocol_profile: ProtocolProfile::default(),
            capabilities: Capabilities::default(),
            timeouts: Timeouts::default(),
        }
    }

    #[test]
    fn validates_registry_identity_revision_profile_and_timeouts() {
        let s = server();
        let mut registry = Registry {
            schema_version: 1,
            revision: 1,
            servers: vec![s.clone()],
        };
        registry.validate().unwrap();
        let encoded = serde_json::to_value(&registry).unwrap();
        assert_eq!(encoded["servers"][0]["protocol_profile"], "discover_2026");
        assert_eq!(
            serde_json::from_value::<Registry>(encoded).unwrap(),
            registry
        );
        registry.servers.push(s.clone());
        assert_eq!(registry.validate(), Err(Error::InvalidRegistration));
        registry.servers.pop();
        registry.revision = 0;
        assert_eq!(registry.validate(), Err(Error::InvalidRegistration));
        for id in ["", "a/b", "日本語"] {
            let mut invalid = s.clone();
            invalid.server_id = id.into();
            assert_eq!(invalid.validate(), Err(Error::InvalidRegistration));
        }
        let mut changed = s.clone();
        changed.timeouts.operation_seconds = 901;
        assert_eq!(changed.validate(), Err(Error::InvalidRegistration));
        changed.timeouts.operation_seconds = 900;
        changed.validate().unwrap();
        changed.protocol_profile = ProtocolProfile::Auto2025Compat;
        assert_ne!(
            s.destination_hash().unwrap(),
            changed.destination_hash().unwrap()
        );
        assert_eq!(
            serde_json::to_value(changed.protocol_profile).unwrap(),
            "auto_2025_compat"
        );
        changed.protocol_profile = ProtocolProfile::Auto2024Compat;
        changed.validate().unwrap();
        let http = Server {
            transport: Destination::StreamableHttp {
                url: "https://example.com/mcp".into(),
                secret_headers: BTreeMap::new(),
            },
            ..changed.clone()
        };
        assert_eq!(http.validate(), Err(Error::InvalidRegistration));
    }

    #[test]
    fn rejects_relative_paths_nuls_and_transport_control_headers() {
        for path in ["/tmp/./fixture", "/tmp//fixture", "/tmp/fixture/"] {
            assert!(!normalized_path(Path::new(path)), "{path}");
        }
        assert!(normalized_path(Path::new("/tmp/fixture")));
        let mut s = server();
        if let Destination::Stdio { command, .. } = &mut s.transport {
            *command = "fixture".into();
        }
        assert_eq!(s.validate(), Err(Error::InvalidRegistration));
        s = server();
        if let Destination::Stdio { args, .. } = &mut s.transport {
            args.push("a\0b".into());
        }
        assert_eq!(s.validate(), Err(Error::InvalidRegistration));
        s.transport = Destination::StreamableHttp {
            url: "https://example.com/mcp".into(),
            secret_headers: BTreeMap::from([("authorization".into(), "fixture-token".into())]),
        };
        s.validate().unwrap();
        for header in [
            "Authorization",
            "host",
            "content-length",
            "mcp-session-id",
            "x\r\ny",
        ] {
            if let Destination::StreamableHttp { secret_headers, .. } = &mut s.transport {
                *secret_headers = BTreeMap::from([(header.into(), "fixture-token".into())]);
            }
            assert_eq!(s.validate(), Err(Error::InvalidRegistration));
        }
    }

    #[test]
    fn bounded_json_rejects_depth_nodes_and_encoded_byte_overflow() {
        let mut value = serde_json::Value::Null;
        for _ in 0..MAX_JSON_DEPTH {
            value = serde_json::json!([value]);
        }
        canonical_hash(&value).unwrap();
        assert_eq!(
            canonical_hash(&serde_json::json!([value])),
            Err(Error::Limit)
        );
        assert_eq!(
            canonical_hash(&serde_json::json!(vec![0; MAX_JSON_NODES])),
            Err(Error::Limit)
        );
        assert_eq!(bounded_json(&"\n".repeat(100), 201), Err(Error::Limit));
        assert_eq!(bounded_json(&"abc", 5).unwrap(), br#""abc""#);
    }
    #[test]
    fn http_rejects_credential_and_non_loopback_cleartext_destinations() {
        for value in [
            "http://localhost/mcp",
            "http://192.0.2.1/mcp",
            "https://a:b@example.com/mcp",
            "https://example.com/mcp?token=x",
            "https://example.com/mcp#x",
            "file:///mcp",
        ] {
            assert_eq!(normalize_http_url(value), Err(Error::InvalidRegistration));
        }
        for value in [
            "https://example.com/mcp",
            "http://127.0.0.1:8000/mcp",
            "http://[::1]:8000/mcp",
        ] {
            assert_eq!(normalize_http_url(value).unwrap(), value);
        }
    }
    #[test]
    fn hashes_ignore_object_order_but_preserve_arrays_and_types() {
        let a = serde_json::json!({"a": {"x": 1, "y": 2}, "b": [1, 2]});
        let b = serde_json::from_str(r#"{"b":[1,2],"a":{"y":2,"x":1}}"#).unwrap();
        assert_eq!(canonical_hash(&a).unwrap(), canonical_hash(&b).unwrap());
        assert_ne!(
            canonical_hash(&a).unwrap(),
            canonical_hash(&serde_json::json!({"a": {"x": 1, "y": 2}, "b": [2, 1]})).unwrap()
        );
        assert_eq!(
            canonical_hash(&serde_json::json!("x".repeat(MAX_JSON_BYTES))),
            Err(Error::Limit)
        );
    }
}
