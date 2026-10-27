#!/usr/bin/env python3
"""polarisと純正codexを、台本化した多ターン会話で比較する。

`benchmark_scale.py` は1プロンプト1プロセスで、ブレストも承認のやり取りも
測れない。ここは**同じ台本を両腕へ流す**。台本はターン番号で固定しており、
相手の文面で分岐しない。分岐させると、腕ごとに違う会話を測ってしまう。

3つの型がある。

- `analyze` 既存システムの読解。1ターン。fixtureと採点は `benchmark_scale`
  のものをそのまま使う。
- `small` 小規模実装。ブレスト3ターンのあと実装1ターン。実装対象と採点器は
  `benchmark_scale` の `build` を使う。
- `todo` todo web app。ブレスト10ターン、計画、**却下、再送の確認、承認**、
  実装、レビューの15ターン。fixtureと採点はここで新しく作る。

**却下は文面ではなくJSONの鍵で採点する。** 「計画を直したか」を語彙で見ると、
正しく直した回答を言い回しで落とす。2026-09-19のパイロットで実際に起きた。
そこで計画は末尾JSONで出させ、1回目は `plan_id` と `steps` だけを求め、
却下で `schema_sql`・`error_table`・`tests` の3つを追加で要求する。再送が
その3鍵を持ち、かつ内容が1回目と違うことだけを見る。

既定では公開既定を観測する。`--profile jev-routed --history-mode legacy`
では両腕のrootをAstra/mediumへ固定し、PolarisのJev・4段のモデル・検索退避を
明示する。strict10を選ぶ場合は既存の埋め込み資源の固定情報も要る。
`--prepare-only`は送信せず設定と台本のhashを出す。実行は共通ランナーを使う。

**`--max-turns` だけは例外で、既定の20から上げる。** 2026-09-20のパイロットで、
todoの実装ターンがpolarisの内部ツールループ上限に当たり `stopped: MaxTurns`
で終了コード1になった（1ターンで入力951,526を使い切った）。codexにこの上限は
無い。既定のままだと、polarisだけが仕事を途中でやめた状態を「安い」と数える
ことになり、比較が成立しない。**この既定値が中規模の実装1ターンに足りないこと
自体は所見として残す。** ここで上げるのは、同じ仕事をさせてから量を比べるため
である。

費用は `benchmark_costs.price_usage` で両腕を同じ表・同じ式にかける。

    python3 scripts/benchmark_conversation.py --arm polaris --pattern todo \
        --repeats 5 --output /abs/outside/a/repo
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))

import benchmark_codex  # noqa: E402
import benchmark_scale  # noqa: E402
from benchmark_profile import Profile, PREFIX, claim_attempt, verify_binary  # noqa: E402

REPO = Path(__file__).resolve().parent.parent
POLARIS = REPO.parent / "polaris" / "target" / "release" / "polaris"
CODEX = Path(shutil.which("codex") or Path.home() / ".local" / "bin" / "codex")

# ------------------------------------------------------------------ 台本

SMALL_TURNS = [
    "配送料金を計算する小さなPythonモジュールを作りたい。まだコードは書かないでください。"
    "READMEを読んで、実装方針の案を2つ、それぞれの弱点とあわせて挙げてください。",
    "軽い方の案で進めます。依存は標準ライブラリだけ、ファイルは1つに収めてください。"
    "まだコードは書かないでください。",
    "境界値で迷いそうな点を3つ挙げ、それぞれどちらに倒すか決めてください。"
    "まだコードは書かないでください。",
    benchmark_scale.BUILD_PROMPT,
]

TODO_PLAN_KEYS_FIRST = ("plan_id", "steps")
TODO_PLAN_KEYS_SECOND = ("plan_id", "steps", "schema_sql", "error_table", "tests")

TODO_TURNS = [
    # 1 — 依頼。技術選定をわざと空けておき、次のターンで答える。
    "todoのwebアプリを作りたい。この環境にネットワークは無く、npm installはできません。"
    "まだコードは書かないでください。まず要件を詰めたい。"
    "技術選定で確認したいことがあれば質問してください。",
    # 2 — 利用者が指定する技術スタック。
    "データベースはsqlite、画面はreactを使います。サーバーはNode.jsの標準モジュールだけで書いてください"
    "（Node 24なので`node:sqlite`と`node:http`が使えます）。"
    "reactはビルドせず、ソースファイルとして置くだけで構いません。まだコードは書かないでください。",
    # 3〜10 — ブレスト。
    "保存するデータの項目を決めたい。todo 1件が持つ項目と型を挙げてください。まだコードは書かないでください。",
    "HTTPのAPIを決めたい。必要なエンドポイントを、メソッド・パス・要求・応答の形で挙げてください。"
    "まだコードは書かないでください。",
    "入力の検証はどこまでやるか決めたい。拒否する入力と、その理由を挙げてください。まだコードは書かないでください。",
    "エラー応答の形を統一したい。どの状況でどの状態コードを返すか決めてください。まだコードは書かないでください。",
    "再起動してもデータが残ることを確かめたい。永続化の方式と、確認手順を書いてください。"
    "まだコードは書かないでください。",
    "画面側の構成を決めたい。必要なコンポーネントと、それぞれの責務を挙げてください。まだコードは書かないでください。",
    "テストの方針を決めたい。何を自動で確かめ、何を手で確かめるか分けてください。まだコードは書かないでください。",
    "今回やらないことを決めたい。範囲外にする機能を挙げ、理由を添えてください。まだコードは書かないでください。",
    # 11 — 計画。JSONの鍵で採点するため、形をここで固定する。
    "ここまでを実装計画にまとめてください。まだコードは書かないでください。"
    "回答の最後に、JSONだけの行を1つ置いてください。形は "
    '{"plan_id":文字列,"steps":[文字列,...]} です。',
    # 12 — 却下。要求する追加は3つとも鍵で数えられるものにする。
    "この計画は却下します。3点が足りません。"
    "(1) テーブルのCREATE TABLE文がない。(2) HTTPエラー応答の一覧がない。(3) テスト項目の列挙がない。"
    "計画を作り直して再送してください。まだコードは書かないでください。"
    "最後のJSONは "
    '{"plan_id":文字列,"steps":[文字列,...],"schema_sql":文字列,'
    '"error_table":[{"status":整数,"when":文字列},...],"tests":[文字列,...]} '
    "の形にしてください。",
    # 13 — 再送の確認。1点だけ聞く。まだ承認しない。
    "再送された計画を確認しました。1点だけ教えてください。"
    "サーバーを再起動したとき、既存のtodoが消えないことはどの手順で確かめられますか。"
    "まだコードは書かないでください。",
    # 14 — 承認して実装。
    "この計画で進めてよいです。承認します。README.mdの「環境の約束」に書いた通りに実装してください。",
    # 15 — レビュー。
    "実装をレビューしてください。README.mdの約束から外れている点と、直すべき点を挙げ、"
    "直せるものはその場で直してください。",
]

TODO_README = """# todo web app

