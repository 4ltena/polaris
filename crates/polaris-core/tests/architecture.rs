//! A snapshot test that confirms `docs/architecture.md` matches the
//! repository: which crate depends on which, and where a run touches the
//! outside world.
//!
//! Agents and reviewers read that document instead of exploring. A grep
//! finds the same call sites, but only for someone who already knows what
//! to grep for, and nothing at all reconstructs the crate graph by hand. If
//! the document drifts it stops being that answer, so drift fails loudly.
use std::path::{Path, PathBuf};

#[path = "support/repository_files.rs"]
mod repository_files;

/// A kind of outside contact, and how to recognise it in source.
struct Boundary {
    /// Heading in the document.
    title: &'static str,
    /// Why a reader cares: what goes wrong when one of these is wrong.
    risk: &'static str,
    /// Any line matching one of these is a call site.
    patterns: &'static [&'static str],
    /// Lines that match a pattern but are not this boundary. A reader who
    /// follows a false entry loses more time than one who greps by hand.
    exclude: &'static [&'static str],
    /// Files whose own job is to *implement* this boundary rather than to
    /// use it. They are named once instead of line by line, because listing
    /// a transport's own internals among its callers buries the callers.
    implementations: &'static [&'static str],
}

const BOUNDARIES: &[Boundary] = &[
    Boundary {
        title: "モデルへの送信",
        risk: "外部へデータが出る。費用が発生する",
        patterns: &[".complete(", "complete_in_turn"],
        // Same method name, different method. `grader.complete()` asks
        // whether a scoring run finished, and the desktop engine's own
        // `self.complete(Observation::..)` settles a run. Neither sends.
        exclude: &["grader.complete()", "self.complete(Observation"],
        implementations: &[
            "crates/polaris-provider/src/lib.rs",
            "crates/polaris-provider/src/openai.rs",
            "crates/polaris-provider/src/codex.rs",
            "crates/polaris-provider/src/role.rs",
            "crates/polaris-provider/src/local_inference.rs",
        ],
    },
    Boundary {
        title: "HTTP",
        risk: "外部へ要求が出る",
        patterns: &["reqwest::", "artifact::fetch("],
        exclude: &[],
        implementations: &[
            "crates/polaris-http/src/lib.rs",
            "crates/polaris-http/src/artifact.rs",
        ],
    },
    Boundary {
        title: "隔離実行",
        risk: "processが動く",
        patterns: &["run_confined"],
        exclude: &[],
        implementations: &[
            "crates/polaris-sandbox/src/lib.rs",
            "crates/polaris-sandbox/src/controlled.rs",
        ],
    },
    Boundary {
        title: "process起動",
        risk: "残留・port占有・回収漏れ",
        patterns: &["Command::new"],
        exclude: &[],
        implementations: &[],
    },
    Boundary {
        title: "gitの呼出し",
        risk: "利用者のrepositoryが変わる",
        patterns: &["Command::new(\"git\")", "read_only_git", "fn git("],
        exclude: &[],
        implementations: &[],
    },
];

fn repo_root() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path
}

/// The repository's own `.rs` files, tests excluded. A test that contacts
/// the outside does so under its own fixture, which is not the boundary a
/// reader is looking for.
fn production_files(root: &Path) -> Vec<String> {
    repository_files::repository_files(root, true)
        .into_iter()
        .filter(|path| {
            !path.contains("/tests/") && !path.ends_with("tests.rs") && !path.contains("/examples/")
        })
        .collect()
}

/// Lines outside `#[cfg(test)]`. The module attribute is the only form used
/// in this repository, and it always sits at the start of a line.
fn production_lines(body: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut in_test = false;
    let mut depth = 0i32;
    for (index, line) in body.lines().enumerate() {
        if !in_test && line.trim_start().starts_with("#[cfg(test)]") {
            in_test = true;
            depth = 0;
            continue;
        }
        if in_test {
            depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
            // `mod tests;` ends on its own line without ever opening a brace.
            if depth <= 0 && (line.contains('}') || line.trim_end().ends_with(';')) {
                in_test = false;
            }
            continue;
        }
        out.push((index + 1, line));
    }
    out
}

