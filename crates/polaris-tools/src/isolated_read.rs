//! OS隔離されたhelper内でのみ本文を読む。親側の直読fallbackは用意しない。
use crate::{ToolError, path_policy};
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

pub const OUTPUT_BUDGET: usize = 32 * 1024;
pub const WIRE_LIMIT: usize = 256 * 1024;
pub const REQUEST_LIMIT: usize = 16 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Request {
    Lines {
        path: PathBuf,
        offset: usize,
        limit: usize,
        budget: usize,
    },
    DiffBefore {
        path: PathBuf,
    },
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Reply {
    Text(String),
    Missing,
    Unavailable,
}

pub fn output_budget() -> Result<usize, String> {
    match std::env::var("POLARIS_READ_OUTPUT_BYTES") {
        Ok(value) => value
            .parse::<usize>()
            .ok()
            .filter(|n| (1024..=crate::read::MAX_READ_OUTPUT_BYTES).contains(n))
            .map(|n| n.min(OUTPUT_BUDGET))
            .ok_or_else(|| "invalid read output budget".into()),
        Err(std::env::VarError::NotPresent) => Ok(OUTPUT_BUDGET),
        Err(_) => Err("invalid read output budget".into()),
    }
}

/// open前判定は最適化であり権限境界ではない。OS policyが全openを制限する。
pub(crate) fn bounded_text(path: &Path, cap: u64) -> Result<String, ToolError> {
    if path_policy::is_denied(path) {
        return Err(ToolError::PathDenied(path.display().to_string()));
    }
    if let Ok(real) = std::fs::canonicalize(path)
        && path_policy::is_denied(&real)
    {
        return Err(ToolError::PathDenied(path.display().to_string()));
    }
    let file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(ToolError::NotAFile(path.display().to_string()));
    }
    if meta.len() > cap {
        return Err(ToolError::TooLarge {
            path: path.display().to_string(),
            limit: cap,
            actual: meta.len(),
        });
    }
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(ToolError::TooLarge {
            path: path.display().to_string(),
            limit: cap,
            actual: bytes.len() as u64,
        });
    }
    String::from_utf8(bytes).map_err(|_| {
        ToolError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not UTF-8",
        ))
    })
}

/// stdinも上限付き。ファイルパス以外の親由来本文は受け取らない。
pub fn serve(input: impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    let reply = if input
        .take(REQUEST_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > REQUEST_LIMIT
    {
        Reply::Unavailable
    } else {
        match serde_json::from_slice::<Request>(&bytes) {
            Ok(request) => apply(request),
            Err(_) => Reply::Unavailable,
        }
    };
    let wire = serde_json::to_vec(&reply).expect("reply serialization");
    if wire.len() > WIRE_LIMIT {
        serde_json::to_vec(&Reply::Unavailable).expect("constant reply")
    } else {
        wire
    }
}
fn apply(request: Request) -> Reply {
    match request {
        Request::Lines {
            path,
            offset,
            limit,
            budget,
        } => {
            if !(1024..=OUTPUT_BUDGET).contains(&budget) {
                return Reply::Unavailable;
            }
            match crate::read::read_isolated(&path, offset, limit, budget) {
                Ok(text) if text.len() <= OUTPUT_BUDGET + 1024 => Reply::Text(text),
                _ => Reply::Unavailable,
            }
        }
        Request::DiffBefore { path } => match bounded_text(&path, OUTPUT_BUDGET as u64) {
            Ok(text) => Reply::Text(text),
            Err(ToolError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Reply::Missing
            }
            _ => Reply::Unavailable,
        },
    }
}

pub fn decode(wire: &str, request: &Request) -> Result<Reply, String> {
    if wire.len() > WIRE_LIMIT {
        return Err("isolated read response too large".into());
    }
    let reply: Reply = serde_json::from_str(wire).map_err(|_| "invalid isolated read response")?;
    let cap = match request {
        Request::Lines { .. } => OUTPUT_BUDGET + 1024,
        Request::DiffBefore { .. } => OUTPUT_BUDGET,
    };
    match &reply {
        Reply::Text(text) if text.len() > cap => Err("isolated read response too large".into()),
        Reply::Missing if matches!(request, Request::Lines { .. }) => {
            Err("invalid isolated read response".into())
        }
        _ => Ok(reply),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(path: &Path, offset: usize, limit: usize, budget: usize) -> Request {
        Request::Lines {
            path: path.into(),
            offset,
            limit,
            budget,
        }
    }
    fn roundtrip(request: &Request) -> Reply {
        let wire = serve(serde_json::to_vec(request).unwrap().as_slice());
        assert!(wire.len() <= WIRE_LIMIT);
        decode(std::str::from_utf8(&wire).unwrap(), request).unwrap()
    }
    #[test]
    fn isolated_lines_keep_offsets_and_notices() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("source.txt");
        std::fs::write(&p, "one\ntwo\nthree\n").unwrap();
        let Reply::Text(text) = roundtrip(&request(&p, 1, 1, OUTPUT_BUDGET)) else {
            panic!()
        };
        assert!(text.starts_with("2\ttwo\n"));
        assert!(text.contains("offset=2"));
        assert_eq!(
            roundtrip(&request(&p, 0, 0, OUTPUT_BUDGET)),
            Reply::Text("(0 lines: limit is 0)".into())
        );
    }
    #[test]
    fn isolated_long_line_and_json_escape_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("source.txt");
        std::fs::write(&p, "\u{1}".repeat(100_000)).unwrap();
        let Reply::Text(text) = roundtrip(&request(&p, 0, 1, OUTPUT_BUDGET)) else {
            panic!()
        };
        assert!(text.contains("truncated within the line"));
        let Reply::Text(small) = roundtrip(&request(&p, 0, 1, 1024)) else {
            panic!()
        };
        assert!(small.len() < 2048);
    }
    #[test]
    fn isolated_diff_does_not_treat_denial_or_overflow_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("source.txt");
        let req = Request::DiffBefore { path: p.clone() };
        assert_eq!(roundtrip(&req), Reply::Missing);
        std::fs::write(&p, "x".repeat(OUTPUT_BUDGET + 1)).unwrap();
        assert_eq!(roundtrip(&req), Reply::Unavailable);
        std::fs::write(&p, "dummy").unwrap();
        assert_eq!(roundtrip(&req), Reply::Text("dummy".into()));
        let denied = dir.path().join(".env");
        std::fs::write(&denied, "DUMMY_ONLY").unwrap();
        assert_eq!(
            roundtrip(&Request::DiffBefore { path: denied }),
            Reply::Unavailable
        );
    }
    #[test]
    fn isolated_rejects_invalid_input_and_oversized_wire() {
        assert_eq!(
            serve(vec![b'x'; REQUEST_LIMIT + 1].as_slice()),
            b"\"Unavailable\""
        );
        let req = request(Path::new("unused"), 0, 1, OUTPUT_BUDGET);
        assert!(decode(&" ".repeat(WIRE_LIMIT + 1), &req).is_err());
        assert_eq!(
            roundtrip(&request(Path::new("unused"), 0, 1, OUTPUT_BUDGET + 1)),
            Reply::Unavailable
        );
    }
}
