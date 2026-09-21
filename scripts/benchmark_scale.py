#!/usr/bin/env python3
"""3種の規模で polaris と純正 Codex CLI を同条件で実測する。

**1つのfileに両腕を置いてある。** 課題文・fixture・採点を2つのfileへ
分けると、片方だけ直した瞬間に比較が崩れる。2026-09-18の測定は腕が
揃っておらず（片側だけMCPが起動していた）、その条件差を残したまま費用を
比べていた。

3種は規模で分けてある。

- `analyze`  … 既存システム（polaris自身）のコードを読んで答える。書き込み無し。
- `build`    … 中規模の設計・実装・レビュー。todo webアプリと同程度。
- `native`   … Swift Packageの実装。ビルドと試験が通ることまで。

採点はすべてオフラインで決まる。**モデルに採点させない。**
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

sys.path.insert(0, str(Path(__file__).resolve().parent))
import benchmark_codex as cxarm  # noqa: E402  rolloutの読み取りを共有する

REPO = Path(__file__).resolve().parent.parent

# ---------------------------------------------------------------- analyze

ANALYZE_PROMPT = (
    "このリポジトリのRustコードを読み、次の5点を調べてください。変更は禁止です。"
    "最終回答はJSONだけで、"
    '{"constitution_limit":整数,"budget_limit":整数,'
    '"tier_default":文字列,"spawn_withdrawn_when":文字列,'
    '"decisions_max_shown":整数} を返してください。'
    "constitution_limitはAGENTS.mdのalways-on節に許される上限トークン数、"
    "budget_limitは常時コンテキスト全体の上限、"
    "tier_defaultは判定器が振り分けなかったときに使われる重さの名前、"
    "spawn_withdrawn_whenはspawnツールが要求から外される条件を一文で、"
    "decisions_max_shownは決定台帳が一度に見せる最大件数です。"
)
ANALYZE_EXPECTED = {
    "constitution_limit": 100,
    "budget_limit": 990,
    "tier_default": "subgiant",
    "decisions_max_shown": 3,
}

# ---------------------------------------------------------------- build

BUILD_README = """# 配送料金API
Python 3.11の標準ライブラリだけで実装する。ネットワーク不使用。外部依存禁止。

`app.py` に次を実装する。

- `parse_order(line: str) -> dict`
  CSV1行 `id,weight_g,destination` を読む。idは非空、weight_gは整数、
  destinationは `tokyo` か `osaka`。違反は `ValueError`。
- `shipping_fee(weight_g: int, destination: str) -> int`
  基本500円。1000gを超える分は1000gごと200円で端数は切り上げ。
  osakaは合計に1.2を掛けて小数点以下切り捨て。負の重量は `ValueError`。
- `process(lines: list[str]) -> list[dict]`
  全行を検証してから結果を返す。1行でも違反があれば何も返さず `ValueError`。
  結果は `{"id":..., "fee":...}` の一覧で、入力順を保つ。

設計と実装のあと、自分の実装を読み直して仕様との相違を直すこと。
"""
BUILD_PROMPT = (
    "README.mdの仕様どおりに app.py を実装してください。"
    "設計・実装・自己レビューまで行い、標準ライブラリだけを使ってください。"
    "変更してよいのは app.py だけです。最後に実装の要点を一文で返してください。"
)
BUILD_CASES = [
    ("shipping_fee", (500, "tokyo"), 500),
    ("shipping_fee", (1000, "tokyo"), 500),
    ("shipping_fee", (1001, "tokyo"), 700),
    ("shipping_fee", (2000, "tokyo"), 700),
    ("shipping_fee", (2001, "tokyo"), 900),
    ("shipping_fee", (1001, "osaka"), 840),
    ("shipping_fee", (500, "osaka"), 600),
]

BUILD_CHECK = r'''
import json, sys
sys.path.insert(0, sys.argv[1])
out = {"import": False, "fee": 0, "fee_total": 0, "parse": False, "atomic": False, "order": False}
try:
    import app
except Exception:
    print(json.dumps(out)); raise SystemExit(0)
out["import"] = True
cases = json.loads(sys.argv[2])
for name, args, want in cases:
    out["fee_total"] += 1
    try:
        if getattr(app, name)(*args) == want:
            out["fee"] += 1
    except Exception:
        pass
try:
    app.parse_order("a,100,tokyo")
    bad = 0
    for line in ("a,100,kyoto", ",100,tokyo", "a,x,tokyo"):
        try:
            app.parse_order(line)
        except ValueError:
            bad += 1
        except Exception:
            pass
    out["parse"] = bad == 3
except Exception:
    pass
try:
    rows = app.process(["b,100,tokyo", "a,2000,osaka"])
    out["order"] = [r["id"] for r in rows] == ["b", "a"]
except Exception:
    pass
try:
    app.process(["a,100,tokyo", "b,-1,tokyo"])
except ValueError:
    out["atomic"] = True
except Exception:
    pass
print(json.dumps(out))
'''

# ---------------------------------------------------------------- native

NATIVE_README = """# ShippingKit
Swift Packageを完成させる。外部依存は禁止で、ネットワークも使わない。

