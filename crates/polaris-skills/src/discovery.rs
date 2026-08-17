//! skill の探索。1 件の破損が全体を巻き込まないよう、読めないものは飛ばす。
//! ただし読み捨てはしない。飛ばした skill とその理由は `Discovered::skipped`
//! として呼び出し側へ返す。呼び出し側（将来の CLI）がそれを表示する。

use std::path::{Path, PathBuf};

use crate::frontmatter;
use crate::{Skill, SkillError};

/// skill を 1 件飛ばした理由。`SKILL.md` そのものを読めなかったのか、読めた
/// が検証に落ちたのかは別の失敗なので、区別して運ぶ。
#[derive(Debug, thiserror::Error)]
pub enum SkipCause {
    /// `SKILL.md` は存在するが読めない（権限、あるいはパス自体がディレクトリ
    /// である、など）。パースへ一度も到達していないので `SkillError` は
    /// 手に入らない。
    #[error("SKILL.md を読めない: {0}")]
    Unreadable(std::io::Error),
    /// `SKILL.md` は読めたがフロントマターの検証に落ちた。
    #[error(transparent)]
    Invalid(SkillError),
}

/// 飛ばした skill 1 件。ディレクトリ名と理由を運ぶ。`SkillError` 側が
/// 自分の識別子を文言に含めているのと同じ理由で、ここでもディレクトリ名を
/// 明示のフィールドとして持つ — 呼び出し側が理由の種類によらず一貫して
/// 「どの skill が飛ばされたか」を取り出せるようにするため。
#[derive(Debug, thiserror::Error)]
#[error("{dir_name}: {cause}")]
pub struct Skipped {
    pub dir_name: String,
    #[source]
    pub cause: SkipCause,
}

/// `discover`/`discover_in` の結果。読み込めた skill と、読み込めずに飛ばした
/// skill を理由付きで両方運ぶ。
#[derive(Debug, Default)]
pub struct Discovered {
    pub skills: Vec<Skill>,
    pub skipped: Vec<Skipped>,
}

/// 与えられたディレクトリ群を順に走査する。名前が衝突したら先に見つけたものを
/// 採る。読めない・検証に落ちた skill は `skipped` に理由付きで積んで、探索
/// 自体は続ける。ディレクトリ内のエントリはファイル名でソートしてから処理する
/// — `read_dir` の返す順序に実行ごとの再現性はなく、探索結果の順序を
/// ファイルシステム任せにはできない。探索先ディレクトリ群自体の順序（外側の
/// ループ）は呼び出し側が渡した並びをそのまま使う。
pub fn discover_in(dirs: &[PathBuf]) -> Discovered {
    let mut skills: Vec<Skill> = Vec::new();
    let mut skipped: Vec<Skipped> = Vec::new();
    for dir in dirs {
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut entries: Vec<std::fs::DirEntry> = read_dir.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let manifest = path.join("SKILL.md");
            let text = match std::fs::read_to_string(&manifest) {
                Ok(text) => text,
                // `SKILL.md` が無いことは「このディレクトリは skill ではない」
                // であって、破損した skill ではない。それ以外の読み取り失敗
                // （権限、あるいはパスがディレクトリであるなど）は、パースへ
                // 到達する前に skill が消えるのと同じ穴なので報告する。
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    skipped.push(Skipped {
                        dir_name: dir_name.to_string(),
                        cause: SkipCause::Unreadable(err),
                    });
                    continue;
                }
            };
            match frontmatter::parse(&text, dir_name) {
                Ok((name, description, body)) => {
                    if skills.iter().any(|s| s.name == name) {
                        continue;
                    }
                    skills.push(Skill {
                        name,
                        description,
                        body,
                        path: manifest,
                    });
                }
                Err(err) => skipped.push(Skipped {
                    dir_name: dir_name.to_string(),
                    cause: SkipCause::Invalid(err),
                }),
            }
        }
    }
    Discovered { skills, skipped }
}

