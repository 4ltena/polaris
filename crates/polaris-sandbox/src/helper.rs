//! 拘束された子の中で実行する変更操作。
//!
//! 親から子へは JSON 1 件で渡る。シェルを挟まないのは、パスや内容に含まれる
//! 引用符と改行で壊れる経路を作らないためである。

use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Mutation {
    Write {
        path: PathBuf,
        content: String,
    },
    Edit {
        path: PathBuf,
        old: String,
        new: String,
    },
}

/// 変更を実行する。成功なら人間とモデルの双方が読める 1 行を返す。
pub fn apply(m: &Mutation) -> Result<String, String> {
    match m {
        Mutation::Write { path, content } => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(path, content).map_err(|e| e.to_string())?;
            Ok(format!(
                "{} へ {} バイト書いた",
                path.display(),
                content.len()
            ))
        }
        Mutation::Edit { path, old, new } => {
            let body = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let hits = body.matches(old.as_str()).count();
            match hits {
                0 => Err(format!("{} に置換対象が見つからない", path.display())),
                1 => {
                    let out = body.replace(old.as_str(), new.as_str());
                    std::fs::write(path, out).map_err(|e| e.to_string())?;
                    Ok(format!("{} を 1 箇所置換した", path.display()))
                }
                // どちらを置き換えたのか呼び出し側に分からない置換は成功と
                // して返さない。1 件目だけ黙って置き換えるのが最悪であり、
                // 通ったのに意図と違う結果が残る。
                n => Err(format!(
                    "{} に置換対象が {n} 箇所ある。一意に定まる文字列を渡すこと",
                    path.display()
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_creates_the_file_and_its_parents() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let target = dir.path().join("a/b/c.txt");
        let m = Mutation::Write {
            path: target.clone(),
            content: "本文".into(),
        };
        apply(&m).expect("失敗した");
        assert_eq!(std::fs::read_to_string(&target).expect("読めない"), "本文");
    }

    #[test]
    fn edit_replaces_exactly_one_occurrence() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let target = dir.path().join("f.txt");
        std::fs::write(&target, "前 xxx 後").expect("書けない");

        apply(&Mutation::Edit {
            path: target.clone(),
            old: "xxx".into(),
            new: "yyy".into(),
        })
        .expect("失敗した");

        assert_eq!(
            std::fs::read_to_string(&target).expect("読めない"),
            "前 yyy 後"
        );
    }

    #[test]
    fn edit_refuses_when_the_marker_appears_more_than_once() {
        // どちらを置き換えたのかモデルに分からない置換は、成功として
        // 返してはならない。1 件目だけ黙って置き換えるのが最悪であり、
        // 通ったのに意図と違う結果が残る。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let target = dir.path().join("f.txt");
        std::fs::write(&target, "xxx と xxx").expect("書けない");

        let err = apply(&Mutation::Edit {
            path: target.clone(),
            old: "xxx".into(),
            new: "yyy".into(),
        })
        .expect_err("複数一致が通ってしまった");
        assert!(err.contains("2"), "件数が伝わらない: {err}");
        assert_eq!(
            std::fs::read_to_string(&target).expect("読めない"),
            "xxx と xxx",
            "拒否したのに書き換わっている"
        );
    }

    #[test]
    fn edit_refuses_when_the_marker_is_absent() {
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let target = dir.path().join("f.txt");
        std::fs::write(&target, "何も無い").expect("書けない");

        let err = apply(&Mutation::Edit {
            path: target,
            old: "xxx".into(),
            new: "yyy".into(),
        })
        .expect_err("不一致が通ってしまった");
        assert!(err.contains("見つからない"), "{err}");
    }

    #[test]
    fn a_mutation_round_trips_through_json() {
        // ヘルパへは JSON 1 行で渡る。往復で壊れると変更操作が成立しない。
        let m = Mutation::Write {
            path: "/a/b".into(),
            content: "改行\nと \"引用符\"".into(),
        };
        let s = serde_json::to_string(&m).expect("直列化");
        let back: Mutation = serde_json::from_str(&s).expect("復元");
        match back {
            Mutation::Write { path, content } => {
                assert_eq!(path, std::path::PathBuf::from("/a/b"));
                assert_eq!(content, "改行\nと \"引用符\"");
            }
            other => panic!("別の変種になった: {other:?}"),
        }
    }
}