`Sources/ShippingKit/ShippingKit.swift` に次を実装する。

- `public enum ShippingError: Error { case negativeWeight, unknownDestination, emptyID }`
- `public enum Destination: String { case tokyo, osaka }`
- `public func shippingFee(weightGrams: Int, to: Destination) throws -> Int`
  基本500。1000を超える分は1000ごと200で端数は切り上げ。
  osakaは合計に1.2を掛けて切り捨て。負の重量は `negativeWeight`。
- `public struct Order { public let id: String; public let weightGrams: Int; public let destination: Destination }`
- `public func parseOrder(_ line: String) throws -> Order`
  `id,weight,destination` を読む。idが空なら `emptyID`、宛先が未知なら
  `unknownDestination`、重量が負なら `negativeWeight`。
- `public func process(_ lines: [String]) throws -> [(id: String, fee: Int)]`
  全行を検証してから返す。1行でも違反があれば何も返さず投げる。順序を保つ。

`swift build` と `swift test` が通ることまで行う。
"""
NATIVE_PROMPT = (
    "README.mdの仕様どおりに Sources/ShippingKit/ShippingKit.swift を実装してください。"
    "`swift build` と `swift test` が通るまで直してください。"
    "Package.swift と Tests は変更しないでください。"
    "最後に実装の要点を一文で返してください。"
)
NATIVE_PACKAGE = """// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "ShippingKit",
    targets: [
        .target(name: "ShippingKit"),
        .testTarget(name: "ShippingKitTests", dependencies: ["ShippingKit"]),
    ]
)
"""
NATIVE_TESTS = """import XCTest
@testable import ShippingKit