/// 既定の 2 箇所と設定で追加された場所を、この順に走査する。
pub fn discover(project_root: &Path, extra_paths: &[PathBuf]) -> Discovered {
    let mut dirs = vec![project_root.join(".polaris").join("skills")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join(".polaris").join("skills"));
    }
    dirs.extend_from_slice(extra_paths);
    discover_in(&dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(root: &std::path::Path, name: &str, desc: &str, body: &str) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).expect("作れない");
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\n{body}\n"),
        )
        .expect("書けない");
    }

    /// `HOME` はプロセス全体で共有される。cargo test は同一プロセス内で
    /// テストを並行に走らせるため、差し替える側どうしを直列化しないと、
    /// 一方が張った `HOME` をもう一方が読む。
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `HOME` を差し替え、スコープを抜けたら（パニックしても）元へ戻す。
    struct HomeGuard {
        prev: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            // SAFETY: HOME_LOCK を保持している間だけ書き換える。この
            // クレートで HOME を読むのは discover だけであり、その呼び出し
            // 元テストはすべて同じロックを取る。
            unsafe {
                match &self.prev {
                    Some(p) => std::env::set_var("HOME", p),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    fn set_home(home: Option<&std::path::Path>) -> HomeGuard {
        // 直前のテストがロックを保持したままパニックしても、後続を巻き添えに
        // しない（毒された中身は単なる ()）。
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HOME");
        // SAFETY: 上と同じ。
        unsafe {
            match home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
        HomeGuard { prev, _lock: lock }
    }

    #[test]
    fn the_project_directory_is_searched_before_home() {
        // 仕様が固定している探索順そのもの。順序を入れ替えると、個人の skill
        // とプロジェクトの skill が同名で衝突したときの勝者が静かに反転する
        // —— discover_in の衝突テストは「先に渡された方が勝つ」しか見ないので、
        // 渡す順序を作る discover 側でひっくり返されると気付けない。
        let project = tempfile::tempdir().expect("一時");
        let home = tempfile::tempdir().expect("一時");
        put(
            &project.path().join(".polaris").join("skills"),
            "dup",
            "ぷろじぇくと側",
            "PROJECT",
        );
        put(
            &home.path().join(".polaris").join("skills"),
            "dup",
            "ほーむ側",
            "HOME",
        );

        let _guard = set_home(Some(home.path()));
        let found = discover(project.path(), &[]);

        assert_eq!(found.skills.len(), 1, "同名は1件へ解決されるべき");
        assert_eq!(
            found.skills[0].body.trim(),
            "PROJECT",
            "探索順が仕様と違う。プロジェクトの skill が ~/.polaris/skills に負けている"
        );
    }

    #[test]
    fn home_is_searched_before_the_configured_extra_paths() {
        // 設定で足した場所は3番目。ここでは同時に「追加パスが実際に走査
        // されている」ことも確かめる。走査されていなければ順序の主張は
        // 空振りするため、勝者だけを見ても意味が無い。
        let project = tempfile::tempdir().expect("一時");
        let home = tempfile::tempdir().expect("一時");
        let extra = tempfile::tempdir().expect("一時");
        put(
            &home.path().join(".polaris").join("skills"),
            "dup",
            "ほーむ側",
            "HOME",
        );
        put(extra.path(), "dup", "設定側", "EXTRA");
        put(
            extra.path(),
            "only-in-extra",
            "設定でのみ足した場所",
            "EXTRA-ONLY",
        );

        let _guard = set_home(Some(home.path()));
        let found = discover(project.path(), &[extra.path().to_path_buf()]);

        assert!(
            found.skills.iter().any(|s| s.name == "only-in-extra"),
            "設定の skills.paths が走査されていない: {:?}",
            found.skills.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        let dup = found
            .skills
            .iter()
            .find(|s| s.name == "dup")
            .expect("dup が見つからない");
        assert_eq!(
            dup.body.trim(),
            "HOME",
            "探索順が仕様と違う。~/.polaris/skills が設定の追加パスに負けている"
        );
    }

    #[test]
    fn home_skills_are_found_when_the_project_has_none() {
        // 上の2つは衝突の勝者を見るので、~/.polaris/skills を丸ごと外しても
        // 「プロジェクトが勝つ」側は通ってしまう。既定の2番目が実際に
        // 走査されていること自体を独立に固定する。
        let project = tempfile::tempdir().expect("一時");
        let home = tempfile::tempdir().expect("一時");
        put(
            &home.path().join(".polaris").join("skills"),
            "personal",
            "個人の skill",
            "HOME",
        );

        let _guard = set_home(Some(home.path()));
        let found = discover(project.path(), &[]);

        let names: Vec<&str> = found.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["personal"],
            "~/.polaris/skills が走査されていない"
        );
    }

    #[test]
    fn a_missing_home_does_not_stop_the_project_from_being_searched() {
        // HOME が無い環境でも、プロジェクトの skill は読めなければならない。
        let project = tempfile::tempdir().expect("一時");
        put(
            &project.path().join(".polaris").join("skills"),
            "alpha",
            "あるふぁ",
            "PROJECT",
        );

        let _guard = set_home(None);
        let found = discover(project.path(), &[]);

        let names: Vec<&str> = found.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha"]);
        assert!(found.skipped.is_empty());
    }

    #[test]
    fn finds_skills_in_each_directory() {
        let a = tempfile::tempdir().expect("一時");
        let b = tempfile::tempdir().expect("一時");
        put(a.path(), "alpha", "あるふぁ", "A");
        put(b.path(), "beta", "べーた", "B");

        let found = discover_in(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let mut names: Vec<&str> = found.skills.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert!(found.skipped.is_empty());
    }

    #[test]
    fn first_directory_wins_on_a_name_collision() {
        let first = tempfile::tempdir().expect("一時");
        let second = tempfile::tempdir().expect("一時");
        put(first.path(), "dup", "さき", "FIRST");
        put(second.path(), "dup", "あと", "SECOND");

        let found = discover_in(&[first.path().to_path_buf(), second.path().to_path_buf()]);
        assert_eq!(found.skills.len(), 1);
        assert_eq!(found.skills[0].body.trim(), "FIRST");
    }

    #[test]
    fn an_invalid_skill_is_skipped_without_killing_the_others() {
        let root = tempfile::tempdir().expect("一時");
        put(root.path(), "good", "よい", "OK");
        let bad = root.path().join("Bad-Name");
        std::fs::create_dir_all(&bad).expect("作れない");
        std::fs::write(
            bad.join("SKILL.md"),
            "---\nname: Bad-Name\ndescription: x\n---\n",
        )
        .ok();

        let found = discover_in(&[root.path().to_path_buf()]);
        assert_eq!(
            found.skills.len(),
            1,
            "壊れた skill 1 件で全部が落ちてはいけない"
        );
        assert_eq!(found.skills[0].name, "good");
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let found = discover_in(&[std::path::PathBuf::from("/does/not/exist")]);
        assert!(found.skills.is_empty());
        assert!(found.skipped.is_empty());
    }

    #[test]
    fn a_directory_without_skill_md_is_ignored() {
        let root = tempfile::tempdir().expect("一時");
        std::fs::create_dir_all(root.path().join("notaskill")).expect("作れない");
        let found = discover_in(&[root.path().to_path_buf()]);
        assert!(found.skills.is_empty());
        assert!(found.skipped.is_empty());
    }

    #[test]
    fn a_skipped_skill_names_the_directory_that_failed() {
        // discover_in が壊れた skill を黙って捨てるだけでは、前段の
        // frontmatter::parse がどれだけ丁寧に skill 名を運んでも利用者には
        // 何も届かない。skipped 側にエラーが載り、かつそのエラー文言が
        // 壊れたディレクトリ名を含むことを確かめる。
        let root = tempfile::tempdir().expect("一時");
        let bad = root.path().join("Bad-Name");
        std::fs::create_dir_all(&bad).expect("作れない");
        std::fs::write(
            bad.join("SKILL.md"),
            "---\nname: Bad-Name\ndescription: x\n---\n",
        )
        .expect("書けない");

        let found = discover_in(&[root.path().to_path_buf()]);
        assert_eq!(found.skipped.len(), 1, "壊れた skill 1 件が報告されるべき");
        let message = found.skipped[0].to_string();
        assert!(
            message.contains("Bad-Name"),
            "エラーに壊れた skill の名前が含まれていない: {message}"
        );
    }

    #[test]
    fn a_skill_md_that_cannot_be_read_is_reported_not_forgotten() {
        // SKILL.md をディレクトリにしておくと read_to_string は失敗するが、
        // これは NotFound ではない — ファイルという名の何かは存在する、
        // 読めないだけ。パースに一度も到達しないので frontmatter::parse の
        // エラーは手に入らない。それでも理由付きで skipped に載らなければ、
        // 「パースに落ちた skill は報告するがそれ以前に読めなかった skill は
        // 黙って消える」という同じ穴が一歩手前で開いたままになる。
        let root = tempfile::tempdir().expect("一時");
        let bad = root.path().join("unreadable-skill");
        std::fs::create_dir_all(bad.join("SKILL.md")).expect("作れない");

        let found = discover_in(&[root.path().to_path_buf()]);
        assert!(found.skills.is_empty());
        assert_eq!(
            found.skipped.len(),
            1,
            "読めない skill 1 件が報告されるべき"
        );
        assert_eq!(found.skipped[0].dir_name, "unreadable-skill");
        let message = found.skipped[0].to_string();
        assert!(
            message.contains("unreadable-skill"),
            "エラーに読めなかった skill の名前が含まれていない: {message}"
        );
    }

    #[test]
    fn processing_order_within_a_directory_is_sorted_not_creation_order() {
        // 作成順をソート順の逆に近い順にしておく。read_dir が返す順序に
        // たまたま頼っていても気付けないよう、単調でない順で作る。
        let root = tempfile::tempdir().expect("一時");
        put(root.path(), "zeta", "ぜーた", "Z");
        put(root.path(), "mid", "みっど", "M");
        put(root.path(), "alpha", "あるふぁ", "A");

        let found = discover_in(&[root.path().to_path_buf()]);
        let names: Vec<&str> = found.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["alpha", "mid", "zeta"],
            "ディレクトリ内の処理順が名前でソートされていない"
        );
    }
}
