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

/// `apply` が失敗した理由の種別。
///
/// 分けるのは、モデルへ返す言葉が正反対になるためである。「置換対象が
/// 見つからない」はごく普通の結果であり、モデルは目印を選び直せばよい。
/// 「OS が拒んだ」は方針の話であり、モデルは書き先を選び直すか承認を
/// 求めるほかない。両方を同じ拒否として返すと、目印を直せば済む場面で
/// モデルが権限の問題を探し始め、往復を1回捨てることになる。
///
/// 種別の判定をここで行うのは、ここにしか材料が無いからである。`apply` は
/// 拘束された子の中で走り、親へ渡せるのは終了状態と標準出力・標準エラー
/// だけで、errno は境界を越えない。`e.to_string()` を作った時点で
/// `ErrorKind` は捨てられており、親側でそれを文字列から復元しようとすれば
/// OS の文言（しかも locale 依存）を当てにすることになる。判定は errno が
/// 手に入るこの場所で行い、結論だけを [`ApplyError::to_wire`] の形で親へ
/// 渡す。
#[derive(Debug)]
pub enum ApplyError {
    /// 要求そのものの問題。ヘルパは走ったが、要求どおりには実行できない
    /// （置換対象が0件・複数件、対象ファイルが無い、等）。
    Request(String),
    /// OS が拒んだ。サンドボックスの方針違反はここへ来る。
    ///
    /// 「拘束下でない場面での、ごく普通の権限エラー」（ルート内の
    /// 読み取り専用ファイル等）もここへ入る。両者は errno では区別が
    /// 付かない。仕様も errno による分類は成立しないと述べている。
    /// ここで拒否側へ倒すのは、`run_mutation` の従来の扱い（非0は
    /// すべて拒否）と同じ保守的な向きであり、新たな見落としを作らない。
    Refused(String),
}

/// 印。要求の問題であることを、拘束された子から親の1本の標準エラー越しに
/// 伝えるためだけに使う。終了コードには何も足さない（子は今までどおり
/// 0 か非0しか返さない）。
const REQUEST_PROBLEM_MARKER: &str = "polaris-helper[要求の問題]: ";

impl ApplyError {
    /// 子が標準エラーへ書く 1 行を組み立てる。印を付けるのは
    /// [`ApplyError::Request`] だけである。印の無い出力は親側で従来どおり
    /// 「拒否」として扱われるので、ヘルパが起動できなかった場合や、想定外の
    /// 経路で落ちた場合の扱いは今までと変わらない。
    pub fn to_wire(&self) -> String {
        match self {
            ApplyError::Request(msg) => format!("{REQUEST_PROBLEM_MARKER}{msg}"),
            ApplyError::Refused(msg) => msg.clone(),
        }
    }

    /// 理由の本文。印は含まない。
    pub fn message(&self) -> &str {
        match self {
            ApplyError::Request(msg) | ApplyError::Refused(msg) => msg,
        }
    }
}

/// 入出力エラーを種別へ振り分ける。
///
/// `ErrorKind::PermissionDenied` は、std が `EPERM` と `EACCES` へ与えて
/// いる移植性のある名前である。Seatbelt（macOS）は `EPERM`、landlock
/// （Linux）は `EACCES` を返すので、サンドボックスの拒否は必ずここへ入る。
/// それ以外（対象が無い、対象がディレクトリ、等）は「要求どおりには実行
/// できない」のであって拒否ではなく、モデルは要求を直せばよい。
fn classify_io(e: std::io::Error) -> ApplyError {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        ApplyError::Refused(e.to_string())
    } else {
        ApplyError::Request(e.to_string())
    }
}

/// 子の標準エラーから、要求の問題としての理由を取り出す。印が無ければ
/// `None`（＝拒否として扱う）。
///
/// 親（`polaris_tools::write::run_mutation`）はこの関数越しにしか印を見ない。
/// 印の文字列を両側へ書き写すと、片方だけ変えても誰も気づけない。
pub fn request_problem(stderr: &str) -> Option<&str> {
    stderr.trim().strip_prefix(REQUEST_PROBLEM_MARKER)
}

/// 変更を実行する。成功なら人間とモデルの双方が読める 1 行を返す。
pub fn apply(m: &Mutation) -> Result<String, ApplyError> {
    match m {
        Mutation::Write { path, content } => {
            // path がシンボリックリンクで、その先が書込可能ルートの外を
            // 指していても、ここでは検証しない。意図的である。apply 自体は
            // パスの検証を一切行わず、実際に止めるのは OS のサンドボックス
            // （拘束された子として起動されること）に全面的に委ねている。
            // ここへ検証を足しても二重の主張になるだけで、外れれば偽の
            // 安心を生む。
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(classify_io)?;
            }
            std::fs::write(path, content).map_err(classify_io)?;
            Ok(format!(
                "{} へ {} バイト書いた",
                path.display(),
                content.len()
            ))
        }
        Mutation::Edit { path, old, new } => {
            let body = std::fs::read_to_string(path).map_err(classify_io)?;
            let hits = count_overlapping(&body, old);
            match hits {
                0 => Err(ApplyError::Request(format!(
                    "{} に置換対象が見つからない",
                    path.display()
                ))),
                1 => {
                    let out = body.replace(old.as_str(), new.as_str());
                    std::fs::write(path, out).map_err(classify_io)?;
                    Ok(format!("{} を 1 箇所置換した", path.display()))
                }
                // どちらを置き換えたのか呼び出し側に分からない置換は成功と
                // して返さない。1 件目だけ黙って置き換えるのが最悪であり、
                // 通ったのに意図と違う結果が残る。
                n => Err(ApplyError::Request(format!(
                    "{} に置換対象が {n} 箇所ある。一意に定まる文字列を渡すこと",
                    path.display()
                ))),
            }
        }
    }
}