final class ShippingKitTests: XCTestCase {
    func testFee() throws {
        XCTAssertEqual(try shippingFee(weightGrams: 500, to: .tokyo), 500)
        XCTAssertEqual(try shippingFee(weightGrams: 1000, to: .tokyo), 500)
        XCTAssertEqual(try shippingFee(weightGrams: 1001, to: .tokyo), 700)
        XCTAssertEqual(try shippingFee(weightGrams: 2001, to: .tokyo), 900)
        XCTAssertEqual(try shippingFee(weightGrams: 1001, to: .osaka), 840)
        XCTAssertEqual(try shippingFee(weightGrams: 500, to: .osaka), 600)
    }
    func testNegative() {
        XCTAssertThrowsError(try shippingFee(weightGrams: -1, to: .tokyo))
    }
    func testParse() throws {
        let order = try parseOrder("a,100,tokyo")
        XCTAssertEqual(order.id, "a")
        XCTAssertThrowsError(try parseOrder(",100,tokyo"))
        XCTAssertThrowsError(try parseOrder("a,100,kyoto"))
        XCTAssertThrowsError(try parseOrder("a,-1,tokyo"))
    }
    func testProcessIsAtomicAndOrdered() throws {
        let rows = try process(["b,100,tokyo", "a,2000,osaka"])
        XCTAssertEqual(rows.map { $0.id }, ["b", "a"])
        XCTAssertThrowsError(try process(["a,100,tokyo", "b,-1,tokyo"]))
    }
}
"""

STUB = """// ここに実装する。
"""

TASKS = ("analyze", "build", "native")
PROMPTS = {"analyze": ANALYZE_PROMPT, "build": BUILD_PROMPT, "native": NATIVE_PROMPT}
SANDBOX = {"analyze": "read-only", "build": "workspace-write", "native": "workspace-write"}


def build_fixture(task: str, root: Path) -> None:
    """課題ごとのfixtureを作る。**毎runまっさらから作り直す。**

    `.polaris` を必ず置く。無いと `resolve_root` が上のディレクトリへ
    抜け、**測定対象のリポジトリ自身をプロジェクトルートとして解決する。**
    2026-09-19のパイロットで実際に起き、buildの回答に
    `cargo test --workspace` が現れた——polaris-v100 の `AGENTS.md` を
    読んでいた。0バイトの `AGENTS.md` だけでは足りない。

    出力先そのものもリポジトリの外へ置くこと（`--output`）。codex側は
    別の規則で上位を探すので、マーカーだけでは塞げない。
    """
    root.mkdir(mode=0o700, parents=True)
    (root / ".polaris").mkdir(mode=0o700)
    (root / "AGENTS.md").write_text("")
    if not (root / ".polaris").is_dir() or root.resolve() != root:
        raise SystemExit("測定用ルートの隔離を確認できません。")
    if task == "analyze":
        # 読ませるのは実物のRustソースだけ。target・.git・作業文書は入れない。
        # 元のworktreeを触らせないため複製する。
        dst = root / "crates"
        shutil.copytree(
            REPO / "crates", dst,
            ignore=shutil.ignore_patterns("target", "*.rlib", "*.rmeta"),
        )
        (root / "README.md").write_text(
            "polarisのRust実装。`crates/` の下に13のcrateがある。\n"
        )
    elif task == "build":
        (root / "README.md").write_text(BUILD_README)
        (root / "app.py").write_text("# ここに実装する。\n")
    else:
        (root / "README.md").write_text(NATIVE_README)
        (root / "Package.swift").write_text(NATIVE_PACKAGE)
        (root / "Sources" / "ShippingKit").mkdir(parents=True)
        (root / "Sources" / "ShippingKit" / "ShippingKit.swift").write_text(STUB)
        (root / "Tests" / "ShippingKitTests").mkdir(parents=True)
        (root / "Tests" / "ShippingKitTests" / "ShippingKitTests.swift").write_text(NATIVE_TESTS)


def fixture_digest(task: str) -> str:
    parts = {
        "analyze": [ANALYZE_PROMPT, json.dumps(ANALYZE_EXPECTED, sort_keys=True)],
        "build": [BUILD_README, BUILD_PROMPT, json.dumps(BUILD_CASES)],
        "native": [NATIVE_README, NATIVE_PROMPT, NATIVE_PACKAGE, NATIVE_TESTS],
    }[task]
    return hashlib.sha256("".join(parts).encode()).hexdigest()


def last_json_object(text: str):
    """末尾のJSONオブジェクトを取る。前置きや囲みがあっても拾う。"""
    depth = 0
    end = None
    for i in range(len(text) - 1, -1, -1):
        if text[i] == "}":
            if depth == 0:
                end = i
            depth += 1
        elif text[i] == "{":
            depth -= 1
            if depth == 0 and end is not None:
                try:
                    return json.loads(text[i:end + 1])
                except ValueError:
                    end = None
                    depth = 0
    return None


def score(task: str, answer: str, root: Path, timeout: int) -> dict:
    """オフラインの採点。**部分点を通過と混ぜない。**"""
    if task == "analyze":
        got = last_json_object(answer)
        if not isinstance(got, dict):
            return {"pass": False, "detail": "JSONが見つからない"}
        hits = {}
        for key, want in ANALYZE_EXPECTED.items():
            value = got.get(key)
            if isinstance(want, str) and isinstance(value, str):
                hits[key] = value.strip().lower() == want
            else:
                hits[key] = value == want
        # 自由記述は**合否に入れない。** 語彙で照合すると、正しい答えを
        # 言い回しで落とす。2026-09-19のパイロットで、codexの
        # 「agent_typesが空の場合」という正答を「空」が語彙に無いという
        # 理由だけで不正解にした。**採点しているのは知識ではなく表現に
        # なっていた。** 記録には残し、判定は数値4項目で行う。
        return {"pass": all(hits.values()), "detail": hits,
                "free_text": got.get("spawn_withdrawn_when")}
    if task == "build":
        proc = subprocess.run(
            [sys.executable, "-c", BUILD_CHECK, str(root), json.dumps(BUILD_CASES)],
            capture_output=True, text=True, timeout=timeout,
        )
        got = last_json_object(proc.stdout) or {}
        ok = (
            got.get("import")
            and got.get("fee") == got.get("fee_total")
            and got.get("parse")
            and got.get("atomic")
            and got.get("order")
        )
        return {"pass": bool(ok), "detail": got}
    swift = shutil.which("swift")
    if swift is None:
        return {"pass": False, "detail": "swiftが無い"}
    proc = subprocess.run(
        [swift, "test", "--package-path", str(root)],
        capture_output=True, text=True, timeout=timeout,
    )
    return {
        "pass": proc.returncode == 0,
        "detail": {"returncode": proc.returncode, "tail": proc.stderr[-400:]},
    }


def run_polaris(binary: Path, task: str, root: Path, out: Path, stem: str, timeout: int):
    metrics = out / f"{stem}.metrics.jsonl"
    env = os.environ.copy()
    for key in ("POLARIS_CACHE_MODE", "POLARIS_READ_OUTPUT_BYTES"):
        env.pop(key, None)
    env.update(POLARIS_PROVIDER="codex", POLARIS_METRICS_PATH=str(metrics))
    # --model/--effort は渡さない。公開既定こそが比較対象である。
    cmd = [
        str(binary), "--sandbox", SANDBOX[task], "--approval", "never",
        "--max-turns", "40", "--audit", str(out / f"{stem}.audit.jsonl"),
        "--prompt", PROMPTS[task],
    ]
    started = time.monotonic()
    try:
        done = subprocess.run(cmd, cwd=root, env=env, capture_output=True,
                              text=True, timeout=timeout, stdin=subprocess.DEVNULL)
        stdout, stderr, code = done.stdout, done.stderr, done.returncode
    except subprocess.TimeoutExpired as exc:
        stdout = (exc.stdout or b"")
        stderr = (exc.stderr or b"")
        stdout = stdout.decode(errors="replace") if isinstance(stdout, bytes) else stdout
        stderr = stderr.decode(errors="replace") if isinstance(stderr, bytes) else stderr
        code = None
    traces = []
    if metrics.exists():
        for line in metrics.read_text().splitlines():
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if isinstance(row, dict):
                traces.append(row)
    def total(name):
        return sum((r.get("usage") or {}).get(name) or 0 for r in traces)
    missing = sum((r.get("usage") or {}).get("input_tokens") is None for r in traces)
    return dict(
        stdout=stdout, stderr=stderr, exit_code=code,
        seconds=round(time.monotonic() - started, 3),
        requests=len(traces), missing=missing,
        input=total("input_tokens"), cache_read=total("cache_read_tokens"),
        cache_write=total("cache_write_tokens"), output=total("output_tokens"),
        reasoning=total("reasoning_tokens"),
        observed_model_effort=sorted({(r.get("model"), r.get("effort")) for r in traces}),
    )


def run_codex(codex: str, home: Path, task: str, root: Path, out: Path, stem: str, timeout: int):
    last = out / f"{stem}.last.txt"
    cmd = [
        codex, "exec", "--ignore-user-config", "--skip-git-repo-check",
        "--sandbox", SANDBOX[task], "-C", str(root), "-o", str(last), PROMPTS[task],
    ]
    wall = time.time()
    started = time.monotonic()
    try:
        done = subprocess.run(cmd, capture_output=True, text=True,
                              timeout=timeout, stdin=subprocess.DEVNULL)
        stdout, stderr, code = done.stdout, done.stderr, done.returncode
    except subprocess.TimeoutExpired as exc:
        stdout = (exc.stdout or b"")
        stderr = (exc.stderr or b"")
        stdout = stdout.decode(errors="replace") if isinstance(stdout, bytes) else stdout
        stderr = stderr.decode(errors="replace") if isinstance(stderr, bytes) else stderr
        code = None
    # 使用量はここで読まない。**codexはプロセスが終わったあとに
    # rolloutを書き終える**ので、直後に読むと0件になる。60秒待っても
    # 駄目だった（2026-09-19）。全runが終わってから第2パスで集める。
    # 0件は部分的な取りこぼしと区別が付くが、区別が付くことは正しさの
    # 代わりにならない。
    return dict(
        stdout=(last.read_text() if last.exists() else stdout), stderr=stderr, exit_code=code,
        seconds=round(time.monotonic() - started, 3),
        collect={"since": wall, "cwd": str(root.resolve())},
    )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--arm", choices=("polaris", "codex"), required=True)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--label", required=True)
    ap.add_argument("--repeats", type=int, default=5)
    ap.add_argument("--tasks", nargs="+", choices=TASKS, default=list(TASKS))
    ap.add_argument("--timeout", type=int, default=1800)
    ap.add_argument("--score-timeout", type=int, default=600)
    ap.add_argument("--binary", type=Path)
    ap.add_argument("--codex", default="codex")
    args = ap.parse_args()
    if not 1 <= args.repeats <= 10:
        ap.error("repeatsは1〜10")
    if args.arm == "polaris" and args.binary is None:
        ap.error("--binary が要ります")

    home = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex"))
    codex = shutil.which(args.codex) if args.arm == "codex" else None
    if args.arm == "codex" and codex is None:
        ap.error(f"{args.codex} が見つかりません")
    binary = args.binary.resolve(strict=True) if args.binary else None

    out = args.output.resolve()
    # 出力先が既存のリポジトリの中にあると、fixtureの上位に .git や
    # .polaris が現れる。マーカーを置いても codex 側は別の規則で上を
    # 探すので、**場所そのものを外に取る。**
    # home で止める。`resolve_root` が同じところで止まるからである
    # （`$HOME/.polaris` は `polaris login` が作るので、これを根拠に
    # するとhome全体が1つのプロジェクトになる）。危険なのはhomeより
    # 下に現れるマーカーだけである。
    home = Path.home().resolve()
    for parent in [out, *out.parents]:
        if parent == home or parent == parent.parent:
            break
        if (parent / ".git").exists() or (parent / ".polaris").exists():
            ap.error(f"--output はリポジトリの外を指してください（{parent} にマーカーがあります）")
    out.mkdir(parents=True, exist_ok=False, mode=0o700)
    (out / "metadata.json").write_text(json.dumps({
        "arm": args.arm, "label": args.label, "repeats": args.repeats,
        "tasks": args.tasks,
        "requested": "published defaults",
        "agents_md": "0バイトをfixtureへ置く",
        "codex_version": (subprocess.run([codex, "--version"], capture_output=True,
                                         text=True).stdout.strip() if codex else None),
        "binary_sha256": (hashlib.sha256(binary.read_bytes()).hexdigest() if binary else None),
        "fixture_digest": {task: fixture_digest(task) for task in args.tasks},
    }, ensure_ascii=False, indent=2))

    rows = []
    arm_started = time.monotonic()
    for task in args.tasks:
        for repeat in range(args.repeats):
            root = out / f"work-{task}-{repeat + 1}"
            build_fixture(task, root)
            stem = f"{task}-{repeat + 1}"
            got = (run_polaris(binary, task, root, out, stem, args.timeout)
                   if args.arm == "polaris"
                   else run_codex(codex, home, task, root, out, stem, args.timeout))
            (out / f"{stem}.out").write_text(got.pop("stdout"))
            (out / f"{stem}.err").write_text(got.pop("stderr"))
            answer = (out / f"{stem}.out").read_text()
            scoring = time.monotonic()
            try:
                graded = score(task, answer, root, args.score_timeout)
            except subprocess.TimeoutExpired:
                graded = {"pass": False, "detail": "採点がtimeout"}
            score_seconds = round(time.monotonic() - scoring, 3)
            collect = got.pop("collect", None)
            row = {"arm": args.arm, "task": task, "repeat": repeat + 1, **got,
                   "_collect": collect,
                   # モデルが走った時間と、採点が走った時間は別の量である。
                   # nativeの採点は`swift test`で分単位かかり、混ぜると
                   # 「polarisは遅い」に見える。
                   "score_seconds": score_seconds,
                   "quality": graded}
            if args.arm == "polaris":
                row["observed_model_effort"] = [list(p) for p in row["observed_model_effort"]]
                row["billed_input"] = row["input"] - row["cache_read"]
                row["cost_comparison_ready"] = (
                    row["exit_code"] == 0 and row["requests"] > 0 and row["missing"] == 0)
            rows.append(row)
            print(json.dumps(row, ensure_ascii=False), flush=True)
            # 生成物は残す。fixtureの複製（analyze）だけ大きいので消す。
            if task == "analyze":
                shutil.rmtree(root, ignore_errors=True)
    if args.arm == "codex":
        # 第2パス。ここまで来ればcodexは全runぶん書き終えている。
        for row in rows:
            spec = row.pop("_collect")
            measured = cxarm.usage_from(
                cxarm.rollouts_for(home, spec["since"], Path(spec["cwd"])))
            totals = measured["totals"]
            row.update(requests=measured["requests"], missing=measured["missing"],
                       input=totals["input"], cache_read=totals["cache"],
                       cache_write=totals["cache_write"], output=totals["output"],
                       reasoning=totals["reasoning"],
                       observed_model_effort=[list(p) for p in measured["observed_model_effort"]],
                       rollouts=measured["rollouts"],
                       billed_input=totals["uncached_input"],
                       cost_comparison_ready=row["exit_code"] == 0
                       and measured["requests"] > 0 and measured["missing"] == 0)
    for row in rows:
        row.pop("_collect", None)
    total_seconds = round(time.monotonic() - arm_started, 3)
    (out / "rows.jsonl").write_text(
        "".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rows))

    def mean(values):
        return round(sum(values) / len(values), 3) if values else None

    summary = {"arm": args.arm, "label": args.label, "repeats": args.repeats,
               # 腕全体の実時間。run の合計ではない——fixtureの作成、
               # analyzeのcrates複製、採点がここに入る。**これが人が待つ時間である。**
               "total_wall_seconds": total_seconds, "per_task": {}}
    for task in args.tasks:
        held = [r for r in rows if r["task"] == task]
        summary["per_task"][task] = {
            "n": len(held),
            "mean_model_seconds": mean([r["seconds"] for r in held]),
            "mean_score_seconds": mean([r["score_seconds"] for r in held]),
            "mean_requests": mean([r["requests"] for r in held]),
            "mean_billed_input": mean([r["billed_input"] for r in held]),
            "mean_output": mean([r["output"] for r in held]),
            "mean_reasoning": mean([r["reasoning"] for r in held]),
            "passed": sum(r["quality"]["pass"] for r in held),
            "cost_comparison_ready": sum(r["cost_comparison_ready"] for r in held),
        }
    (out / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2))

    ready = sum(r["cost_comparison_ready"] for r in rows)
    passed = sum(r["quality"]["pass"] for r in rows)
    print(f"{len(rows)} 件: 費用比較可 {ready} / 正答 {passed} / 腕全体 {total_seconds:.0f}s")
    for task, block in summary["per_task"].items():
        print(f"  {task:<9} n={block['n']} モデル{block['mean_model_seconds']:.0f}s "
              f"採点{block['mean_score_seconds']:.0f}s 正答{block['passed']}/{block['n']}")
    return 0 if ready == len(rows) else 1


if __name__ == "__main__":
    raise SystemExit(main())
