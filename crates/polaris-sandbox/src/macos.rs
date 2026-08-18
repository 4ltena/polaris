//! macOS の強制。実行時に Seatbelt プロファイルを組み立て、
//! `/usr/bin/sandbox-exec` へ渡して子を起動する。
//!
//! パスは本文へ埋め込まず `-D key=value` と `(param "KEY")` で渡す。
//! 空白や括弧を含むパスで SBPL の引用規則を踏まないためである。

use std::path::Path;

use crate::policy::{SandboxMode, SandboxPolicy};

/// `PATH` を引かない。`PATH` 上の同名バイナリで差し替えられる経路を塞ぐ。
/// この実体そのものが改竄されている状況では、攻撃者はすでに root を持つ。
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// 方針から SBPL のプロファイル本文を組み立てる。
pub fn build_profile(policy: &SandboxPolicy) -> String {
    let mut p = String::from("(version 1)\n");

    if policy.mode() == SandboxMode::FullAccess {
        // 境界は越えるが制限しない。プロファイルを作らない分岐にしないのは、
        // 試験する経路と本番の経路を同一に保つためである。
        p.push_str("(allow default)\n");
        return p;
    }

    p.push_str("(deny default)\n");
    // シェルがシェルとして振る舞うために要る最低限。
    p.push_str("(allow process-fork)\n");
    p.push_str("(allow process-exec)\n");
    p.push_str("(allow signal (target same-sandbox))\n");
    if policy.mode() == SandboxMode::ReadOnly {
        p.push_str("(allow file-read* file-ioctl (literal \"/dev/ptmx\"))\n");
    } else {
        p.push_str("(allow file-read* file-write* file-ioctl (literal \"/dev/ptmx\"))\n");
    }
    p.push_str("(allow file-ioctl (regex #\"^/dev/ttys[0-9]+\"))\n");
    p.push_str("(allow file-read*)\n");
    // Rust のランタイムが起動時に要る。main スレッドのガードページを張る
    // 前に `sysconf(_SC_PAGESIZE)` を引き、macOS ではこれが sysctl
    // （`hw.pagesize_compat`）へ落ちる。拒否するとページ長が取れず、
    // 続く mmap が EINVAL で失敗して
    // 「failed to allocate a guard page: Invalid argument (os error 22)」
    // → `fatal runtime error` → SIGABRT となる。これは *こちらのコードが
    // 1 行も走る前* に起きるので、`--confined-apply` ヘルパは read-only と
    // workspace-write の両方で必ず落ち、その abort が方針違反による拒否と
    // 同じ形（非0終了＋stderr）で親へ届いていた。実測で確認した
    // （`polaris-cli/tests/confined_helper.rs` が本物のバイナリと本物の
    // プロファイルで固定している）。
    //
    // なぜ絞り込まないか。実測では
    // `(allow sysctl-read (sysctl-name "hw.pagesize" "hw.pagesize_compat"))`
    // でもヘルパは起動する（`hw.pagesize` だけでは足りない）。それでも
    // 名前で絞らないのは、このプロファイルがヘルパ専用ではないためである。
    // `bash` ツールが起動する任意の子も同じプロファイルの下で走り、機械の
    // 諸元（`hw.ncpu`、`hw.memsize`、`kern.osversion` 等）を引くものは
    // 珍しくない。2 件だけを許すと、ヘルパは直るが `bash` から起動した
    // Rust バイナリや多くのランタイムが同じ様態で落ち続ける（現に、この
    // 行が無い状態では `sh -c 'polaris --help'` すら abort する）。
    //
    // 境界を広げないと言える理由。`sysctl-read` は機械の諸元の読み取り
    // だけであり、書き込みの権限を一切与えない。このプロファイルが守って
    // いるのは書き込みの境界であって、読み取りはすでに直上の
    // `(allow file-read*)` でファイルシステム全体に開いている。
    p.push_str("(allow sysctl-read)\n");

    let roots = policy.writable_roots();
    if !roots.is_empty() {
        p.push_str("(allow file-write*\n");
        for i in 0..roots.len() {
            p.push_str(&format!("  (subpath (param \"WRITABLE_ROOT_{i}\"))\n"));
        }
        p.push_str(")\n");
    }

    p
}

