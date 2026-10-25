#!/usr/bin/env python3
"""Align AGENTS.md derived values with code and detect truncated lines.

Only the ## Always on section reaches the model. constitution::cap adds
whole lines from the start and silently drops lines above the limit. On
2026-09-19, the final two of four rules were found not to have reached the
model for half a month: the text claimed a 150-token limit, but code used 100.

The default check measures the delivered lines and returns 1 if any are lost.
--write updates only numeric values derived from code and the document itself.
No new values or rules are invented.

    python3 scripts/sync_agents_md.py            # inspect only
    python3 scripts/sync_agents_md.py --write    # update derived values
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

try:
    import tiktoken
except ImportError:  # pragma: no cover - 環境の問題であって規則の問題ではない
    sys.exit("tiktoken is required: pip install tiktoken")

ROOT = Path(__file__).resolve().parent.parent
ENCODING = "o200k_base"

# 本文が「これはコードから来た値だ」と言っている箇所。周囲の語で固定して
# いるのは、同じ数字が複数の意味で出てくるためである。`100` は上限にも
# 使われるし、履歴の説明にも出てくる。履歴の数字は動かしてはならない。
DERIVED = {
    "cap": r"(?P<pre>\*\*節の中身は )(?P<v>[\d,]+)(?P<post> トークンで)",
    "cap2": r"(?P<pre>この節が )(?P<v>[\d,]+)(?P<post>\s*\n?トークンを超えた原因は)",
    "always_on": r"(?P<pre>英語の同じ \d+ 規則は )(?P<v>[\d,]+)(?P<post> トークンで、)",
    "slack": r"(?P<pre>トークンで、)(?P<v>[\d,]+)(?P<post> の余白が残る)",
    "pinned_base": r"(?P<pre>（基準 )(?P<v>[\d,]+)(?P<post>）)",
    "pinned_root": r"(?P<pre>（root ターン )(?P<v>[\d,]+)(?P<post>）)",
    "budget_limit": r"(?P<pre>上限は `BUDGET_LIMIT` の )(?P<v>[\d,]+)(?P<post> である)",
    "current_core": r"(?P<pre>いまの `CURRENT\.md` は )(?P<v>[\d,]+)(?P<post> で)",
}


def count(text: str) -> int:
    return len(tiktoken.get_encoding(ENCODING).encode(text))


def rust_const(path: Path, name: str) -> int:
    """`pub const NAME: usize = 123;` と `const NAME: usize = 123;` を読む。"""
    source = path.read_text(encoding="utf-8")
    found = re.search(rf"const {re.escape(name)}: usize = ([\d_]+)", source)
    if not found:
        raise LookupError(f"{name} not found in {path}")
    return int(found.group(1).replace("_", ""))


def pinned(path: Path, test_name: str) -> int:
    """試験関数の中の `const PINNED: usize = 123;` を読む。

    `budget.rs` には同名の定数が 2 つあり、どちらも `PINNED` である。関数
    名で位置を決めてから、その先の最初の 1 件を取る。
    """
    source = path.read_text(encoding="utf-8")
    at = source.find(f"fn {test_name}(")
    if at < 0:
        raise LookupError(f"{test_name} not found in {path}")
    found = re.search(r"const PINNED: usize = ([\d_]+)", source[at:])
    if not found:
        raise LookupError(f"PINNED not found in {test_name}")
    return int(found.group(1).replace("_", ""))


def always_on_block(markdown: str) -> str:
    """`constitution::extract_always_on` と同じ切り出し。"""
    out: list[str] = []
    inside = False
    for line in markdown.splitlines():
        if inside:
            if line.startswith("## "):
                break
            out.append(line)
        elif line.startswith("## ") and line[3:].strip() == "Always on":
            inside = True
    return "\n".join(out).strip()


def cap_lines(block: str, limit: int) -> tuple[list[str], list[str]]:
    """`constitution::cap` と同じ積み方。届く行と消える行を返す。

    空行を飛ばさないのは、本物が飛ばさないからである。ここで整形して
    しまうと、本物が消す行を見逃す。
    """
    kept: list[str] = []
    for line in block.splitlines():
        if count("\n".join(kept + [line])) > limit:
            break
        kept.append(line)
    return kept, block.splitlines()[len(kept) :]


def measured(agents: Path) -> dict[str, int]:
    constitution = ROOT / "crates/polaris-core/src/constitution.rs"
    budget = ROOT / "crates/polaris-core/src/budget.rs"
    current = ROOT / ".polaris/notes/CURRENT.md"

    limit = rust_const(constitution, "CONSTITUTION_LIMIT")
    block = always_on_block(agents.read_text(encoding="utf-8"))
    values = {
        "cap": limit,
        "cap2": limit,
        "always_on": count(block),
        "slack": limit - count(block),
        "pinned_base": pinned(budget, "the_always_on_total_is_pinned"),
        "pinned_root": pinned(budget, "the_root_turn_total_is_pinned"),
        "budget_limit": rust_const(budget, "BUDGET_LIMIT"),
    }
    # 現況文書は別リポジトリ状態では無いことがある。無ければその行だけ
    # 触らない。読めなかったことを、値が 0 だったことにしない。
    if current.exists():
        values["current_core"] = count(current.read_text(encoding="utf-8"))
    return values


def apply(markdown: str, values: dict[str, int]) -> tuple[str, list[str]]:
    changed: list[str] = []
    for key, pattern in DERIVED.items():
        if key not in values:
            continue
        want = f"{values[key]:,}"

        def swap(match: re.Match[str], key=key, want=want) -> str:
            if match.group("v") != want:
                changed.append(f"{key}: {match.group('v')} -> {want}")
            return f"{match.group('pre')}{want}{match.group('post')}"

        markdown, hits = re.subn(pattern, swap, markdown)
        if hits == 0:
            changed.append(f"{key}: not found in the document (unchanged)")
    return markdown, changed


def project_limit(global_block: str, limit: int) -> tuple[int, str | None]:
    """`constitution::combine` と同じ配分。

    グローバル側に always-on 節があると、project の枠は上限の全部ではなく
    「上限 − グローバルの実測 − 改行1」になる。グローバルは先に上限の半分で
    切られる。**この配分を見ていないと、project 単独で測って「全部届く」と
    出しておきながら実際には後ろが落ちる。** 2026-09-19 まで、この道具は
    project 側しか見ていなかった。
    """
    if not global_block:
        return limit, None
    global_kept, _ = cap_lines(global_block, limit // 2)
    global_capped = "\n".join(global_kept)
    used = count(global_capped)
    return max(0, limit - used - count("\n")), global_capped


def report(agents: Path, global_agents: Path | None) -> int:
    text = agents.read_text(encoding="utf-8")
    limit = rust_const(ROOT / "crates/polaris-core/src/constitution.rs", "CONSTITUTION_LIMIT")
    block = always_on_block(text)
    if not block:
        print("No ## Always on section. No rules reach the model.")
        return 1

    global_block = ""
    if global_agents is not None and global_agents.exists():
        global_block = always_on_block(global_agents.read_text(encoding="utf-8"))
    effective, global_capped = project_limit(global_block, limit)
    if global_block:
        g_kept, g_dropped = cap_lines(global_block, limit // 2)
        print(
            f"Global {global_agents}: {len(g_kept)} lines delivered "
            f"({count(global_capped)} tokens, limit {limit // 2})"
        )
        for line in g_dropped:
            print(f"  ★truncated {line}")
        print(f"→ project budget: configured {limit}, effective {effective}")

    kept, dropped = cap_lines(block, effective)
    total, reaching = count(block), count("\n".join(kept))
    print(f"Limit {effective} / section {total} / delivered {reaching} / remaining {effective - reaching}")
    for line in kept:
        print(f"  delivered {line}")
    for line in dropped:
        print(f"  ★truncated {line}")

    stale = [c for c in apply(text, measured(agents))[1]]
    for entry in stale:
        print(f"  stale value {entry}")

    if dropped:
        print("\nSome lines are truncated. Reorder them or remove duplicates.")
        return 1
    if stale:
        print("\nDerived values in the document are stale. Update with --write.")
        return 1
    return 0


def self_check() -> int:
    assert always_on_block("## Always on\n- a\n- b\n\n## next\n- c\n") == "- a\n- b"
    assert always_on_block("# no section\n") == ""
    # 上限を 1 にすれば、どの実文も 1 行目から入らない。cap は空を返さず
    # 少なくとも切り詰めた先頭を返す本物とは違い、ここは行単位だけを見る。
    kept, dropped = cap_lines("- " + "x" * 400 + "\n- second", 1)
    assert kept == [] and len(dropped) == 2, (kept, dropped)
    kept, dropped = cap_lines("- short\n- also short", 10_000)
    assert dropped == [], dropped
    # 置換は周囲の語で固定されており、履歴の数字を巻き込まない。
    before = "以前は 143 トークンあり、**節の中身は 100 トークンで**切り詰められる"
    after, changed = apply(before, {"cap": 120})
    assert "143 トークンあり" in after, after
    assert "**節の中身は 120 トークンで**" in after, after
    assert changed == ["cap: 100 -> 120"], changed
    # グローバル側があると project の枠が縮む。縮まないなら配分を見ていない。
    assert project_limit("", 100) == (100, None)
    narrowed, capped = project_limit("- one global rule that costs tokens.", 100)
    assert 0 < narrowed < 100, narrowed
    assert capped == "- one global rule that costs tokens.", capped
    print("self-check ok")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true", help="Update derived values")
    parser.add_argument("--self-check", action="store_true", help="Check this tool itself")
    parser.add_argument("--path", default=str(ROOT / "AGENTS.md"))
    parser.add_argument(
        "--global-path",
        default=str(Path.home() / ".polaris/AGENTS.md"),
        help="Global rules; use --global-path= to measure the project alone",
    )
    args = parser.parse_args()

    if args.self_check:
        return self_check()

    agents = Path(args.path)
    if not agents.exists():
        print(f"{agents} not found")
        return 1

    if args.write:
        text, changed = apply(agents.read_text(encoding="utf-8"), measured(agents))
        if changed:
            agents.write_text(text, encoding="utf-8")
            for entry in changed:
                print(f"Updated {entry}")
        else:
            print("Derived values already match")
    global_agents = Path(args.global_path) if args.global_path else None
    return report(agents, global_agents)


if __name__ == "__main__":
    sys.exit(main())
