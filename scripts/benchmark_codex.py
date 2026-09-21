#!/usr/bin/env python3
"""純正Codex CLIを、polaris側と同じ課題・同じfixtureで実測する。

`benchmark_efficiency.py`の対照腕である。課題文・fixture・採点は
あちらから読み込み、**片方だけ書き換えると比較が崩れる状態を作らない。**

腕を揃えるためにしていること。

- `--ignore-user-config`。`~/.codex/config.toml`にはモデル・推論強度・
  MCPサーバーが書いてあり、それは**公開された既定値ではなく利用者の設定**
  である。2026-09-18の測定では`-c 'mcp_servers={}'`が効かず、HTTP型MCP
  3本が毎turn起動して認証失敗していた。フラグで丸ごと外す。
- fixtureへ0バイトの`AGENTS.md`を置く。無いと探索が上位ディレクトリへ
  抜ける。
- 使用量は**セッションのrollout全件**から数える。`turn.completed`は本
  スレッドしか報告せず、前回は子スレッド2本ぶん（11要求・入力490,297）を
  落としていた。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
import benchmark_efficiency as bench  # noqa: E402


def rollouts_for(home: Path, started: float, fixture: Path) -> list[Path]:
    """`started`以降に書かれ、この fixture を cwd とする rollout 全件。

    時刻だけで選ばない。**住所で選ぶ。** `turn.completed`は本スレッドしか
    報告せず、2026-09-18の測定は子スレッド2本（11要求・入力490,297）を
    落として1.87倍の過小計上になった。`session_meta`と`turn_context`は
    どちらも`cwd`を持つので、親子を問わずここで拾える。

    時刻は前回の run の残骸を除くためだけに使う。
    """
    root = home / "sessions"
    if not root.is_dir():
        return []
    target = str(fixture.resolve())
    held = []
    for path in sorted(root.rglob("rollout-*.jsonl")):
        if not path.is_file() or path.stat().st_mtime < started:
            continue
        for line in path.read_text(errors="replace").splitlines():
            try:
                payload = (json.loads(line) or {}).get("payload") or {}
            except ValueError:
                continue
            if isinstance(payload, dict) and payload.get("cwd") == target:
                held.append(path)
                break
    return held


def wait_for_rollouts(home: Path, started: float, fixture: Path,
                      deadline_seconds: float = 60.0) -> list[Path]:
    """rolloutが出そろうまで待つ。

    codexはプロセスが終わったあとに書き終える。終了直後に読むと**1件も
    見つからない**——2026-09-19のパイロットで3 runとも要求0件になった。
    幸い0件は部分的な取りこぼしと区別が付くが、待たない理由にはならない。

    現れるまで待ち、さらに**大きさが止まるまで**待つ。書き終える前に
    読むと、最後のturnの`token_count`を落とした合計になる。
    """
    import time as _t
    stop = _t.monotonic() + deadline_seconds
    stable: dict[str, int] = {}
    while _t.monotonic() < stop:
        found = rollouts_for(home, started, fixture)
        if found:
            sizes = {str(p): p.stat().st_size for p in found}
            if sizes == stable:
                return found
            stable = sizes
        _t.sleep(1.0)
    return rollouts_for(home, started, fixture)


def usage_from(paths: list[Path]) -> dict:
    """rolloutの各行からトークンを拾う。

    **欠測を0として足さない。** 数えられなかった行は`missing`に積み、
    合計とは別に持つ。0と「分からない」を混ぜた瞬間、その合計は費用の
    比較に使えなくなる。

    足すのは`last_token_usage`（要求ごと）であって
    `total_token_usage`（累計）ではない。後者を足すと二重に数える。
    """
    totals = {"input": 0, "output": 0, "cache": 0, "cache_write": 0, "reasoning": 0}
    requests = 0
    missing = 0
    models: set[tuple[str | None, str | None]] = set()
    for path in paths:
        for line in path.read_text(errors="replace").splitlines():
            try:
                event = json.loads(line)
            except ValueError:
                continue
            payload = event.get("payload") if isinstance(event, dict) else None
            if not isinstance(payload, dict):
                continue
            if payload.get("model") or payload.get("effort"):
                models.add((payload.get("model"), payload.get("effort")))
            info = payload.get("info")
            usage = info.get("last_token_usage") if isinstance(info, dict) else None
            if not isinstance(usage, dict):
                continue
            requests += 1
            got = False
            for key, name in (
                ("input", "input_tokens"),
                ("output", "output_tokens"),
                ("cache", "cached_input_tokens"),
                ("cache_write", "cache_write_input_tokens"),
                ("reasoning", "reasoning_output_tokens"),
            ):
                if isinstance(usage.get(name), int):
                    totals[key] += usage[name]
                    got = True
            if not got:
                missing += 1
    # 課金されるのはキャッシュに当たらなかった分である。
    totals["uncached_input"] = totals["input"] - totals["cache"]
    return {
        "requests": requests,
        "missing": missing,
        "totals": totals,
        "observed_model_effort": sorted([list(pair) for pair in models]),
        "rollouts": [path.name for path in paths],
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--label", required=True)
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--tasks", nargs="+", choices=list(bench.PROMPTS), default=list(bench.PROMPTS))
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--codex", default="codex")
    args = ap.parse_args()
    if not 1 <= args.repeats <= 10 or not 1 <= args.timeout <= 900:
        ap.error("repeatsは1〜10、timeoutは1〜900秒")

    codex = shutil.which(args.codex)
    if codex is None:
        ap.error(f"{args.codex} が見つかりません")
    version = subprocess.run([codex, "--version"], capture_output=True, text=True).stdout.strip()
    home = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex"))

    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    fixture = output / "fixture"
    (output / "metadata.json").write_text(
        json.dumps(
            {
                "arm": "stock-codex",
                "label": args.label,
                "codex_version": version,
                "codex_home": str(home),
                "requested": "published defaults (--ignore-user-config)",
                "agents_md": "empty",
                "prompts": {task: bench.task_prompt(task, "default") for task in args.tasks},
                "fixture_sha256": hashlib.sha256(
                    (bench.README + bench.RATES + bench.HISTORY).encode()
                ).hexdigest(),
                "repeats": args.repeats,
            },
            ensure_ascii=False,
            indent=2,
        )
    )

    rows = []
    for task in args.tasks:
        for repeat in range(args.repeats):
            fixture.mkdir(mode=0o700)
            for name, body in (
                ("README.md", bench.README),
                ("rates.py", bench.RATES),
                ("history.md", bench.HISTORY),
            ):
                (fixture / name).write_text(body)
            (fixture / "AGENTS.md").write_text("")
            before = bench.snapshot(fixture)
            stem = f"{task}-{repeat + 1}"
            last = output / f"{stem}.last.txt"
            sandbox = "workspace-write" if task == "repair" else "read-only"
            cmd = [
                codex, "exec",
                "--ignore-user-config",
                "--skip-git-repo-check",
                "--sandbox", sandbox,
                "-C", str(fixture),
                "-o", str(last),
                bench.task_prompt(task, "default"),
            ]
            started = time.time()
            clock = time.monotonic()
            try:
                result = subprocess.run(
                    cmd, capture_output=True, text=True,
                    timeout=args.timeout, stdin=subprocess.DEVNULL,
                )
                stdout, stderr, code = result.stdout, result.stderr, result.returncode
            except subprocess.TimeoutExpired as exc:
                stdout = (exc.stdout or b"")
                stderr = (exc.stderr or b"")
                stdout = stdout.decode(errors="replace") if isinstance(stdout, bytes) else stdout
                stderr = stderr.decode(errors="replace") if isinstance(stderr, bytes) else stderr
                code = None
            (output / f"{stem}.out").write_text(stdout)
            (output / f"{stem}.err").write_text(stderr)
            answer = last.read_text() if last.exists() else stdout
            after = bench.snapshot(fixture)
            changed = sorted(k for k in before.keys() | after.keys() if before.get(k) != after.get(k))
            scope_ok = all(k == "rates.py" for k in changed) if task == "repair" else not changed
            measured = usage_from(wait_for_rollouts(home, started, fixture))
            rows.append(
                {
                    "arm": "stock-codex",
                    "task": task,
                    "repeat": repeat + 1,
                    "exit_code": code,
                    "elapsed_seconds": round(time.monotonic() - clock, 3),
                    "quality_pass": code == 0 and scope_ok and bench.quality(task, answer, fixture),
                    "scope_pass": scope_ok,
                    "changed_paths": changed,
                    **measured,
                    # 欠測が1件でもあれば合計は費用比較に使えない。
                    "cost_comparison_ready": code == 0
                    and measured["requests"] > 0
                    and measured["missing"] == 0,
                }
            )
            shutil.rmtree(fixture)
    (output / "rows.jsonl").write_text(
        "".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows)
    )
    ready = sum(1 for row in rows if row["cost_comparison_ready"])
    passed = sum(1 for row in rows if row["quality_pass"])
    print(f"{len(rows)} 件: 費用比較可 {ready} / 正答 {passed}")
    return 0 if ready == len(rows) else 1


if __name__ == "__main__":
    raise SystemExit(main())