/// `sandbox-exec` へ渡す argv を組み立てる。プログラム本体と引数は
/// `--` の後ろへ置き、境界を曖昧にしない。
pub fn build_args(policy: &SandboxPolicy, program: &Path, args: &[String]) -> Vec<String> {
    let mut out = vec!["-p".to_string(), build_profile(policy)];
    for (i, root) in policy.writable_roots().iter().enumerate() {
        out.push(format!("-DWRITABLE_ROOT_{i}={}", root.display()));
    }
    out.push("--".to_string());
    out.push(program.display().to_string());
    out.extend(args.iter().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{SandboxMode, SandboxPolicy};

    fn workspace(roots: &[std::path::PathBuf]) -> SandboxPolicy {
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, roots).expect("方針を作れない")
    }

    #[test]
    fn the_profile_starts_closed() {
        // deny default が無ければ、以降の allow は「既定で全許可の上に
        // 少し足す」ことになり、方針の意味が反転する。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let p = build_profile(&workspace(&[dir.path().to_path_buf()]));
        assert!(p.contains("(deny default)"), "既定拒否が無い:\n{p}");
    }

    #[test]
    fn every_writable_root_gets_its_own_parameterised_subpath() {
        // ルートを 1 件でも落とすと、書けるはずの場所が黙って減る。
        let a = tempfile::tempdir().expect("一時ディレクトリ");
        let b = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let p = build_profile(&policy);

        assert!(p.contains("(subpath (param \"WRITABLE_ROOT_0\"))"), "{p}");
        assert!(p.contains("(subpath (param \"WRITABLE_ROOT_1\"))"), "{p}");
        assert_eq!(
            p.matches("WRITABLE_ROOT_").count(),
            2,
            "ルート数とパラメータ数が一致しない:\n{p}"
        );
    }

    #[test]
    fn paths_never_appear_verbatim_in_the_profile_body() {
        // パスを本文へ直接書くと、空白や括弧を含むパスで SBPL が壊れる。
        // 値は必ず -D 側へ渡す。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(&[dir.path().to_path_buf()]);
        let p = build_profile(&policy);
        let root = policy.writable_roots()[0].display().to_string();
        assert!(!p.contains(&root), "パスが本文に埋め込まれている:\n{p}");
    }

    #[test]
    fn read_only_grants_no_write_at_all() {
        // read-only でルートは持てない（Task 1 で拒否される）。ここで見るのは
        // 書き込み許可の節そのものが出ないこと。空のルート一覧に対して
        // (allow file-write*) だけが裸で残ると、全書き込みが許可される。
        let policy = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("方針");
        let p = build_profile(&policy);
        assert!(!p.contains("file-write*"), "書き込み許可がある:\n{p}");
        assert!(
            p.contains("(allow file-read*)"),
            "読み取りが許可されていない:\n{p}"
        );
    }

    #[test]
    fn full_access_still_produces_a_profile_so_the_path_is_the_same_one_we_test() {
        // full-access でも境界を越える。プロファイルを作らない分岐を設けると、
        // 試験する経路と本番の経路が別物になる。
        let policy = SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("方針");
        let p = build_profile(&policy);
        assert!(p.starts_with("(version 1)"), "{p}");
        assert!(p.contains("(allow default)"), "{p}");
    }

    #[test]
    fn args_pass_each_root_as_a_d_parameter_and_separate_the_command_with_dashdash() {
        let a = tempfile::tempdir().expect("一時ディレクトリ");
        let b = tempfile::tempdir().expect("一時ディレクトリ");
        let policy = workspace(&[a.path().to_path_buf(), b.path().to_path_buf()]);

        let args = build_args(
            &policy,
            std::path::Path::new("/bin/echo"),
            &["hello".to_string()],
        );

        assert_eq!(args[0], "-p", "プロファイルの指定が先頭でない: {args:?}");
        let root0 = policy.writable_roots()[0].display();
        assert!(
            args.iter()
                .any(|a| a == &format!("-DWRITABLE_ROOT_0={root0}")),
            "ルート 0 が -D で渡っていない: {args:?}"
        );
        let sep = args
            .iter()
            .position(|a| a == "--")
            .expect("-- が無い: コマンドと引数の境界が曖昧になる");
        assert_eq!(args[sep + 1], "/bin/echo");
        assert_eq!(args[sep + 2], "hello");
    }
}
