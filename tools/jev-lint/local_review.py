#!/usr/bin/env python3
"""The same three questions about a test, asked without sending anything.

`jev-lint` reads a diff with a model and reports where a test's name and
its body disagree. This asks the same three questions structurally, on
this machine, and sends nothing anywhere. It stands to `jev-lint` as the
local decider stands to the hosted one: the same job, a weaker mechanism,
and never reported as the same evidence.

What it can see:

- a test whose body asserts nothing, so its name claims something no code
  checks;
- a test whose name says it refuses, rejects, fails or denies, whose body
  never exercises a failing path;
- a test whose name mentions an identifier that never appears in its own
  body.

What it cannot see is whether the assertion means what the name says. That
needs a reader. A clean run here is not a clean `jev-lint` run, and the
report says so rather than implying otherwise.
"""
import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

TEST_ATTR = re.compile(r'#\[(?:tokio::)?test\b[^\]]*\]')
FN = re.compile(r'\bfn\s+([a-z0-9_]+)\s*\(')
ASSERTS = ('assert!', 'assert_eq!', 'assert_ne!', 'assert_matches!', 'panic!', 'unreachable!')
# Only words that name an error path. `not` and `never` were here and
# fired on every test that asserts a negative outcome positively,
# which is most of them.
FAILURE_WORDS = ('refus', 'reject', 'fail', 'den', 'invalid', 'cannot', 'rejected')
FAILURE_EVIDENCE = ('is_err', 'unwrap_err', 'Err(', 'should_panic', 'assert!(!', 'is_none', 'None)')
# Words that carry no identity, so their absence from a body means nothing.
STOP = {'a', 'an', 'the', 'is', 'are', 'and', 'or', 'of', 'to', 'for', 'in', 'on', 'it', 'its',
        'that', 'than', 'with', 'without', 'when', 'where', 'what', 'not', 'no', 'one', 'two',
        'does', 'do', 'be', 'by', 'from', 'into', 'only', 'still', 'even', 'same', 'its', 'as',
        'so', 'at', 'all', 'any', 'each', 'both', 'up', 'out', 'off', 'over', 'under', 'again'}


def test_bodies(text):
    """Every `#[test]` function in the file, as (name, body)."""
    for attr in TEST_ATTR.finditer(text):
        head = FN.search(text, attr.end())
        if head is None:
            continue
        open_brace = text.find('{', head.end())
        if open_brace < 0:
            continue
        depth, at = 0, open_brace
        while at < len(text):
            if text[at] == '{':
                depth += 1
            elif text[at] == '}':
                depth -= 1
                if depth == 0:
                    break
            at += 1
        yield head.group(1), text[open_brace:at]


def findings_for(path, name, body):
    where = f'{path}::{name}'
    if not any(token in body for token in ASSERTS):
        yield {'rule': 'test-name-verifies-claim', 'where': where,
               'finding': 'この試験は何も検査していない。名前が主張していることを確かめるコードが無い'}
    if any(word in name for word in FAILURE_WORDS) \
            and not any(token in body for token in FAILURE_EVIDENCE):
        yield {'rule': 'tests-cover-failure-paths', 'where': where,
               'finding': '名前は失敗や拒否を主張しているが、本体に失敗経路を通す記述が無い'}
    # A whole sentence of a name shares no vocabulary with its body more
    # often than a two-word one does, and saying so every time teaches
    # people to stop reading. Only flag a short, specific name.
    words = [w for w in name.split('_') if len(w) > 3 and w not in STOP]
    missing = [w for w in words if w not in body.lower()]
    if 1 <= len(words) <= 3 and len(missing) == len(words):
        yield {'rule': 'test-name-describes-code', 'where': where,
               'finding': '名前の語がどれも本体に現れない: ' + ', '.join(missing)}


def changed_files(repo, base):
    out = subprocess.run(['git', '-C', str(repo), 'diff', '--name-only', base, '--', '*.rs'],
                         capture_output=True, text=True, check=False)
    if out.returncode:
        raise SystemExit('git diff に失敗しました: ' + out.stderr.strip()[:200])
    return [line for line in out.stdout.splitlines() if line.strip()]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', default=str(Path(__file__).resolve().parent.parent.parent))
    parser.add_argument('--base', default='HEAD')
    parser.add_argument('--path', action='append', default=[],
                        help='diffの代わりに調べるfile。複数可')
    args = parser.parse_args()

    repo = Path(args.repo)
    paths = args.path or changed_files(repo, args.base)
    found, examined = [], 0
    for rel in paths:
        file = repo / rel
        if not file.is_file():
            continue
        text = file.read_text(errors='replace')
        for name, body in test_bodies(text):
            examined += 1
            found.extend(findings_for(rel, name, body))
    print(json.dumps({
        'reviewer': 'local-structural',
        'sent_anywhere': False,
        'files': len(paths),
        'tests_examined': examined,
        'findings': found,
        'not_equivalent_to': 'jev-lint の意味review。断言の中身が名前どおりかは読めていない',
    }, ensure_ascii=False, indent=2))
    # Findings are candidates. A reviewer that fails the build is a gate,
    # and this is not one.
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
