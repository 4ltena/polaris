//! The corpus is data the comparison depends on, so its shape is checked
//! here rather than discovered when a measurement comes out wrong.
use super::*;

const BYTES: &[u8] = include_bytes!("../../tests/fixtures/preselection_corpus.json");

fn corpus() -> Corpus {
    Corpus::parse(BYTES).expect("the shipped corpus parses")
}

#[test]
fn the_shipped_corpus_loads_and_every_label_names_a_tool_that_exists() {
    let held = corpus();
    assert_eq!(held.schema_version, 1);
    assert!(held.tool_count() >= 20, "{} tools", held.tool_count());
    assert!(held.tasks.len() >= 3);
    // A label naming a tool the catalogue does not hold would count as a
    // miss forever, and nothing would say why. `parse` refuses it.
    let mut broken: serde_json::Value = serde_json::from_slice(BYTES).unwrap();
    broken["tasks"][0]["useful"] = serde_json::json!(["nothing_like_this"]);
    assert!(matches!(
        Corpus::parse(&serde_json::to_vec(&broken).unwrap()),
        Err(CorpusError::UnknownLabel { .. })
    ));
}

#[test]
fn the_index_is_the_same_every_time_and_changes_when_a_description_does() {
    let held = corpus();
    assert_eq!(held.index(), held.index(), "two builds must agree");
    let built = held.index();
    assert_eq!(built.len(), held.servers.len());
    assert_eq!(
        built.values().map(|entry| entry.tools.len()).sum::<usize>(),
        held.tool_count()
    );

    let mut changed: serde_json::Value = serde_json::from_slice(BYTES).unwrap();
    changed["servers"]["files"][0]["description"] = serde_json::json!("something else entirely");
    let changed = Corpus::parse(&serde_json::to_vec(&changed).unwrap()).unwrap();
    assert_ne!(
        changed.index()["files"].tools[0].summary_hash,
        built["files"].tools[0].summary_hash,
        "a changed description must change what identifies it"
    );
    assert_ne!(
        changed.digest(),
        held.digest(),
        "and a measurement taken against it must not claim to be comparable"
    );
}

#[test]
fn a_row_separates_what_was_missed_from_what_was_paid_for_unnecessarily() {
    let held = corpus();
    let task = held
        .tasks
        .iter()
        .find(|task| task.id == "test-failure")
        .expect("the fixture task");
    let row = Row::score(
        &held,
        "legacy",
        task,
        "selected",
        vec!["run_tests".into(), "deploy".into()],
        None,
        true,
    );
    assert_eq!(row.wrong, vec!["deploy".to_owned()]);
    assert!(row.missed.contains(&"grep".to_owned()));
    assert!(!row.missed.contains(&"run_tests".to_owned()));
    // The two failures are different: one means the run cannot do the
    // work, the other means it paid for schema it did not need.
    assert!(!row.missed.is_empty() && !row.wrong.is_empty());
    assert_eq!(row.corpus_digest, held.digest());
}

#[test]
fn an_unmeasured_arm_is_not_an_arm_that_chose_nothing() {
    let held = corpus();
    let task = &held.tasks[0];
    let refused = Row::score(
        &held,
        "local",
        task,
        "unsupported_backend",
        vec![],
        None,
        false,
    );
    let chose_nothing = Row::score(&held, "legacy", task, "none_selected", vec![], None, true);
    assert_eq!(refused.picked, chose_nothing.picked);
    assert_ne!(
        refused.measured, chose_nothing.measured,
        "an empty result must say which of the two it is"
    );
    // Both look like a total miss on the labels, which is exactly why the
    // flag has to carry the difference.
    assert_eq!(refused.missed.len(), task.useful.len());
}