## 環境の約束

この約束から外れた実装は採点できない。

- ネットワークは使えない。`npm install` は失敗する。依存を増やさないこと。
- Node.js 24。サーバーは標準モジュールだけで書く。`node:http` と `node:sqlite`
  （`import { DatabaseSync } from 'node:sqlite'`）が使える。
- サーバーの入口は `server.mjs`。`process.env.PORT` の番号で待ち受け、
  `process.env.DB_PATH` のファイルにsqliteで保存する。どちらも必ず環境変数から読む。
- 画面は `web/App.jsx`。default export のReactコンポーネントを1つ置く。ビルドはしない。

## APIの約束

| メソッド | パス | 要求 | 応答 |
| --- | --- | --- | --- |
| POST | `/api/todos` | `{"title":"..."}` | 201 `{"id":整数,"title":文字列,"done":false}` |
| GET | `/api/todos` | — | 200 `[{"id":...,"title":...,"done":...}, ...]` |
| PATCH | `/api/todos/:id` | `{"done":true}` | 200 `{"id":...,"title":...,"done":true}` |
| DELETE | `/api/todos/:id` | — | 204、本文なし |

- `title` が空文字列または欠落なら 400。
- 存在しない `id` への PATCH と DELETE は 404。
- `done` はJSONの真偽値で返す。0/1や文字列にしない。
"""

TURNS = {
    "analyze": [benchmark_scale.ANALYZE_PROMPT],
    "small": SMALL_TURNS,
    "todo": TODO_TURNS,
}
SANDBOX = {"analyze": "read-only", "small": "workspace-write", "todo": "workspace-write"}
# 公開既定は20。上の理由でここだけ上げる。値は全型で1つにして、腕の設定が
# 課題ごとに変わらないようにする。
POLARIS_MAX_TURNS = 60
# 1ターンの上限。todoの実装ターンは`--max-turns 60`で1,800秒を越えた。
TURN_TIMEOUT = 5400
PATTERNS = tuple(TURNS)

# ------------------------------------------------------------------ fixture


def build_fixture(pattern: str, root: Path) -> None:
    """課題ごとのfixtureを毎回まっさらから作る。

    `.polaris` を必ず置く。無いと `resolve_root` が上のディレクトリへ抜け、
    測定対象のリポジトリ自身をプロジェクトルートに採ってしまう。
    `AGENTS.md` は0バイト。過去の比較と条件を揃える。
    """
    # Keep failed attempts as evidence; a new attempt needs a new directory.
    if root.exists():
        raise FileExistsError(f"fixture already exists: {root}")
    if pattern in ("analyze", "small"):
        benchmark_scale.build_fixture(
            {"analyze": "analyze", "small": "build"}[pattern], root)
        return
    root.mkdir(parents=True)
    (root / ".polaris").mkdir()
    (root / "AGENTS.md").write_text("", encoding="utf-8")
    (root / "README.md").write_text(TODO_README, encoding="utf-8")


def fixture_digest(pattern: str) -> str:
    if pattern in ("analyze", "small"):
        return benchmark_scale.fixture_digest(
            {"analyze": "analyze", "small": "build"}[pattern])
    import hashlib
    return hashlib.sha256(TODO_README.encode("utf-8")).hexdigest()


# ------------------------------------------------------------------ 腕: polaris


class Money:
    """監査済みの要求別会計を既存レポートへ接続する。

    `accounting`を正本とし、互換欄にも確定値だけを置く。欠測を含む量は
    `known_tokens`・`known_usd`に小計を残す。未知モデルの上限は作らない。
    """

    def __init__(self, rows: list[dict], errors=()) -> None:
        self.rows = rows
        self.errors = list(errors)

    def report(self) -> dict:
        import benchmark_usage_audit as accounting

        summary = accounting.summarize(self.rows, self.errors)
        grouped: dict[str, list[dict]] = {}
        for row in self.rows:
            name = row.get("model")
            name = name if isinstance(name, str) and name else "unknown"
            grouped.setdefault(name, []).append(row)
        models = {}
        for name, rows in grouped.items():
            per = accounting.summarize(rows, self.errors)
            models[name] = {"requests": per["requests"], **per["tokens"],
                            "efforts": summary["models"][name]["efforts"],
                            "known_tokens": per["known_tokens"],
                            "usd": per["usd"], "known_usd": per["known_usd"]}
        unpriced_rows = [row for name, rows in grouped.items()
                         if name in summary["unpriced_models"] for row in rows]
        unpriced = accounting.summarize(unpriced_rows)
        return {"requests": summary["requests"], **summary["tokens"],
                "missing": len(summary["issues"]) + summary["unpriced_models"].get("unknown", 0),
                "known_tokens": summary["known_tokens"], "models": models,
                "usd": summary["usd"], "known_usd": summary["known_usd"],
                "usd_floor": summary["known_usd"], "usd_ceiling": summary["usd"],
                "unpriced_models": summary["unpriced_models"],
                "unpriced_usage": {key: unpriced["tokens"][key] if unpriced_rows else 0
                                   for key in accounting.FIELDS[:4]},
                "accounting": summary}


def polaris_session_id(data_dir: Path) -> str | None:
    """turn 1 が作ったv2セッションを探す。

    CLIはセッションidを一度も出力しない（`main.rs` に println が無い）。
    `POLARIS_DATA_DIR` を run ごとに分けてあるので候補は1つに決まる。
    """
    root = data_dir / "sessions-v2"
    if not root.is_dir():
        return None
    found = [(p.stat().st_mtime, p.name) for p in root.iterdir() if p.is_dir()]
    return sorted(found)[-1][1] if found else None


def run_polaris(fixture: Path, out: Path, stem: str, turns: list[str],
                sandbox: str, timeout: int, profile: Profile | None = None,
                binary: Path | None = None) -> dict:
    data = out / f"{stem}.data"
    metrics = out / f"{stem}.metrics.jsonl"
    if profile:
        profile.preflight(os.environ)
    claim_attempt(out, stem)
    data.mkdir(parents=True)
    env = dict(os.environ,
               POLARIS_PROVIDER="codex",
               POLARIS_DATA_DIR=str(data),
               POLARIS_METRICS_PATH=str(metrics),
               POLARIS_WS="off", POLARIS_BENCHMARK_REPORT="1")
    for name in ("POLARIS_MODEL", "POLARIS_EFFORT"):
        env.pop(name, None)
    session, replies, failures, observations = None, [], [], []
    attempted = 0
    started = time.monotonic()
    for index, text in enumerate(turns, start=1):
        argv = [str(binary or POLARIS), "--sandbox", sandbox, "--approval", "never",
                "--max-turns", str(POLARIS_MAX_TURNS)]
        if profile:
            argv += profile.polaris_args()
        if session is not None:
            argv += ["--resume", session]
        argv += ["-p", text]
        try:
            attempted += 1
            proc = subprocess.run(argv, cwd=fixture, env=env, capture_output=True,
                                  text=True, timeout=timeout)
        except subprocess.TimeoutExpired as error:
            for suffix, value in (("out", error.stdout), ("err", error.stderr)):
                if isinstance(value, bytes):
                    value = value.decode("utf-8", "replace")
                (out / f"{stem}.turn{index:02}.{suffix}").write_text(value or "", encoding="utf-8")
            failures.append({"turn": index, "returncode": None,
                             "stderr_tail": f"timeout after {timeout}s"})
            break
        (out / f"{stem}.turn{index:02}.out").write_text(proc.stdout, encoding="utf-8")
        (out / f"{stem}.turn{index:02}.err").write_text(proc.stderr, encoding="utf-8")
        replies.append(proc.stdout)
        try:
            observations.append(profile.observe(proc.stderr) if profile else {
                "events": [json.loads(line[len(PREFIX):]) for line in proc.stderr.splitlines()
                           if line.startswith(PREFIX)]})
        except ValueError as error:
            failures.append({"turn": index, "returncode": proc.returncode,
                             "stderr_tail": str(error), "kind": "observation_contract"})
            break
        if proc.returncode:
            failures.append({"turn": index, "returncode": proc.returncode,
                             "stderr_tail": proc.stderr[-400:]})
            break
        if session is None:
            session = polaris_session_id(data)
            if session is None:
                failures.append({"turn": index, "returncode": 0,
                                 "stderr_tail": "session id not found under POLARIS_DATA_DIR"})
                break
    return {"arm": "polaris", "session": session, "replies": replies,
            "failures": failures, "seconds": round(time.monotonic() - started, 3),
            "metrics_path": str(metrics), "turns_sent": attempted,
            "observations": observations}


def polaris_usage(metrics: Path) -> dict:
    """物理要求の原記録を共通集計器へ渡し、欠測と既知小計を保持する。"""
    from benchmark_usage_audit import polaris_requests

    rows, _, _, errors = polaris_requests(metrics)
    return Money(rows, errors).report()


# ------------------------------------------------------------------ 腕: codex


def codex_events(path: Path) -> list[dict]:
    rows = []
    if not path.exists():
        return rows
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rows.append(json.loads(line))
        except ValueError:
            continue
    return rows


def run_codex(fixture: Path, out: Path, stem: str, turns: list[str],
              sandbox: str, timeout: int, profile: Profile | None = None,
              binary: Path | None = None) -> dict:
    claim_attempt(out, stem)
    thread, replies, failures, observations = None, [], [], []
    attempted = 0
    started = time.monotonic()
    started_unix = time.time()
    for index, text in enumerate(turns, start=1):
        prompt = out / f"{stem}.turn{index:02}.prompt"
        prompt.write_text(text, encoding="utf-8")
        last = out / f"{stem}.turn{index:02}.last.txt"
        events = out / f"{stem}.turn{index:02}.jsonl"
        common = ["--json", "--ignore-user-config", "--skip-git-repo-check",
                  "-o", str(last)]
        if profile:
            common += profile.codex_args()
        if thread is None:
            argv = [str(binary or CODEX), "exec", *common, "--sandbox", sandbox,
                    "-C", str(fixture), "-"]
        else:
            # `codex exec resume` has no --sandbox and no -C: the mode goes
            # through -c and the directory through the process cwd.
            argv = [str(binary or CODEX), "exec", "resume", thread, "-", *common,
                    "-c", f'sandbox_mode="{sandbox}"']
        try:
            with prompt.open("rb") as stdin, events.open("wb") as stdout:
                attempted += 1
                proc = subprocess.run(argv, cwd=fixture, stdin=stdin, stdout=stdout,
                                      stderr=subprocess.PIPE, timeout=timeout)
        except subprocess.TimeoutExpired:
            failures.append({"turn": index, "returncode": None,
                             "stderr_tail": f"timeout after {timeout}s"})
            break
        (out / f"{stem}.turn{index:02}.err").write_bytes(proc.stderr)
        replies.append(last.read_text(encoding="utf-8", errors="replace")
                       if last.exists() else "")
        if proc.returncode:
            failures.append({"turn": index, "returncode": proc.returncode,
                             "stderr_tail": proc.stderr.decode("utf-8", "replace")[-400:]})
            break
        seen = [e["thread_id"] for e in codex_events(events)
                if e.get("type") == "thread.started" and e.get("thread_id")]
        if not seen:
            failures.append({"turn": index, "returncode": 0,
                             "stderr_tail": "thread.started missing"})
            break
        if thread is not None and seen[0] != thread:
            failures.append({"turn": index, "returncode": 0,
                             "stderr_tail": "resumed a different thread"})
            break
        thread = seen[0]
        if profile:
            paths = benchmark_codex.wait_for_rollouts(Path.home() / ".codex", started_unix, fixture)
            try:
                previous = observations[-1]['observed_requests'] if observations else 0
                observations.append(profile.observe_codex(paths, thread, previous))
            except (ValueError, OSError) as error:
                failures.append({"turn": index, "returncode": proc.returncode,
                                 "stderr_tail": str(error), "kind": "observation_contract"})
                break
    return {"arm": "codex", "session": thread, "replies": replies,
            "failures": failures, "seconds": round(time.monotonic() - started, 3),
            "turns_sent": attempted, "observations": observations}


def codex_turn_usage(out: Path, stem: str, turns: int) -> dict:
    """`turn.completed` を足す。

    主スレッドしか載らないことが2026-09-18に分かっているので、これは
    照合用であって正本ではない。正本はrolloutの第2パスで取る。
    """
    total = {"requests": 0, "input": 0, "cache_read": 0, "cache_write": 0,
             "output": 0, "reasoning": 0, "missing": 0}
    for index in range(1, turns + 1):
        rows = [e for e in codex_events(out / f"{stem}.turn{index:02}.jsonl")
                if e.get("type") == "turn.completed"]
        if not rows:
            total["missing"] += 1
            continue
        for row in rows:
            usage = row.get("usage") or {}
            total["requests"] += 1
            total["input"] += usage.get("input_tokens") or 0
            total["cache_read"] += usage.get("cached_input_tokens") or 0
            total["cache_write"] += usage.get("cache_write_input_tokens") or 0
            total["output"] += usage.get("output_tokens") or 0
            total["reasoning"] += usage.get("reasoning_output_tokens") or 0
    return total


# ------------------------------------------------------------------ 採点


def last_json_object(text: str):
    return benchmark_scale.last_json_object(text)


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def http(method: str, url: str, body=None, timeout: float = 5.0):
    data = None if body is None else json.dumps(body).encode("utf-8")
    request = urllib.request.Request(url, data=data, method=method)
    if data is not None:
        request.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            raw = response.read().decode("utf-8", "replace")
            try:
                return response.status, json.loads(raw) if raw else None
            except ValueError:
                return response.status, raw
    except urllib.error.HTTPError as error:
        return error.code, None
    except Exception as error:  # 接続不可も採点対象の事実として残す
        return None, str(error)


def todo_api_check(fixture: Path, timeout: int) -> dict:
    """実際に起動して叩く。ファイルのgrepでは通ったことにしない。"""
    server = fixture / "server.mjs"
    detail = {"server_exists": server.is_file(),
              "app_jsx_exists": (fixture / "web" / "App.jsx").is_file()}
    if not detail["server_exists"]:
        return {"pass": False, "detail": detail}
    port, db = free_port(), fixture / "score.sqlite3"
    if db.exists():
        db.unlink()
    env = dict(os.environ, PORT=str(port), DB_PATH=str(db))
    base = f"http://127.0.0.1:{port}"

    def start():
        return subprocess.Popen([shutil.which("node") or "node", str(server)],
                                cwd=fixture, env=env,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def stop(proc) -> None:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)
        for pipe in (proc.stdout, proc.stderr):
            if pipe is not None and not pipe.closed:
                pipe.close()

    def wait(proc) -> bool:
        for _ in range(int(timeout * 10)):
            if proc.poll() is not None:
                return False
            if http("GET", f"{base}/api/todos")[0] is not None:
                return True
            time.sleep(0.1)
        return False

    proc = start()
    try:
        detail["started"] = wait(proc)
        if not detail["started"]:
            proc.kill()
            detail["stderr_tail"] = (proc.stderr.read() or b"")[-400:].decode("utf-8", "replace")
            return {"pass": False, "detail": detail}
        status, created = http("POST", f"{base}/api/todos", {"title": "牛乳を買う"})
        detail["create_201"] = status == 201
        detail["create_shape"] = (isinstance(created, dict)
                                  and created.get("title") == "牛乳を買う"
                                  and created.get("done") is False
                                  and isinstance(created.get("id"), int))
        todo_id = created.get("id") if isinstance(created, dict) else None
        detail["empty_title_400"] = http("POST", f"{base}/api/todos", {"title": ""})[0] == 400
        detail["missing_patch_404"] = http(
            "PATCH", f"{base}/api/todos/999999", {"done": True})[0] == 404
        status, listed = http("GET", f"{base}/api/todos")
        detail["list_200"] = status == 200 and isinstance(listed, list) and len(listed) == 1
        if todo_id is not None:
            status, patched = http("PATCH", f"{base}/api/todos/{todo_id}", {"done": True})
            detail["patch_200"] = status == 200
            detail["patch_done_true"] = isinstance(patched, dict) and patched.get("done") is True
    finally:
        stop(proc)
    # 再起動して残っているか。ここが永続化の唯一の証拠である。
    proc = start()
    try:
        detail["restarted"] = wait(proc)
        if detail["restarted"]:
            status, listed = http("GET", f"{base}/api/todos")
            detail["persisted"] = (status == 200 and isinstance(listed, list)
                                   and len(listed) == 1 and listed[0].get("done") is True)
            if todo_id is not None:
                detail["delete_204"] = http("DELETE", f"{base}/api/todos/{todo_id}")[0] == 204
                detail["missing_delete_404"] = http(
                    "DELETE", f"{base}/api/todos/{todo_id}")[0] == 404
    finally:
        stop(proc)
    required = ("server_exists", "started", "create_201", "create_shape", "empty_title_400",
                "missing_patch_404", "list_200", "patch_200", "patch_done_true",
                "restarted", "persisted", "delete_204", "missing_delete_404")
    return {"pass": all(detail.get(name) is True for name in required), "detail": detail}


def todo_protocol(replies: list[str]) -> dict:
    """計画を出したか、却下のあと直したか。**鍵の有無だけで見る。**"""
    first = last_json_object(replies[10]) if len(replies) > 10 else None
    second = last_json_object(replies[12 - 1]) if len(replies) > 11 else None
    detail = {
        "plan_v1_json": isinstance(first, dict),
        "plan_v1_keys": isinstance(first, dict) and all(
            key in first for key in TODO_PLAN_KEYS_FIRST),
        "plan_v2_json": isinstance(second, dict),
        "plan_v2_keys": isinstance(second, dict) and all(
            key in second for key in TODO_PLAN_KEYS_SECOND),
        "plan_changed": (isinstance(first, dict) and isinstance(second, dict)
                         and json.dumps(first, sort_keys=True, ensure_ascii=False)
                         != json.dumps(second, sort_keys=True, ensure_ascii=False)),
    }
    detail["turns_completed"] = len(replies)
    return {"pass": all(detail[name] is True for name in
                        ("plan_v1_json", "plan_v1_keys", "plan_v2_json",
                         "plan_v2_keys", "plan_changed")),
            "detail": detail}


def score(pattern: str, run: dict, fixture: Path, timeout: int) -> dict:
    replies = run["replies"]
    if run["failures"]:
        return {"pass": False, "detail": {"failed_before_scoring": run["failures"]}}
    if pattern == "analyze":
        return benchmark_scale.score("analyze", replies[-1], fixture, timeout)
    if pattern == "small":
        return benchmark_scale.score("build", replies[-1], fixture, timeout)
    protocol = todo_protocol(replies)
    api = todo_api_check(fixture, timeout)
    return {"pass": protocol["pass"] and api["pass"],
            "detail": {"protocol": protocol["detail"], "api": api["detail"]},
            "protocol_pass": protocol["pass"], "api_pass": api["pass"]}


# ------------------------------------------------------------------ 実行


def one_run(arm: str, pattern: str, repeat: int, out: Path, timeout: int,
            score_timeout: int, profile: Profile | None = None,
            binary: Path | None = None) -> dict:
    stem = f"{pattern}-{arm}-{repeat}"
    fixture = out / f"{stem}.fixture"
    build_fixture(pattern, fixture)
    if profile and arm == "polaris":
        (fixture / ".polaris" / "config.toml").write_text(profile.toml(), encoding="utf-8")
    driver = run_polaris if arm == "polaris" else run_codex
    run = driver(fixture, out, stem, TURNS[pattern], SANDBOX[pattern], timeout, profile, binary)
    try:
        result = score(pattern, run, fixture, score_timeout)
    except Exception as error:  # preserve observed consumption even if scoring breaks
        run["failures"].append({"kind": "scoring", "turn": None, "returncode": None,
                                "stderr_tail": f"{type(error).__name__}: {error}"[:400]})
        result = {"pass": False, "detail": {"scoring_error": type(error).__name__}}
    row = {"arm": arm, "pattern": pattern, "repeat": repeat, "stem": stem,
           "session": run["session"], "seconds": run["seconds"],
           "turns_sent": run["turns_sent"], "turns_scripted": len(TURNS[pattern]),
           "failures": run["failures"], "pass": result["pass"],
           "score_detail": result["detail"],
           "fixture_sha256": fixture_digest(pattern)}
    row["observations"] = run.get("observations", [])
    if arm == "polaris":
        row["usage"] = polaris_usage(Path(run["metrics_path"]))
    else:
        row["usage_turn_events"] = codex_turn_usage(out, stem, run["turns_sent"])
        row["usage"] = None  # rollout の第2パスで埋める
    return row


def collect_codex_usage(rows: list[dict], out: Path, started: float) -> None:
    """codexの使用量はrunが全部終わってから読む。

    codexはプロセス終了後にrolloutを書き終える。直後に読むと0件になる。
    2026-09-19に60秒待っても足りず、第2パスへ移した経緯がある。
    """
    for row in rows:
        if row["arm"] != "codex":
            continue
        fixture = out / f"{row['stem']}.fixture"
        paths = benchmark_codex.wait_for_rollouts(
            Path.home() / ".codex", started, fixture)
        raw = benchmark_codex.usage_from(paths) if paths else None
        row["usage"] = codex_usage_by_model(paths) if paths else None
        row["usage_rollout_raw"] = raw
        row["rollouts"] = [str(p) for p in paths] if paths else []


def codex_usage_by_model(paths: list[Path]) -> dict | None:
    """共通parserで再通知を除き、物理要求のモデル・単価で集計する。"""
    from benchmark_usage_audit import codex_requests

    rows, duplicates, errors = codex_requests(paths)
    return Money(rows, errors).report() | {"duplicate_notifications": duplicates}


def priced(usage: dict | None) -> str | None:
    """正本の要求別会計から確定総額を返す。旧集計から再推定しない。"""
    if not usage or not usage.get("requests"):
        return None
    accounting = usage.get("accounting")
    return accounting.get("usd") if isinstance(accounting, dict) else None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arm", choices=("polaris", "codex"), required=True)
    parser.add_argument("--pattern", choices=PATTERNS, required=True)
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--timeout", type=int, default=TURN_TIMEOUT)
    parser.add_argument("--score-timeout", type=int, default=120)
    parser.add_argument("--profile", choices=("published-defaults", "jev-routed"),
                        default="published-defaults")
    parser.add_argument("--history-mode", choices=("legacy", "strict10"))
    parser.add_argument("--embedding-pins", type=Path,
                        help="JSON containing existing runtime, model_path and revision")
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--binary", type=Path, help="explicit artifact for this arm")
    parser.add_argument("--binary-sha256", help="required frozen artifact digest for live jev-routed")
    args = parser.parse_args()
    binary = args.binary or (POLARIS if args.arm == 'polaris' else CODEX)
    if args.repeats < 1 or args.timeout < 1 or args.score_timeout < 1:
        parser.error("repeats and time limits must be positive")
    profile = None
    if args.profile == "jev-routed":
        try:
            pins = json.loads(args.embedding_pins.read_text()) if args.embedding_pins else None
            profile = Profile(args.history_mode, pins)
            if not args.prepare_only:
                if args.binary is None:
                    raise ValueError('live jev-routed requires --binary and --binary-sha256')
                verify_binary(binary, args.binary_sha256)
            if args.arm == "polaris" and not args.prepare_only:
                profile.preflight(os.environ)
        except (ValueError, OSError) as error:
            parser.error(str(error))
    elif args.history_mode or args.embedding_pins:
        parser.error("history/resource overrides require --profile jev-routed")

    out = args.output.resolve()
    # `--output` をリポジトリの中へ置くと、fixtureの `.polaris` より先に
    # 上位の `.git` や `.polaris` が見つかって隔離が壊れる。
    for parent in [out, *out.parents]:
        if parent == Path.home():
            break
        if (parent / ".git").exists() or (parent / "Cargo.toml").exists():
            print(f"--output はリポジトリの外を指すこと: {parent} が見つかった", file=sys.stderr)
            return 2
    out.mkdir(parents=True, exist_ok=True)
    contract = {
        "profile": profile.manifest() if profile else {"name": "published-defaults",
                    "root_comparison": "not controlled; observe actual settings"},
        "arm": args.arm, "pattern": args.pattern, "repeats": args.repeats,
        "turns_per_repeat": len(TURNS[args.pattern]), "timeout": args.timeout,
        "prompts_sha256": hashlib.sha256(json.dumps(TURNS[args.pattern],
            ensure_ascii=False).encode()).hexdigest(),
        "fixture_sha256": fixture_digest(args.pattern),
        "rates_sha256": hashlib.sha256((REPO / "scripts/rates.json").read_bytes()).hexdigest(),
        "binary_path": str(binary),
        "binary_expected_sha256": args.binary_sha256,
        "execution_ready": False if args.prepare_only else None,
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest() if binary.is_file() else None,
    }
    if args.prepare_only:
        with (out / f"{args.pattern}-{args.arm}.plan.json").open("x", encoding="utf-8") as stream:
            json.dump(contract, stream, ensure_ascii=False, indent=2)
        print("送信せず比較設定を保存しました。", flush=True)
        return 0

    report_path = out / f"{args.pattern}-{args.arm}.json"
    if report_path.exists() or (out / f"{args.pattern}-{args.arm}.claim").exists():
        print("既存試行を保持します。新しい出力先を指定してください。", file=sys.stderr)
        return 2

    if not binary.is_file():
        print(f"バイナリが無い: {binary}", file=sys.stderr)
        return 2
    with (out / f"{args.pattern}-{args.arm}.claim").open("x", encoding="utf-8") as stream:
        json.dump(contract, stream, ensure_ascii=False, indent=2)

    started = time.time()
    rows = []
    for repeat in range(1, args.repeats + 1):
        try:
            row = one_run(args.arm, args.pattern, repeat, out, args.timeout,
                          args.score_timeout, profile, binary)
        except Exception as error:  # noqa: BLE001 - preserve failure, stop new dispatches
            row = {"arm": args.arm, "pattern": args.pattern, "repeat": repeat,
                   "stem": f"{args.pattern}-{args.arm}-{repeat}", "session": None,
                   "seconds": None, "turns_sent": None,
                   "turns_scripted": len(TURNS[args.pattern]),
                   "failures": [{"turn": None, "returncode": None,
                                 "stderr_tail": f"{type(error).__name__}: {error}"[:400]}],
                   "pass": False, "score_detail": {"crashed": type(error).__name__},
                   "fixture_sha256": None, "usage": polaris_usage(
                       out / f"{args.pattern}-{args.arm}-{repeat}.metrics.jsonl")
                       if args.arm == "polaris" else None}
        rows.append(row)
        print(json.dumps({k: row[k] for k in ("arm", "pattern", "repeat", "pass",
                                              "seconds", "turns_sent")},
                         ensure_ascii=False), flush=True)
        if row["failures"]:
            break
    if args.arm == "codex":
        collect_codex_usage(rows, out, started)
    for row in rows:
        row["usd"] = priced(row.get("usage"))
        if profile and args.arm == "polaris":
            row["provider_usd"] = row["usd"]
            row["usd"] = None  # Jev physical attempts/rates are not fully accounted for.
            row["decision_state"] = ("observed" if any(
                v.get("decision_state") == "observed" for v in row.get("observations", []))
                else "not_exercised" if row.get("observations") else "unknown")
            row["classifier_exercised"] = row["decision_state"] == "observed"
            row["cost_comparison_valid"] = False

    report = {"arm": args.arm, "pattern": args.pattern, "repeats": args.repeats,
              "model_requested": "gpt-6-astra/medium" if profile else "published defaults (no --model/--effort)",
              "contract": contract,
              "polaris_max_turns": POLARIS_MAX_TURNS,
              "max_turns_note": ("published default is 20; raised because the todo "
                                 "implementation turn hit it and codex has no counterpart"),
              "priced_as": "per-request observed model", "rows": rows,
              "started_unix": started, "finished_unix": time.time()}
    with report_path.open("x", encoding="utf-8") as stream:
        stream.write(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    passed = sum(1 for row in rows if row["pass"])
    print(f"{args.pattern}/{args.arm}: {passed}/{len(rows)} 合格", flush=True)
    return int(any(row["failures"] for row in rows))


if __name__ == "__main__":
    sys.exit(main())
