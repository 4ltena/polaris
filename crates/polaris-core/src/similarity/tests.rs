//! Names and sizes only; no folder is read here.
use super::*;

fn folder(id: &str, entries: &[(&str, u64)]) -> Folder {
    Folder {
        id: id.into(),
        entries: entries
            .iter()
            .map(|(name, size)| ((*name).to_string(), *size))
            .collect(),
    }
}

#[test]
fn two_copies_of_the_same_work_are_found_and_unrelated_folders_are_not() {
    let files = [
        ("src/main.rs", 1_000),
        ("README.md", 200),
        ("Cargo.toml", 80),
    ];
    let copy = [
        ("src/main.rs", 1_010),
        ("README.md", 200),
        ("Cargo.toml", 80),
    ];
    let other = [("index.js", 400), ("package.json", 120)];
    let graph = graph(&[
        folder("app", &files),
        folder("app-backup", &copy),
        folder("site", &other),
    ]);

    assert_eq!(graph.edges.len(), 1, "unrelated folders share no edge");
    let edge = &graph.edges[0];
    assert_eq!(
        (edge.left.as_str(), edge.right.as_str()),
        ("app", "app-backup")
    );
    assert!(edge.similarity >= 90, "{}", edge.similarity);
    assert!(edge.shared.contains(&"src/main.rs".to_string()));
    assert_eq!(
        graph.proposals[0].suggestion,
        Suggestion::SameWorkInTwoPlaces
    );
    assert!(graph.proposals[0].because.contains("類似度"));
    assert_eq!(graph.omitted, 0);
}

#[test]
fn a_partly_shared_folder_suggests_a_common_piece_not_a_duplicate() {
    // Seven files in common, one of its own each: a large shared part, not
    // the same folder twice.
    let common: Vec<(String, u64)> = (0..7)
        .map(|n| (format!("shared/part{n}.rs"), 500))
        .collect();
    let with_own = |id: &str, own: &str| Folder {
        id: id.into(),
        entries: common
            .iter()
            .cloned()
            .chain([(own.to_string(), 1_200)])
            .collect(),
    };
    let left = with_own("service", "service.rs");
    let right = with_own("worker", "worker.rs");
    let graph = graph(&[left, right]);
    let edge = &graph.edges[0];
    assert!(
        (SUGGEST_AT..90).contains(&edge.similarity),
        "{}",
        edge.similarity
    );
    assert_eq!(graph.proposals[0].suggestion, Suggestion::SharedPart);
}

#[test]
fn the_graph_is_bounded_and_says_how_many_it_left_out() {
    let folders: Vec<Folder> = (0..MAX_FOLDERS + 3)
        .map(|n| folder(&format!("f{n}"), &[("same.rs", 100)]))
        .collect();
    let graph = graph(&folders);
    assert_eq!(graph.omitted, 3);
    // Every remaining pair is identical, so the edges are the pairs of those.
    assert_eq!(graph.edges.len(), MAX_FOLDERS * (MAX_FOLDERS - 1) / 2);
    assert!(graph.edges.iter().all(|edge| edge.similarity == 100));
}

#[test]
fn different_sizes_under_the_same_names_lower_the_similarity() {
    let left = folder("one", &[("a.rs", 1_000), ("b.rs", 1_000)]);
    let right = folder("two", &[("a.rs", 1_000), ("b.rs", 10)]);
    let graph = graph(&[left, right]);
    assert!(graph.edges[0].similarity < 60, "{:?}", graph.edges[0]);
    assert!(graph.proposals.is_empty(), "a weak match suggests nothing");
}
