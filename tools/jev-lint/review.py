#!/usr/bin/env python3
"""Plan a jev-lint review of the changed lines, and only then run it.

`jev-lint` reads a diff and reports where a test's name and its body
disagree, or where a claim has no failing path behind it. It is a second
reader, not a gate: a finding is a candidate, never an automatic fix and
never a reason to fail a build. A Jev failure is also not a compiler
success — the other checks still have to pass on their own.

Nothing here sends source anywhere without being asked. `--dry-run`, the
default, prints the subject, the request count and the estimated tokens
and cost, and makes no request at all. A live review needs `--live`, is
limited to the changed lines, and reads its key only from
`TYPESAFE_API_KEY`, which is never written to a file or a record.

The tool lives in its own directory with its own lockfile so that Node
stays out of the cargo dependency graph and out of the shipped runtime.
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
PINNED = '0.5.0'
# The three shipped Rust rules, and nothing home-made until these are
# calibrated against real diffs.
RULES = ('test-name-describes-code', 'test-name-verifies-claim', 'tests-cover-failure-paths')
# Rough and deliberately rough: a plan that claims precision nobody
# measured is worse than one that says it is an estimate.
CHARS_PER_TOKEN = 4
USD_PER_MILLION_INPUT = 0.042


def changed_lines(repo, base):
    """The added lines of the diff, which is all a review may see."""
    out = subprocess.run(
        ['git', '-C', str(repo), 'diff', '--unified=0', base, '--', '*.rs'],
        capture_output=True, text=True, check=False)
    if out.returncode:
        raise SystemExit('git diff に失敗しました: ' + out.stderr.strip()[:200])
    return [line[1:] for line in out.stdout.splitlines()
            if line.startswith('+') and not line.startswith('+++')]


def plan(lines):
    body = '\n'.join(lines)
    tokens = len(body) // CHARS_PER_TOKEN
    return {
        'subject': f'{len(lines)} 行の追加（*.rs のみ）',
        'rules': list(RULES),
        'requests': 1 if lines else 0,
        'estimated_input_tokens': tokens,
        'estimated_usd': round(tokens * USD_PER_MILLION_INPUT / 1_000_000, 6),
        'pinned_version': PINNED,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', default=str(HERE.parent.parent))
    parser.add_argument('--base', default='HEAD')
    parser.add_argument('--live', action='store_true',
                        help='実際に送信する。既定は計画の表示だけ')
    args = parser.parse_args()

    lines = changed_lines(Path(args.repo), args.base)
    report = plan(lines)
    print(json.dumps(report, ensure_ascii=False, indent=2))
    if not args.live:
        print('送信していない。実行するには --live を付ける。', file=sys.stderr)
        return 0
    if not lines:
        print('送る変更行が無い。', file=sys.stderr)
        return 0
    if not os.environ.get('TYPESAFE_API_KEY'):
        raise SystemExit('TYPESAFE_API_KEY が環境にありません')
    binary = shutil.which('jev-lint', path=str(HERE / 'node_modules' / '.bin'))
    if binary is None:
        raise SystemExit(f'固定版 jev-lint {PINNED} が {HERE} に導入されていません')
    done = subprocess.run(
        [binary, 'review', '--stdin', *sum((['--rule', rule] for rule in RULES), [])],
        input='\n'.join(lines), text=True, capture_output=True, check=False)
    sys.stdout.write(done.stdout)
    sys.stderr.write(done.stderr)
    # A finding is a candidate. The exit value says the review ran, not
    # that the change is acceptable.
    return 0 if done.returncode in (0, 1) else done.returncode


if __name__ == '__main__':
    raise SystemExit(main())
