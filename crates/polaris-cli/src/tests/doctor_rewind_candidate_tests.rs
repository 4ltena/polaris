
use super::doctor_rewind_candidate_lines;
use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};

#[test]
fn absent_store_is_reported_without_creating_state() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let state = root.path().join("state");
    let data = root.path().join("data");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir_all(data.join("sessions-v2").join("broken")).unwrap();

    let lines = doctor_rewind_candidate_lines(&project, &state, &data);

    assert!(lines[0].contains("Storage not created"));
    assert!(lines.iter().any(|line| line.contains("broken")));
    assert!(!state.exists());
}

#[test]
fn rejected_candidate_id_and_reason_are_visible_without_terminal_controls() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let state = root.path().join("state");
    let data = root.path().join("data");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&state).unwrap();
    WorkspaceOwner::open(
        &project,
        &state.join("rewind-workspace"),
        RewindLimits::default(),
    )
    .unwrap();
    std::fs::create_dir_all(data.join("sessions-v2").join("broken\ncandidate")).unwrap();

    let lines = doctor_rewind_candidate_lines(&project, &state, &data);

    assert!(lines[0].contains("1 refused or indeterminate candidate"));
    assert!(lines[1].contains("broken\\ncandidate: state.json is missing"));
    assert!(lines.iter().all(|line| !line.contains('\n')));
}

#[test]
fn malformed_store_keeps_candidate_ids_visible() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let state = root.path().join("state");
    let data = root.path().join("data");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&state).unwrap();
    let store = state.join("rewind-workspace");
    std::fs::write(&store, b"keep").unwrap();
    std::fs::create_dir_all(data.join("sessions-v2").join("broken")).unwrap();

    let lines = doctor_rewind_candidate_lines(&project, &state, &data);

    assert!(lines[0].contains("Cannot verify storage"));
    assert!(lines.iter().any(|line| line.contains("broken")));
    assert_eq!(std::fs::read(store).unwrap(), b"keep");
}
