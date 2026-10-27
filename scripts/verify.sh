#!/bin/sh
# 全段検証。生成物を先に揃えてから回す。
#
# `docs/architecture.md` と `docs/filemap.md` はソースから作り直して突き合わせる
# 生成物なので、行番号が1つ動いただけで落ちる。2026-09-19 に3回、これだけを
# 理由に全段検証をやり直した。検証の前に揃えるのが正しい順序である。
#
# 順序が効く。`cargo fmt` はソースの行番号を動かすので、**整形してから**
# 生成物を作り直す。逆にすると、再生成した直後に fmt が行をずらして
# `architecture_matches_repository` が落ちる。2026-09-19 にこの script
# 自身がその順序で書かれていて、一度そのとおりに落ちた。
#
# `set -e` は使わない。目的は「どこが落ちたか全部見せる」ことで、最初の失敗で
# 止めると残りの段の状態が分からなくなる。
set -u
cd "$(dirname "$0")/.." || exit 1
: "${CARGO_TARGET_DIR:=$PWD/target}"
export CARGO_TARGET_DIR
fail=0
clippy_failed=0
fresh_test_failure=0

say() { printf '\n=== %s\n' "$1"; }

say "AGENTS.md derived values and truncation"
python3 scripts/sync_agents_md.py --write || fail=1

say "fmt"
cargo fmt --all
cargo fmt --all -- --check || fail=1

say "Regenerate snapshots"
UPDATE_ARCHITECTURE=1 cargo test -q -p polaris-core --test architecture >/dev/null 2>&1 \
  || { echo "architecture regeneration failed"; fail=1; }
UPDATE_FILEMAP=1 cargo test -q -p polaris-core --test filemap >/dev/null 2>&1 \
  || { echo "filemap regeneration failed"; fail=1; }
git --no-pager diff --stat docs/architecture.md docs/filemap.md

say "clippy"
# `clippy_failed` and `fresh_test_failure` are tracked apart on purpose.
# They were one flag until 2026-09-19, and a clippy error then printed
# 「新規failureあり」 under a clean test list — which reads as a test
# regression and sends the next reader to the wrong place.
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -E '^error' && { fail=1; clippy_failed=1; }

say "test"
out=$(mktemp)
cargo test --workspace --no-fail-fast >"$out" 2>&1
python3 - "$out" <<'PY'
import re, sys
t = open(sys.argv[1]).read()
ok = sum(int(m) for m in re.findall(r'^test result: ok\. (\d+) passed', t, re.M))
bad = re.findall(r'^test result: FAILED\. (\d+) passed; (\d+) failed', t, re.M)
names = sorted(set(re.findall(r'^test (\S+) \.\.\. FAILED', t, re.M)))
print(f"passed={ok + sum(int(a) for a, b in bad)}  failed={sum(int(b) for a, b in bad)}")
# 2026-09-19 時点で HEAD でも落ちる既存failure。切り分け済みで、この6件は
# polaris-core / polaris-tools を HEAD へ戻しても同じように落ちる。
known = {
    "native_owner_main_and_recovery_hello_then_eof_release_store",
    "startup::tests::configured_mcp_corruption_stops_startup_but_unsafe_stdio_is_only_quarantined",
    "startup::tests::legacy_owner_ignores_invalid_memory_config_without_creating_memory_resources",
    "startup::tests::mcp_settings_report_startup_revisions_until_the_next_owner_start",
    "startup::tests::owner_reconnect_uses_saved_workflow_even_when_global_config_becomes_invalid",
    "startup::tests::packaged_owner_advertises_real_runs_and_releases_store_after_eof",
}
fresh = [n for n in names if n not in known]
for n in names:
    print(("  known " if n in known else "  new ") + n)
sys.exit(1 if fresh else 0)
PY
[ $? -ne 0 ] && { fail=1; fresh_test_failure=1; }
rm -f "$out"

say "Results"
[ "$fresh_test_failure" -eq 0 ] && echo "Tests: no new failures" || echo "Tests: new failures"
[ "$clippy_failed" -eq 0 ] || echo "clippy: errors"
[ "$fail" -eq 0 ] && echo "All stages passed" || echo "Some stages failed"
exit "$fail"