/// `needle` が `haystack` に何回現れるかを、重なりを許して数える。
///
/// `str::matches` は非重複の出現しか数えない。`needle = "aa"` を
/// `haystack = "aaa"` に対して数えると、重ならない数え方では 1 になるが、
/// 実際には位置 0 と位置 1 の 2 箇所に出現しており、どちらを置き換えたのか
/// 呼び出し側には分からない。これは複数一致を拒否する仕組みが元々
/// 防ごうとしている事態と同じ種類であり、見逃すと「成功したのに意図と
/// 違う結果が残る」という最悪の形になる。
fn count_overlapping(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut offset = 0;
    while let Some(rel) = haystack[offset..].find(needle) {
        count += 1;
        let hit_start = offset + rel;
        // needle の全長分進めると重なりを見逃す。次の探索は、この一致の
        // 先頭文字の次の文字境界から始める。
        let advance = haystack[hit_start..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(1);
        offset = hit_start + advance;
        if offset > haystack.len() {
            break;
        }
    }
    count
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
        assert!(
            matches!(err, ApplyError::Request(_)),
            "複数一致は要求の問題であって拒否ではない: {err:?}"
        );
        assert!(err.message().contains("2"), "件数が伝わらない: {err:?}");
        assert_eq!(
            std::fs::read_to_string(&target).expect("読めない"),
            "xxx と xxx",
            "拒否したのに書き換わっている"
        );
    }

    #[test]
    fn edit_refuses_when_the_marker_overlaps_itself() {
        // "aa" は "aaa" の中に非重複な数え方ではちょうど1回に見えるが、
        // 実際には位置0と位置1の2箇所に出現する。str::matches はここを
        // 見逃し、1件目（先頭2文字）だけを黙って置き換えてしまう。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let target = dir.path().join("f.txt");
        std::fs::write(&target, "aaa").expect("書けない");

        let err = apply(&Mutation::Edit {
            path: target.clone(),
            old: "aa".into(),
            new: "b".into(),
        })
        .expect_err("重なった一致が通ってしまった");
        assert!(
            matches!(err, ApplyError::Request(_)),
            "重なった一致は要求の問題であって拒否ではない: {err:?}"
        );
        assert!(err.message().contains("2"), "件数が伝わらない: {err:?}");
        assert_eq!(
            std::fs::read_to_string(&target).expect("読めない"),
            "aaa",
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
        assert!(
            matches!(err, ApplyError::Request(_)),
            "不一致は要求の問題であって拒否ではない: {err:?}"
        );
        assert!(err.message().contains("見つからない"), "{err:?}");
    }

    #[test]
    fn an_edit_of_a_file_that_does_not_exist_is_a_request_problem_not_a_refusal() {
        // 対象が無いのは「拘束が拒んだ」ではない。モデルは先に作るか別の
        // パスを指せばよく、方針や書込可能ルートを疑う必要は無い。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let err = apply(&Mutation::Edit {
            path: dir.path().join("no-such-file.txt"),
            old: "xxx".into(),
            new: "yyy".into(),
        })
        .expect_err("存在しないファイルの編集が通ってしまった");
        assert!(
            matches!(err, ApplyError::Request(_)),
            "対象が無いことを拒否として分類している: {err:?}"
        );
    }

    #[test]
    fn only_permission_errors_are_classified_as_a_refusal() {
        // 拒否と要求の問題を分ける唯一の材料が errno であることの固定。
        // EPERM(1) は Seatbelt が、EACCES(13) は landlock が返す。std は
        // 両方を ErrorKind::PermissionDenied へ写像する。ENOENT(2) は
        // どちらでもない通常の失敗であり、拒否として扱ってはならない。
        //
        // 実ファイルの権限で作らないのは、コンテナのテストが root で走ると
        // 0444 でも書けてしまい、テストが環境次第で無意味になるためである。
        assert!(
            matches!(
                classify_io(std::io::Error::from_raw_os_error(1)),
                ApplyError::Refused(_)
            ),
            "EPERM が拒否として分類されていない"
        );
        assert!(
            matches!(
                classify_io(std::io::Error::from_raw_os_error(13)),
                ApplyError::Refused(_)
            ),
            "EACCES が拒否として分類されていない"
        );
        assert!(
            matches!(
                classify_io(std::io::Error::from_raw_os_error(2)),
                ApplyError::Request(_)
            ),
            "ENOENT を拒否として分類している"
        );
    }

    #[test]
    fn the_marker_survives_the_trip_to_the_parent_and_only_marks_request_problems() {
        // 親（run_mutation）が見るのは子の標準エラーの文字列だけである。
        // 印を付けて往復させ、種別が本当に伝わることと、拒否には印が
        // 付かない（＝印が無ければ従来どおり拒否として扱われる）ことを
        // 両方固定する。
        let req = ApplyError::Request("置換対象が見つからない".to_string());
        let wire = req.to_wire();
        assert_eq!(
            request_problem(&wire),
            Some("置換対象が見つからない"),
            "印を付けた理由が親側で取り出せない: {wire}"
        );

        let refused = ApplyError::Refused("Operation not permitted (os error 1)".to_string());
        let wire = refused.to_wire();
        assert_eq!(
            request_problem(&wire),
            None,
            "拒否に印が付いている（要求の問題として扱われてしまう）: {wire}"
        );

        // 子は改行付きで書き出す。親は trim してから見る。
        assert_eq!(
            request_problem(&format!("{}\n", ApplyError::Request("x".into()).to_wire())),
            Some("x")
        );
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
