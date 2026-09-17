//! Decisions only; no file is read or written.
use super::*;

const WANTED: &[&str] = &[".next/", "node_modules/", "coverage/"];

#[test]
fn a_missing_rule_is_added_and_one_already_there_is_not() {
    let (add, withheld) = plan_additions("node_modules/\n", WANTED);
    assert_eq!(add, vec![".next/", "coverage/"]);
    assert_eq!(
        withheld,
        vec![Withheld {
            rule: "node_modules/".into(),
            reason: WithheldReason::AlreadyPresent,
        }]
    );
}

#[test]
fn a_rule_that_would_undo_a_negation_is_withheld_rather_than_appended() {
    // Someone deliberately keeps one file out of an otherwise ignored tree.
    let existing = "# keep the report\n!coverage/summary.json\n";
    let (add, withheld) = plan_additions(existing, WANTED);
    assert_eq!(add, vec![".next/", "node_modules/"]);
    assert_eq!(
        withheld,
        vec![Withheld {
            rule: "coverage/".into(),
            reason: WithheldReason::ContradictsNegation {
                line: "!coverage/summary.json".into(),
            },
        }]
    );
}

#[test]
fn the_same_path_written_differently_still_counts_as_a_collision() {
    for negation in ["!coverage", "!/coverage/", "!coverage/deep/file.txt"] {
        let (add, withheld) = plan_additions(negation, &["coverage/"]);
        assert!(add.is_empty(), "{negation}");
        assert_eq!(withheld.len(), 1, "{negation}");
    }
    // A leading `**/` names the same thing wherever it sits.
    let (add, _) = plan_additions("!coverage/keep", &["**/coverage"]);
    assert!(add.is_empty());
}

#[test]
fn a_commented_out_negation_is_not_a_negation() {
    let (add, withheld) = plan_additions("# !coverage/summary.json\n", &["coverage/"]);
    assert_eq!(add, vec!["coverage/"]);
    assert!(withheld.is_empty());
}

#[test]
fn an_unrelated_negation_does_not_hold_anything_back() {
    let (add, withheld) = plan_additions("!src/generated.ts\n", WANTED);
    assert_eq!(add, WANTED.to_vec());
    assert!(withheld.is_empty());
}

#[test]
fn the_recipes_order_is_the_order_the_rules_are_added_in() {
    let (add, _) = plan_additions("", WANTED);
    assert_eq!(add, WANTED.to_vec(), "nothing is sorted or regrouped");
}

mod writing {
    use super::*;

    #[test]
    fn existing_lines_keep_their_text_and_their_order() {
        let dir = tempfile::tempdir().expect("a temporary project");
        let before = "# build output\n!coverage/summary.json\nnode_modules/\n";
        std::fs::write(dir.path().join(".gitignore"), before).expect("the file");
        append(dir.path(), &[".next/", "dist/"]).expect("the rules are appended");
        let after = std::fs::read_to_string(dir.path().join(".gitignore")).expect("the file");
        assert!(after.starts_with(before), "{after:?}");
        assert_eq!(after, format!("{before}.next/\ndist/\n"));
    }

    #[test]
    fn a_file_without_a_final_newline_does_not_get_its_last_rule_merged() {
        let dir = tempfile::tempdir().expect("a temporary project");
        std::fs::write(dir.path().join(".gitignore"), "node_modules/").expect("the file");
        append(dir.path(), &[".next/"]).expect("the rule is appended");
        let after = std::fs::read_to_string(dir.path().join(".gitignore")).expect("the file");
        assert_eq!(after, "node_modules/\n.next/\n");
    }

    #[test]
    fn a_project_without_a_gitignore_gets_one() {
        let dir = tempfile::tempdir().expect("a temporary project");
        append(dir.path(), &[".next/"]).expect("the rule is appended");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitignore")).expect("the file"),
            ".next/\n"
        );
    }

    #[test]
    fn nothing_to_add_leaves_the_file_untouched() {
        let dir = tempfile::tempdir().expect("a temporary project");
        append(dir.path(), &[]).expect("nothing is appended");
        assert!(
            !dir.path().join(".gitignore").exists(),
            "an empty list must not create a file"
        );
    }
}
