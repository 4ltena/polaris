//! Windows recipe setting refusal tests that preserve project state and avoid approval requests.
use super::*;
use crate::approval::{ApprovalPolicy, Decision};
use polaris_sandbox::SandboxMode;

struct Unasked;

impl Approver for Unasked {
    fn ask(&mut self, _reason: &str) -> Decision {
        panic!("unsupported setting operation must not request approval")
    }
}

async fn assert_unavailable(tool: &SettingTool) {
    let sandbox = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).unwrap();
    let mut gate = Gate::new(ApprovalPolicy::Always);
    let mut approver = Unasked;
    for arguments in [
        serde_json::json!({ "op": "plan" }),
        serde_json::json!({ "op": "tidy" }),
        serde_json::json!({ "op": "scaffold" }),
        serde_json::json!({ "op": "unknown" }),
        serde_json::json!({}),
        serde_json::json!(null),
    ] {
        let error = tool
            .call(&arguments, &sandbox, &mut gate, &mut approver)
            .await
            .err()
            .expect("unsupported setting operation must fail");
        assert_eq!(
            error,
            "このプラットフォームでは設定操作に必要な保護を確認できません。"
        );
    }
}

fn names(path: &std::path::Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn windows_recipe_setting_unavailable_does_not_create_root() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("absent-project");
    let before = names(parent.path());
    let tool = SettingTool::new(root.clone());
    assert_unavailable(&tool).await;
    assert!(!root.exists());
    assert_eq!(names(parent.path()), before);
}

#[tokio::test]
async fn windows_recipe_setting_unavailable_preserves_existing_project() {
    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let files = [
        root.path().join("package.json"),
        root.path().join(".gitignore"),
        nested.join("retained.bin"),
    ];
    let contents: [&[u8]; 3] = [
        b"{\"private\":true,\"dependencies\":{\"next\":\"15.0.0\"}}\n",
        b"# retained\n!node_modules/fixture\n",
        b"retained\0\xffproject",
    ];
    for (path, bytes) in files.iter().zip(contents) {
        std::fs::write(path, bytes).unwrap();
    }
    let before = [names(root.path()), names(&nested)];
    let tool = SettingTool::new(root.path().to_owned());
    assert_unavailable(&tool).await;
    assert_eq!([names(root.path()), names(&nested)], before);
    for (path, bytes) in files.iter().zip(contents) {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}
