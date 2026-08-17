//! skill の探索。1 件の破損が全体を巻き込まないよう、読めないものは飛ばす。
//! ただし読み捨てはしない。飛ばした skill とその理由は `Discovered::skipped`
//! として呼び出し側へ返す。呼び出し側（将来の CLI）がそれを表示する。

use std::path::{Path, PathBuf};

use crate::frontmatter;
use crate::{Skill, SkillError};

/// `discover`/`discover_in` の結果。読み込めた skill と、読み込めずに飛ばした
/// skill のエラーを両方運ぶ。エラー自身が壊れた skill の識別子（ディレクトリ名
/// や name）を保持しているため、ここでは `SkillError` をそのまま運ぶだけで
/// 十分である。
#[derive(Debug, Default)]
pub struct Discovered {
    pub skills: Vec<Skill>,
    pub skipped: Vec<SkillError>,
}

/// 与えられたディレクトリ群を順に走査する。名前が衝突したら先に見つけたものを
/// 採る。読めない・検証に落ちた skill は `skipped` に理由付きで積んで、探索
/// 自体は続ける。
pub fn discover_in(dirs: &[PathBuf]) -> Discovered {
    let mut skills: Vec<Skill> = Vec::new();
    let mut skipped: Vec<SkillError> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let manifest = path.join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                continue;
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
                Err(err) => skipped.push(err),
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
}
