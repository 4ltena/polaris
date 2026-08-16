//! read ツール。行番号を付けて返すのは、モデルが path:line で位置を示せるようにするため。

use std::path::Path;

use crate::{ToolError, path_policy};

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
    let body = std::fs::read_to_string(path)?;
    let mut out = String::new();
    for (i, line) in body.lines().enumerate().skip(offset).take(limit) {
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
}