/// Which crate depends on which, read from each `Cargo.toml`. Only the
/// workspace's own crates: an external dependency is not part of the shape
/// a reader is trying to hold in mind.
fn crate_edges(root: &Path) -> Vec<(String, Vec<String>)> {
    let mut graph = Vec::new();
    let mut directories: Vec<PathBuf> = std::fs::read_dir(root.join("crates"))
        .expect("the crates directory")
        .filter_map(|held| held.ok().map(|entry| entry.path()))
        .filter(|path| path.join("Cargo.toml").is_file())
        .collect();
    directories.sort();
    for directory in directories {
        let body = std::fs::read_to_string(directory.join("Cargo.toml")).expect("a manifest");
        let name = body
            .lines()
            .find_map(|line| line.trim().strip_prefix("name = \""))
            .and_then(|held| held.split('"').next())
            .expect("a package name")
            .to_owned();
        // Only the `[dependencies]` table. A dev-dependency exists for the
        // tests and does not describe how the product is put together.
        let mut deps = Vec::new();
        let mut inside = false;
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                inside = trimmed == "[dependencies]";
                continue;
            }
            if inside
                && let Some(dependency) = trimmed.split(['=', ' ']).next()
                && dependency.starts_with("polaris-")
            {
                deps.push(dependency.to_owned());
            }
        }
        graph.push((name, deps));
    }
    graph
}

/// `polaris-` is on every name in the graph, so it carries no information
/// and costs a reader's attention on every node.
fn short(name: &str) -> &str {
    name.strip_prefix("polaris-").unwrap_or(name)
}

fn build_expected(root: &Path) -> String {
    let files = production_files(root);
    let mut body = String::from(
        "# アーキテクチャ\n\n\
         This is a generated file. Do not edit it by hand. \
         `crates/polaris-core/tests/architecture.rs`が実際のsourceから作り直し、\
         この文書と突き合わせる。ずれたら試験が落ちる。更新するには:\n\n\
         ```\nUPDATE_ARCHITECTURE=1 cargo test -p polaris-core --test architecture\n```\n\n\
         この文書は2つを載せる。crateの依存関係と、外部との接続点である。要求の組み立て順は`docs/context-efficiency.md`、fileごとの一覧は`docs/filemap.md`にある。\n",
    );
    body.push_str("\n## crateの依存\n\n");
    let graph = crate_edges(root);
    body.push_str("```mermaid\ngraph TD\n");
    for (name, deps) in &graph {
        for dependency in deps {
            body.push_str(&format!("  {} --> {}\n", short(name), short(dependency)));
        }
    }
    body.push_str("```\n\n");
    body.push_str(&format!(
        "crate {}件、依存 {}件。名前の`polaris-`は省いた。開発用の依存は、\
         製品の組み立てを表さないため含めない。\n\n## 外部との接続点\n\n\
         列挙するのは、外部へデータが出るか、processが動くか、利用者の木が変わる箇所である。\
         試験とexampleは除く。境界そのものを実装しているfileは、呼び出し側を埋もれさせないため\
         一度だけ名前を挙げる。\n",
        graph.len(),
        graph.iter().map(|(_, deps)| deps.len()).sum::<usize>(),
    ));
    for boundary in BOUNDARIES {
        body.push_str(&format!(
            "\n### {}\n\n{}。\n\n",
            boundary.title, boundary.risk
        ));
        let mut sites: Vec<String> = Vec::new();
        for path in &files {
            if boundary.implementations.contains(&path.as_str()) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(root.join(path)) else {
                continue;
            };
            for (number, line) in production_lines(&text) {
                // A doc comment naming the call is not the call.
                let trimmed = line.trim_start();
                if trimmed.starts_with("//")
                    || trimmed.starts_with("/*")
                    || trimmed.starts_with("*")
                {
                    continue;
                }
                if boundary.exclude.iter().any(|held| line.contains(held)) {
                    continue;
                }
                if boundary.patterns.iter().any(|held| line.contains(held)) {
                    sites.push(format!("- `{path}:{number}`"));
                }
            }
        }
        if !boundary.implementations.is_empty() {
            body.push_str(&format!(
                "実装: {}\n\n",
                boundary
                    .implementations
                    .iter()
                    .map(|held| format!("`{held}`"))
                    .collect::<Vec<_>>()
                    .join("、")
            ));
        }
        if sites.is_empty() {
            body.push_str("呼び出し側なし。\n");
        } else {
            body.push_str(&format!("呼び出し側 {}件。\n\n", sites.len()));
            body.push_str(&sites.join("\n"));
            body.push('\n');
        }
    }
    body
}

#[test]
fn architecture_matches_repository() {
    let root = repo_root();
    let expected = build_expected(&root);
    let path = root.join("docs/architecture.md");
    if std::env::var("UPDATE_ARCHITECTURE").is_ok() {
        std::fs::write(&path, &expected).expect("the document is written");
        return;
    }
    let actual = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        actual, expected,
        "docs/architecture.md drifted from the repository. \
         Run `UPDATE_ARCHITECTURE=1 cargo test -p polaris-core --test architecture`."
    );
}
