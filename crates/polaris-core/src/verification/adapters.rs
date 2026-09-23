//! Fixed-shape adapters for registered static checks and Rust test listing.
//!
//! This module never accepts a shell string, cwd, environment, or a model
//! generated command. Process ownership remains with the Session/Desktop host.

use std::{collections::BTreeSet, time::Duration};

use crate::workflow::validate_hash;

use super::{
    AnalysisScopeV1, ScopeCompletenessV1, SourceLocationV1, StaticEvidenceStateV1,
    StaticEvidenceV1, TestCostClassV1, TestEntryV1, TestInventoryV1,
};

pub const STATIC_ADAPTER_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_STATIC_ADAPTER_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_RUST_LISTING_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticAdapterInvocationV1 {
    pub program: std::path::PathBuf,
    pub argv: Vec<String>,
    pub timeout: Duration,
    pub maximum_output_bytes: usize,
}

impl StaticAdapterInvocationV1 {
    pub fn validate(&self) -> Result<(), String> {
        if !self.program.is_absolute()
            || self.argv.is_empty()
            || self.argv.len() > 64
            || self.timeout.is_zero()
            || self.timeout > STATIC_ADAPTER_TIMEOUT
            || self.maximum_output_bytes == 0
            || self.maximum_output_bytes > MAX_STATIC_ADAPTER_OUTPUT_BYTES
            || self
                .argv
                .iter()
                .any(|arg| arg.is_empty() || arg.contains('\0'))
        {
            return Err("static adapter invocation is outside the fixed boundary".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticAdapterOutcomeV1 {
    Completed {
        status: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Failed {
        reason: String,
    },
    TimedOut,
}

impl StaticAdapterOutcomeV1 {
    pub fn bounded(&self) -> bool {
        match self {
            Self::Completed { stdout, stderr, .. } => {
                stdout.len() <= MAX_STATIC_ADAPTER_OUTPUT_BYTES
                    && stderr.len() <= MAX_STATIC_ADAPTER_OUTPUT_BYTES
            }
            Self::Failed { reason } => reason.len() <= 4096,
            Self::TimedOut => true,
        }
    }
}

pub trait StaticAdapterParserV1: Send + Sync {
    fn parse(&self, output: &StaticAdapterOutcomeV1) -> Result<StaticEvidenceV1, String>;
}

pub fn parse_registered_static_evidence(
    invocation: &StaticAdapterInvocationV1,
    outcome: &StaticAdapterOutcomeV1,
    parser: &dyn StaticAdapterParserV1,
) -> Result<StaticEvidenceV1, String> {
    invocation.validate()?;
    if !outcome.bounded()
        || matches!(outcome, StaticAdapterOutcomeV1::Completed { stdout, stderr, .. }
            if stdout.len().checked_add(stderr.len()).is_none_or(|total| total > invocation.maximum_output_bytes))
        || matches!(outcome, StaticAdapterOutcomeV1::Failed { reason }
            if reason.len() > invocation.maximum_output_bytes)
    {
        return Err("static adapter output exceeded the registered bound".into());
    }
    let mut evidence = parser.parse(outcome)?;
    if !matches!(outcome, StaticAdapterOutcomeV1::Completed { .. })
        && evidence.state == StaticEvidenceStateV1::Satisfied
    {
        return Err("failed or timed out static adapter cannot satisfy an obligation".into());
    }
    let analyzer_id = evidence.analyzer_id.clone();
    let rule_id = evidence.rule_id.clone();
    for finding in &mut evidence.findings {
        finding.fingerprint = host_finding_fingerprint(&analyzer_id, &rule_id, finding);
    }
    evidence.validate().map_err(|error| error.to_string())?;
    Ok(evidence)
}

fn host_finding_fingerprint(
    analyzer_id: &str,
    rule_id: &str,
    finding: &super::FindingV1,
) -> String {
    let bytes = serde_json::to_vec(&(
        analyzer_id,
        rule_id,
        &finding.path,
        finding.line,
        &finding.symbol,
        finding.introduced_by_current_diff,
        finding.baseline,
    ))
    .expect("bounded finding identity serializes");
    crate::conversation_state::content_hash(&bytes)
}

pub fn rust_inventory_argv() -> [&'static str; 5] {
    ["cargo", "test", "--workspace", "--", "--list"]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RustInventoryListingV1 {
    Completed { stdout: Vec<u8> },
    Failed,
    TimedOut,
    OutputTooLarge,
}

/// Output from one known Cargo test binary.  The host obtains package and
/// target from its own Cargo metadata/build plan, then supplies only that
/// binary's `--list` output.  Source locations are optional ranking hints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustTestBinaryListingV1 {
    pub package: String,
    /// `lib`, `bin:<name>`, `test:<name>`, `bench:<name>`, or
    /// `example:<name>`.  It is validated before becoming Cargo argv.
    pub target: String,
    pub stdout: Vec<u8>,
    pub source_locations: std::collections::BTreeMap<String, SourceLocationV1>,
}

/// Any malformed/non-zero/truncated listing becomes `None`; callers must use
/// the explicit All-test selection rather than narrowing on partial data.
pub fn collect_complete_rust_inventory(
    listing: &RustInventoryListingV1,
) -> Option<TestInventoryV1> {
    let RustInventoryListingV1::Completed { stdout } = listing else {
        return None;
    };
    if stdout.len() > MAX_RUST_LISTING_BYTES {
        return None;
    }
    let text = std::str::from_utf8(stdout).ok()?;
    let mut entries = Vec::new();
    for line in text.lines() {
        let Some(name) = line.strip_suffix(": test") else {
            if ignored_rust_listing_line(line) {
                continue;
            }
            return None;
        };
        if name.is_empty() || name.contains('\0') {
            return None;
        }
        entries.push(TestEntryV1 {
            id: format!("rust-{}", super::stable_id_hash(&[name.into()])),
            framework: "rust".into(),
            package: "workspace".into(),
            target: "unknown".into(),
            exact_name: name.into(),
            source_location: None,
            dynamic_or_unknown: true,
            cost: TestCostClassV1::Unknown,
            symbols: BTreeSet::new(),
            common_case_ids: BTreeSet::new(),
        });
    }
    (!entries.is_empty())
        .then(|| TestInventoryV1::complete_rust(entries).ok())
        .flatten()
}

/// Collect a complete identity-preserving Rust inventory from host-owned
/// per-binary listings.  Unlike the legacy aggregate collector, entries from
/// this path are not dynamic merely because source locations are unavailable.
/// Any malformed, duplicate, or partial binary listing returns `None` so the
/// owner must execute the explicit `All` path.
pub fn collect_complete_rust_inventory_per_binary(
    listings: &[RustTestBinaryListingV1],
) -> Option<TestInventoryV1> {
    if listings.is_empty() {
        return None;
    }
    let mut entries = Vec::new();
    for listing in listings {
        if listing.package.is_empty()
            || listing.package.starts_with('-')
            || listing.package.contains(['/', '\\', '\0'])
            || super::RustTargetSelectorV1::parse(&listing.target).is_err()
            || listing.stdout.len() > MAX_RUST_LISTING_BYTES
        {
            return None;
        }
        let names = parse_rust_test_listing(&listing.stdout)?;
        if listing
            .source_locations
            .keys()
            .any(|name| !names.iter().any(|listed| listed == name))
        {
            return None;
        }
        for name in names {
            let source_location = listing.source_locations.get(&name).cloned();
            entries.push(TestEntryV1 {
                id: format!(
                    "rust-{}",
                    super::stable_id_hash(&[
                        listing.package.clone(),
                        listing.target.clone(),
                        name.clone(),
                    ])
                ),
                framework: "rust".into(),
                package: listing.package.clone(),
                target: listing.target.clone(),
                exact_name: name,
                source_location,
                dynamic_or_unknown: false,
                cost: TestCostClassV1::Unknown,
                symbols: BTreeSet::new(),
                common_case_ids: BTreeSet::new(),
            });
        }
    }
    (!entries.is_empty())
        .then(|| TestInventoryV1::complete_rust(entries).ok())
        .flatten()
}

fn parse_rust_test_listing(stdout: &[u8]) -> Option<Vec<String>> {
    let text = std::str::from_utf8(stdout).ok()?;
    let mut names = Vec::new();
    for line in text.lines() {
        let Some(name) = line.strip_suffix(": test") else {
            if ignored_rust_listing_line(line) {
                continue;
            }
            return None;
        };
        if name.is_empty() || name.contains('\0') || name.starts_with('-') {
            return None;
        }
        names.push(name.into());
    }
    (!names.is_empty()).then_some(names)
}

fn ignored_rust_listing_line(line: &str) -> bool {
    line.trim().is_empty()
        || line.starts_with("running ")
        || line.starts_with("Doc-tests ")
        || line.contains(" test, ")
        || line.contains(" tests, ")
}

pub fn static_evidence_matches_request(
    evidence: &StaticEvidenceV1,
    source_hash: &str,
    diff_hash: &str,
    config_hash: &str,
    scope: AnalysisScopeV1,
) -> bool {
    validate_hash(source_hash).is_ok()
        && validate_hash(diff_hash).is_ok()
        && validate_hash(config_hash).is_ok()
        && evidence.source_hash == source_hash
        && evidence.diff_hash == diff_hash
        && evidence.config_hash == config_hash
        && evidence.scope == scope
        && evidence.scope_completeness == ScopeCompletenessV1::Complete
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    fn invocation(maximum_output_bytes: usize) -> StaticAdapterInvocationV1 {
        StaticAdapterInvocationV1 {
            program: "/usr/bin/true".into(),
            argv: vec!["/usr/bin/true".into()],
            timeout: Duration::from_secs(1),
            maximum_output_bytes,
        }
    }

    struct SatisfiedParser;
    impl StaticAdapterParserV1 for SatisfiedParser {
        fn parse(&self, _output: &StaticAdapterOutcomeV1) -> Result<StaticEvidenceV1, String> {
            Ok(StaticEvidenceV1 {
                schema_version: super::super::SCHEMA_VERSION,
                evidence_id: "evidence".into(),
                obligation_id: "obligation".into(),
                obligation_kind: "boundary".into(),
                analyzer_id: "analyzer".into(),
                rule_id: "rule".into(),
                tool_hash: hash('t'),
                config_hash: hash('c'),
                source_hash: hash('s'),
                diff_hash: hash('d'),
                scope: AnalysisScopeV1::Workspace,
                scope_completeness: ScopeCompletenessV1::Complete,
                project_graph_basis: None,
                state: StaticEvidenceStateV1::Satisfied,
                findings: Vec::new(),
                error_kind: None,
                duration_ms: Some(1),
                cache_hit: Some(false),
            })
        }
    }

    #[test]
    fn combined_output_must_fit_registered_bound() {
        let output = StaticAdapterOutcomeV1::Completed {
            status: 0,
            stdout: vec![0; 6],
            stderr: vec![0; 6],
        };
        assert!(
            parse_registered_static_evidence(&invocation(10), &output, &SatisfiedParser,).is_err()
        );
    }

    #[test]
    fn timeout_or_failure_cannot_be_parsed_as_satisfied() {
        for output in [
            StaticAdapterOutcomeV1::TimedOut,
            StaticAdapterOutcomeV1::Failed {
                reason: "failed".into(),
            },
        ] {
            assert!(
                parse_registered_static_evidence(&invocation(1024), &output, &SatisfiedParser,)
                    .is_err()
            );
        }
    }

    #[test]
    fn rust_inventory_accepts_standard_summary_without_claiming_static_metadata() {
        let inventory = collect_complete_rust_inventory(&RustInventoryListingV1::Completed {
            stdout: b"suite::one: test\n\n1 test, 0 benchmarks\n".to_vec(),
        })
        .unwrap();
        assert!(inventory.complete);
        assert_eq!(inventory.entries.len(), 1);
        assert!(inventory.entries[0].dynamic_or_unknown);
        assert!(inventory.entries[0].source_location.is_none());
    }

    #[test]
    fn per_binary_listing_keeps_duplicate_test_names_distinct_and_stable() {
        let inventory = collect_complete_rust_inventory_per_binary(&[
            RustTestBinaryListingV1 {
                package: "core".into(),
                target: "lib".into(),
                stdout: b"suite::same: test\n\n1 test, 0 benchmarks\n".to_vec(),
                source_locations: Default::default(),
            },
            RustTestBinaryListingV1 {
                package: "cli".into(),
                target: "test:integration".into(),
                stdout: b"suite::same: test\n\n1 test, 0 benchmarks\n".to_vec(),
                source_locations: Default::default(),
            },
        ])
        .unwrap();
        assert!(inventory.complete);
        assert!(
            inventory
                .entries
                .iter()
                .all(|entry| !entry.dynamic_or_unknown)
        );
        assert_eq!(
            inventory.entries[0].exact_name,
            inventory.entries[1].exact_name
        );
        assert_ne!(inventory.entries[0].id, inventory.entries[1].id);
        assert_eq!(inventory.entries[0].source_location, None);
    }

    #[test]
    fn per_binary_listing_rejects_invalid_target_selector() {
        assert!(
            collect_complete_rust_inventory_per_binary(&[RustTestBinaryListingV1 {
                package: "core".into(),
                target: "--features=all".into(),
                stdout: b"suite::one: test\n".to_vec(),
                source_locations: Default::default(),
            }])
            .is_none()
        );
    }
}
