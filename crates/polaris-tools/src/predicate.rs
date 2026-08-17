//! 書き込みが拒否されるかを事前に予測する。
//!
//! ここは助言であって強制ではない。強制は `polaris-sandbox` が OS へ委譲する。
//! 述語が要るのは、強制側が理由を説明できないからである。親ディレクトリが
//! 存在しない場合の拒否は `ENOENT` を返し、`EACCES` は Linux では拒否だが
//! macOS では通常の失敗なので、errno から「方針違反」を復元できない。
//!
//! 述語と強制は必ず食い違う。食い違いが破れにならないのは、述語が承認を
//! 求める側にしか倒れないためである。述語が甘ければ強制が止め、述語が
//! 厳しければ余計な確認が 1 回増える。

use std::path::{Path, PathBuf};

use polaris_sandbox::{SandboxMode, SandboxPolicy};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    NeedsApproval { reason: String },
}

pub fn predict(policy: &SandboxPolicy, target: &Path) -> Verdict {
    if policy.mode() == SandboxMode::FullAccess {
        return Verdict::Allowed;
    }

    let resolved = resolve_for_judgement(target);

    if !policy.contains(&resolved) {
        return Verdict::NeedsApproval {
            reason: format!(
                "{} は書込可能な範囲の外にある。方針 {}",
                target.display(),
                policy.describe()
            ),
        };
    }

    // ワークスペースの内側であっても、機密として扱うパスは承認へ回す。
    // OS の強制はこの区別を表現できない。
    if crate::path_policy::is_denied(&resolved) {
        return Verdict::NeedsApproval {
            reason: format!(
                "{} は機密として扱うパスに該当する。方針 {}",
                target.display(),
                policy.describe()
            ),
        };
    }

    if has_extra_hard_links(&resolved) {
        return Verdict::NeedsApproval {
            reason: format!(
                "{} はハードリンクを持つ（書き込みが範囲外の実体へ届きうる）。方針 {}",
                target.display(),
                policy.describe()
            ),
        };
    }

    Verdict::Allowed
}

/// 判定用にパスを解決する。存在しないパスは canonicalize できないので、
/// 存在する最も近い祖先まで遡って正規化し、残りを繋ぎ直す。新規作成では
/// 対象も途中のディレクトリも存在しないのが普通であり、ここを素朴に
/// canonicalize すると全ての新規作成が判定不能になる。
///
/// 正規化したベースから先は、残りのパス成分を字句的に処理する。
/// 成分には `.` や `..` が含まれることがあるが、存在しないため
/// canonicalize は使えない。その代わり、字句的に処理することが正しい理由は
/// 以下の通り：ベースは canonicalize() の結果なのでシンボリックリンクを
/// 含まず、残りの成分は存在しないのでこれもシンボリックリンク不可である。
/// したがって `base/../x` は正確に `base.parent()/x` と等価であり、
/// 字句的解決が妥当である。
fn resolve_for_judgement(target: &Path) -> PathBuf {
    use std::path::Component;

    if let Ok(c) = target.canonicalize() {
        return c;
    }

    // ターゲットの全コンポーネントを前もって収集する。
    let target_components: Vec<Component> = target.components().collect();

    let mut cursor = target;
    let mut depth = 0; // 遡った深さを追跡する。
    loop {
        match cursor.parent() {
            Some(parent) => {
                depth += 1;
                if let Ok(base) = parent.canonicalize() {
                    // base の構成を理解するために、target の最初の
                    // (components.len() - depth) コンポーネントが base に
                    // 対応するはずである（正確に一致するとは限らないが）。
                    // 残りのコンポーネントを字句的に処理する。
                    let remaining_start = if target_components.len() > depth {
                        target_components.len() - depth
                    } else {
                        0
                    };

                    let mut out = base;
                    for component in &target_components[remaining_start..] {
                        match component {
                            Component::CurDir => {
                                // `.` は無視する。
                            }
                            Component::ParentDir => {
                                // `..` は親へ遡る。
                                out.pop();
                            }
                            Component::Normal(name) => {
                                out.push(name);
                            }
                            Component::RootDir | Component::Prefix(_) => {
                                // 存在しないパスの残りから RootDir や Prefix が
                                // 出ることはない。出たら判定不能の信号。
                                return target.to_path_buf();
                            }
                        }
                    }
                    return out;
                }
                cursor = parent;
            }
            // ルートまで遡っても正規化できない。判定できないものを
            // 「内側」と答えないため、元のパスをそのまま返す。方針の
            // ルートと一致しないので、呼び出し側では承認へ倒れる。
            None => return target.to_path_buf(),
        }
    }
}

