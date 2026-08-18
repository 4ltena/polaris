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
    // `/dev/null` への書き込みだけを開ける。`cmd > /dev/null` と
    // `cmd 2>/dev/null` はシェルの常套句であり、リダイレクトが開けないと
    // シェルは本体を一度も実行せずに落ちる（`ls / >/dev/null && echo ok`
    // で何も走らないことを実測した）。しかもその失敗は
    // 「方針 workspace-write（書込可能: <root>）」を名指しする形でモデルへ
    // 届くので、モデルは書込可能ルートの側を疑い、パスを変えて同じ失敗を
    // 何度でも繰り返す。仕様の「原因を掴めない拒否メッセージは同じ失敗の
    // 反復を招き、時間とトークンを消費する」に当たる。
    //
    // 実測（本物のプロファイルを `/usr/bin/sandbox-exec` へ直接渡した）:
    //
    // | 追加する許可 | `> /dev/null` | ルート内 | ルート外 | `rm /dev/null` |
    // | 無し（従来） | 拒否 | 可 | 拒否 | 拒否 |
    // | `file-write-data (literal "/dev/null")` | 可 | 可 | 拒否 | 拒否 |
    // | `file-write* (literal "/dev/null")` | 可 | 可 | 拒否 | 拒否 |
    //
    // `file-write-create` だけ、`file-write-mode` だけではどちらも
    // `Operation not permitted` のままで開けない。`file-write-data` が
    // リダイレクトを通す最小の権利であり、`file-write*` と違って unlink も
    // setattr も与えない。対象は `(literal "/dev/null")` の 1 個だけで、
    // `/dev/zero`、`/dev/stdout`、`/dev/stderr`、`/dev/fd/N`、`/dev` 配下の
    // 新規作成がいずれも拒否のままであることも同じ実測で確認した。
    // `/dev/stdout` などを開けるかは fdesc 越しの再判定という別の測定を
    // 要するため、ここでは意図的に触れない（M3 の課題）。
    //
    // read-only でもこの 1 行を出す。`/dev/null` への書き込みはカーネルが
    // 捨てるだけでファイルシステムの状態を一切変えないので、read-only が
    // 守っている性質（何も変更されない）は減らない。実際に read-only の
    // 下で普通のファイルへの書き込みが拒否されたままであることは
    // `confine.rs` の実サンドボックステストが対で見ている。ここで分岐を
    // 設けると、`--sandbox read-only` の `bash` だけが同じ誤解を招く拒否を
    // 出し続けることになる。
    p.push_str("(allow file-write-data (literal \"/dev/null\"))\n");

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
    fn read_only_grants_no_write_to_any_file_beyond_the_dev_null_sink() {
        // read-only でルートは持てない（Task 1 で拒否される）。ここで見るのは
        // 書き込み許可の節そのものが出ないこと。空のルート一覧に対して
        // (allow file-write*) だけが裸で残ると、全書き込みが許可される。
        //
        // 例外は `/dev/null` の 1 行だけである。書いた内容をカーネルが捨てる
        // だけでファイルシステムの状態を変えないため read-only の性質を
        // 減らさない。行を数える形で固定するのは、「書き込みに触れる許可が
        // ここに増えていない」ことが read-only の中身そのものだからである。
        // `!contains("file-write*")` だけでは `file-write-data` を使った
        // 追加の許可が黙って通る。
        let policy = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("方針");
        let p = build_profile(&policy);
        assert!(!p.contains("file-write*"), "書き込み許可がある:\n{p}");
        let writes: Vec<&str> = p.lines().filter(|l| l.contains("file-write")).collect();
        assert_eq!(
            writes,
            vec!["(allow file-write-data (literal \"/dev/null\"))"],
            "read-only に /dev/null 以外の書き込み許可がある:\n{p}"
        );
        assert!(
            p.contains("(allow file-read*)"),
            "読み取りが許可されていない:\n{p}"
        );
    }

    #[test]
    fn the_restrictive_profile_keeps_the_sysctl_read_grant() {
        // この 1 行が消えると、Rust のランタイムは main の前段で
        // `sysconf(_SC_PAGESIZE)` を引けず、ガードページの mmap が EINVAL で
        // 失敗して SIGABRT する。つまり本物のヘルパが起動できなくなり、その
        // 中断は方針違反による拒否と同じ形（非0終了 + stderr）でモデルへ届く。
        // 本命の検出は本物のバイナリを使う `polaris-cli/tests/confined_helper.rs`
        // だが、事故による削除をユニットの段で即座に落とす。
        let dir = tempfile::tempdir().expect("一時ディレクトリ");
        let ws = build_profile(&workspace(&[dir.path().to_path_buf()]));
        assert!(
            ws.contains("(allow sysctl-read)"),
            "workspace-write に sysctl-read が無い:\n{ws}"
        );

        let ro = build_profile(&SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("方針"));
        assert!(
            ro.contains("(allow sysctl-read)"),
            "read-only に sysctl-read が無い:\n{ro}"
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
