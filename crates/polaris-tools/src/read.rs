//! read ツール。行番号を付けて返すのは、モデルが path:line で位置を示せるようにするため。

use std::path::Path;

use crate::{ToolError, path_policy};

/// 読み込みを許す最大バイト数。実在するソースファイルには十分すぎるほど
/// 寛容（このリポジトリ最大の `.rs` ファイルでも 35 KB 弱）にしつつ、
/// 事故（デバイスファイルや巨大ファイルの誤指定）で harness が落ちる規模
/// （観測値: `/dev/zero` で 4 秒 2.26 GB）よりは 3 桁小さく抑える。
pub const MAX_READ_BYTES: u64 = 5 * 1024 * 1024;

/// `offset` は 0 起点の行番号、`limit` は返す行数。出力の行番号は 1 起点。
pub fn read(path: &Path, offset: usize, limit: usize) -> Result<String, ToolError> {
    if path_policy::is_denied(path) {
        return Err(ToolError::PathDenied(path.display().to_string()));
    }
    // シンボリックリンクは `is_denied` の文字列比較をすり抜ける。リンク先を
    // 解決し、そちらにも同じ判定をかけて塞ぐ。存在しないパスは正規化に失敗するが、
    // それは「拒否」ではなく通常の I/O エラーとして扱う。タイポしたパスを
    // 「秘密ファイルだから読めない」と誤報すると、モデルを誤った方向へ導く。
    if let Ok(real) = std::fs::canonicalize(path)
        && path_policy::is_denied(&real)
    {
        return Err(ToolError::PathDenied(path.display().to_string()));
    }

    // `read_to_string` はサイズも種別も見ずに全部メモリへ載せる。デバイス
    // ファイル（`/dev/zero` 等）や FIFO を指されると、読み取りが終わらない
    // まま常駐メモリが際限なく増える。メタデータで種別とサイズを先に見て、
    // 通常ファイルでも上限超なら中身を一切読まずに拒否する。
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        return Err(ToolError::NotAFile(path.display().to_string()));
    }
    if meta.len() > MAX_READ_BYTES {
        return Err(ToolError::TooLarge {
            path: path.display().to_string(),
            limit: MAX_READ_BYTES,
            actual: meta.len(),
        });
    }

    let body = std::fs::read_to_string(path)?;

    // 空ファイル・limit=0・offset が全行数を超える、の3つはどれも
    // 「対象行が1つも無い」という点で同じだが、モデルにとっての意味は
    // まったく違う。すべて `Ok("")` に潰すと、offset を EOF の先へ動かした
    // だけの呼び出しが「ファイルは空だった」に見えてしまう —
    // 成功の皮をかぶった失敗の中で、この milestone が最も繰り返した形。
    if body.is_empty() {
        return Ok("(空ファイル)".to_string());
    }

    let lines: Vec<&str> = body.lines().collect();
    let total = lines.len();

    if limit == 0 {
        return Ok("(0 行: limit が 0)".to_string());
    }
    if offset >= total {
        return Ok(format!(
            "(0 行: offset {offset} は全 {total} 行を超えている)"
        ));
    }

    let mut out = String::new();
    for (i, line) in lines.iter().enumerate().skip(offset).take(limit) {
        out.push_str(&format!("{}\t{}\n", i + 1, line));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("一時ファイルを作れない");
        for l in lines {
            writeln!(f, "{l}").expect("書き込めない");
        }
        f.flush().expect("flush できない");
        f
    }

    #[test]
    fn numbers_lines_from_one() {
        let f = fixture(&["alpha", "beta"]);
        let out = read(f.path(), 0, 100).expect("読めない");
        assert_eq!(out, "1\talpha\n2\tbeta\n");
    }

    #[test]
    fn honors_offset_and_limit() {
        let f = fixture(&["a", "b", "c", "d"]);
        let out = read(f.path(), 1, 2).expect("読めない");
        assert_eq!(out, "2\tb\n3\tc\n");
    }

    #[test]
    fn refuses_denied_paths() {
        let err =
            read(std::path::Path::new("/home/u/.ssh/id_rsa"), 0, 100).expect_err("拒否されるべき");
        assert!(matches!(err, ToolError::PathDenied(_)));
    }

    #[test]
    fn refuses_symlink_to_denied_target() {
        // 拒否ディレクトリを模した実体を作り、そこへのシンボリックリンクを
        // 別の場所（一見無害なファイル名）に置く。`is_denied` はパス文字列しか
        // 見ないため、リンクを辿って実体を解決してから判定しないと素通りする。
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        let ssh_dir = dir.path().join(".ssh");
        std::fs::create_dir(&ssh_dir).expect("ディレクトリを作れない");
        let target = ssh_dir.join("id_rsa");
        std::fs::write(&target, "secret").expect("書き込めない");

        let link = dir.path().join("notes.txt");
        std::os::unix::fs::symlink(&target, &link).expect("symlink を作れない");

        let err = read(&link, 0, 100).expect_err("拒否されるべき");
        assert!(matches!(err, ToolError::PathDenied(_)));
    }

    #[test]
    fn missing_file_is_io_error_not_denied() {
        // 存在しないファイルは正規化(canonicalize)に失敗する。それを拒否だと
        // 誤報すると、モデルはタイポを「秘密ファイルだから読めない」と誤解する。
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        let missing = dir.path().join("does-not-exist.txt");

        let err = read(&missing, 0, 100).expect_err("エラーになるべき");
        assert!(matches!(err, ToolError::Io(_)));
    }

    #[test]
    fn refuses_non_regular_files_like_directories() {
        // ディレクトリは `is_file()` が false になる、`/dev/zero` のような
        // デバイスファイルと同じ「通常ファイルではない」経路。OS を問わず
        // 再現できるので、これを主たる回帰テストにする。
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        let err = read(dir.path(), 0, 100).expect_err("拒否されるべき");
        assert!(matches!(err, ToolError::NotAFile(_)));
    }

    #[test]
    #[cfg(unix)]
    fn refuses_device_files() {
        // レビューがメモリを 2.26 GB まで食わせた実際の経路。メタデータ段階で
        // 弾くので、この呼び出しは中身を一切読まずに即座にエラーへ帰る。
        let err = read(std::path::Path::new("/dev/zero"), 0, 100).expect_err("拒否されるべき");
        assert!(matches!(err, ToolError::NotAFile(_)));
    }

    #[test]
    fn refuses_files_over_the_size_ceiling() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        let path = dir.path().join("huge.txt");
        let f = std::fs::File::create(&path).expect("作れない");
        f.set_len(MAX_READ_BYTES + 1).expect("サイズを設定できない");

        let err = read(&path, 0, 100).expect_err("拒否されるべき");
        match err {
            ToolError::TooLarge {
                limit,
                actual,
                path: p,
            } => {
                assert_eq!(limit, MAX_READ_BYTES);
                assert_eq!(actual, MAX_READ_BYTES + 1);
                assert!(p.contains("huge.txt"));
            }
            other => panic!("TooLarge を期待したが {other:?} だった"),
        }
    }

    #[test]
    fn files_at_the_size_ceiling_are_still_allowed() {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        let path = dir.path().join("at_limit.txt");
        std::fs::write(&path, "1行だけ\n").expect("書き込めない");

        let out = read(&path, 0, 100).expect("上限未満なので読めるべき");
        assert_eq!(out, "1\t1行だけ\n");
    }

    #[test]
    fn empty_file_says_so_instead_of_looking_like_a_match() {
        let f = tempfile::NamedTempFile::new().expect("一時ファイルを作れない");
        let out = read(f.path(), 0, 100).expect("読めるべき");
        assert_eq!(out, "(空ファイル)");
    }

    #[test]
    fn limit_zero_says_so_instead_of_looking_empty() {
        let f = fixture(&["a", "b", "c"]);
        let out = read(f.path(), 0, 0).expect("読めるべき");
        assert_eq!(out, "(0 行: limit が 0)");
    }

    #[test]
    fn offset_past_end_of_file_says_so_instead_of_looking_empty() {
        let f = fixture(&(0..10).map(|_| "line").collect::<Vec<_>>());
        let out = read(f.path(), 5000, 100).expect("読めるべき");
        assert_eq!(out, "(0 行: offset 5000 は全 10 行を超えている)");
    }

    #[test]
    fn offset_exactly_at_line_count_is_also_past_end() {
        // offset は 0 起点。ちょうど行数と同じ offset は「最終行の次」であり、
        // 境界を1つ間違えると最終行が消えるか、逆に1行余分に範囲外を許す。
        let f = fixture(&["a", "b", "c"]);
        let out = read(f.path(), 3, 100).expect("読めるべき");
        assert_eq!(out, "(0 行: offset 3 は全 3 行を超えている)");
    }
}