/// 既存ファイルが複数のリンクを持つか。存在しないパスとメタデータを
/// 読めないパスは偽を返す。ここで真を返せないケースがあることは、
/// 仕様の「保証しない範囲」に明記した限界そのものである。
fn has_extra_hard_links(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path)
            .map(|m| m.is_file() && m.nlink() > 1)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    fn workspace(root: &std::path::Path) -> SandboxPolicy {
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[root.to_path_buf()])
            .expect("方針を作れない")
    }

    #[test]
    fn a_target_inside_the_root_is_allowed() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(dir.path());
        assert_eq!(
            predict(&policy, &dir.path().join("a.txt")),
            Verdict::Allowed
        );
    }

    #[test]
    fn a_target_outside_the_root_needs_approval_and_says_where_it_may_write() {
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(root.path());

        let target = outside.path().join("b.txt");
        let Verdict::NeedsApproval { reason } = predict(&policy, &target) else {
            panic!("ルート外が許可された");
        };
        // 仕様は拒否されたパス、現在の方針、書込可能ルートの 3 点を要求する。
        // どれか 1 つでも欠けると、モデルは同じ失敗を繰り返す。
        assert!(
            reason.contains(&target.display().to_string()),
            "パスが無い: {reason}"
        );
        assert!(reason.contains("workspace-write"), "方針が無い: {reason}");
        assert!(
            reason.contains(&root.path().canonicalize().unwrap().display().to_string()),
            "書込可能ルートが無い: {reason}"
        );
    }

    #[test]
    fn a_target_whose_parent_does_not_exist_yet_is_still_judged_by_its_ancestors() {
        // write は新規作成なので、対象も途中のディレクトリも存在しないことが
        // 普通にある。存在しないパスは canonicalize できないため、素朴に
        // canonicalize すると全ての新規作成が「判定不能」に落ちる。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(dir.path());
        let deep = dir.path().join("no/such/dir/c.txt");
        assert_eq!(predict(&policy, &deep), Verdict::Allowed);
    }

    #[test]
    fn a_symlink_pointing_outside_the_root_needs_approval() {
        // ルートの内側にあるシンボリックリンクが外を指していれば、
        // 見かけのパスは内側でも書き先は外側である。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, "original").expect("書けない");

        let link = root.path().join("looks-inside.txt");
        std::os::unix::fs::symlink(&victim, &link).expect("symlink");

        let policy = workspace(root.path());
        assert!(
            matches!(predict(&policy, &link), Verdict::NeedsApproval { .. }),
            "外を指すリンクが許可された"
        );
    }

    #[test]
    fn an_existing_hardlink_is_surfaced_for_approval() {
        // ハードリンク経由の書き込みは実サンドボックスでも拒否されない。
        // 両機構ともパスで判定し inode で判定しないためである（仕様の
        // 「保証しない範囲」参照）。述語側で気づける唯一の手掛かりが
        // リンク数なので、複数リンクを持つ既存ファイルは承認へ回す。
        // これは緩和であって保証ではない。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let real = outside.path().join("real.txt");
        std::fs::write(&real, "original").expect("書けない");

        let linked = root.path().join("hardlink.txt");
        std::fs::hard_link(&real, &linked).expect("hard_link");

        let policy = workspace(root.path());
        let Verdict::NeedsApproval { reason } = predict(&policy, &linked) else {
            panic!("ハードリンクが素通りした");
        };
        assert!(
            reason.contains("ハードリンク"),
            "理由が伝わらない: {reason}"
        );
    }

    #[test]
    fn a_sensitive_path_inside_the_root_still_needs_approval() {
        // OS の強制はワークスペース内側の機密ファイルを区別できない。
        // 「書込可能ルートの内側だから安全」ではないので、読み取り側と
        // 同じ path_policy をここでも通す。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(root.path());
        let secret = root.path().join(".env");
        assert!(
            matches!(predict(&policy, &secret), Verdict::NeedsApproval { .. }),
            ".env への書き込みが素通りした"
        );
    }

    #[test]
    fn read_only_needs_approval_for_any_write() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("方針");
        assert!(matches!(
            predict(&policy, &dir.path().join("a.txt")),
            Verdict::NeedsApproval { .. }
        ));
    }

    #[test]
    fn full_access_allows_any_path() {
        let policy = SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("方針");
        assert_eq!(
            predict(&policy, std::path::Path::new("/etc/hosts")),
            Verdict::Allowed
        );
    }

    #[test]
    fn a_parent_dir_component_that_escapes_needs_approval() {
        // 存在しないディレクトリ a を通過してから `..` で脱出する場合、
        // a が存在しなくても `..` の効果は変わらない。`<root>/a/../../outside`
        // は字句的には `<root>/../outside` と同等で、ルート外へ出ている。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let outside = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(root.path());

        let target = root
            .path()
            .join("a/../../")
            .join(outside.path().file_name().expect("ファイル名が取れない"))
            .join("new.txt");
        let Verdict::NeedsApproval { .. } = predict(&policy, &target) else {
            panic!("脱出パスが許可された");
        };
    }

    #[test]
    fn a_parent_dir_component_that_stays_inside_is_allowed() {
        // `<root>/sub/../f.txt` は字句的に `<root>/f.txt` となり、ルート内に
        // 留まる。sub が存在しなくても成立する。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(root.path());

        let target = root.path().join("sub/../f.txt");
        assert_eq!(predict(&policy, &target), Verdict::Allowed);
    }

    #[test]
    fn a_parent_dir_that_returns_inside_stays_allowed() {
        // `<root>/../<root-name>/f.txt` は一度ルート外へ出るが、その後
        // ルート自身の親から root に戻ってくる。字句的解決では
        // この往復は尊重される。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(root.path());
        let root_name = root
            .path()
            .file_name()
            .expect("ルートのファイル名が取れない");

        let target = root.path().join("../").join(root_name).join("f.txt");
        assert_eq!(predict(&policy, &target), Verdict::Allowed);
    }

    #[test]
    fn a_parent_dir_under_an_existing_directory_is_canonicalised() {
        // 存在するディレクトリを通してから `..` を使う場合、
        // canonicalize の高速路で処理される。この路は正しく `..` を処理する。
        let root = tempfile::tempdir().expect("一時ディレクトリ");
        std::fs::create_dir(root.path().join("existing")).expect("ディレクトリ作成");
        let policy = workspace(root.path());

        let target = root.path().join("existing/../f.txt");
        // existing が実在しているため `existing` まで canonicalize でき、
        // その後 `..` を処理して `f.txt` を push する。結果はルート内。
        assert_eq!(predict(&policy, &target), Verdict::Allowed);
    }
}
