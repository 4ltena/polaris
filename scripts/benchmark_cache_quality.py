#!/usr/bin/env python3
"""Algedi quality-matrix fixtures and offline result validation.

This module deliberately does not start a model or a benchmark process.  The
Rust driver owns execution; this file makes its inputs and saved turn rows
reproducible and independently checkable with Python 3.9's standard library.
"""
import argparse
import json
import math
from pathlib import Path


MATRIX = (
    ('Q01', 0, 0, 0), ('Q02', 0, 3, 1), ('Q03', 0, 6, 3),
    ('Q04', 3, 0, 1), ('Q05', 3, 3, 3), ('Q06', 3, 6, 0),
    ('Q07', 30, 0, 3), ('Q08', 30, 3, 0), ('Q09', 30, 6, 1),
)
FILE_V1 = 'FILE-3F2A'
FILE_V2 = 'FILE-8B70'
SIGNATURE = 'SIG-7C91'
VALUES = {
    'alpha': 'ALPHA-4D2E', 'beta': 'BETA-91C4', 'gamma': 'ガンマ-虹-7F',
    'delta': 'DELTA-6B8A', 'epsilon': 'EPSILON-2E5D', 'zeta': 'ZETA-0C39',
    'eta': 'ETA-5A71', 'theta': 'THETA-8D04',
}
REVISED = dict(VALUES, alpha='ALPHA-8E16', beta='BETA-2F70')
TOOLS = {
    0: (),
    3: ('read', 'bash', 'skill'),
    6: ('read', 'bash', 'skill', 'write', 'edit', 'spawn'),
}
FORBIDDEN_TOOLS = {'write', 'edit', 'spawn'}


def _examples(count):
    values = ('EX-A-6D2', 'EX-B-9F4', 'EX-C-1A8')
    return [
        {'user': '値 %s を {"value":文字列,"source":"example"} のJSONだけで返してください。' % value,
         'assistant': json.dumps({'value': value, 'source': 'example'}, ensure_ascii=False,
                                 separators=(',', ':'))}
        for value in values[:count]
    ]


def _direct_turn(identifier):
    nonce = 'USER-%s-4C8E' % identifier
    return {'prompt': ('利用者が渡した値 %s を '
                       '{"value":文字列,"source":"user"} のJSONだけで返してください。') % nonce,
            'expected': {'value': nonce, 'source': 'user'}, 'required_tools': [], 'updates': {}}


def _read_turn():
    return {'prompt': ('current.mdを読み、'
                       '{"value":文字列,"source":"current.md:1"} のJSONだけで返してください。'),
            'expected': {'value': FILE_V1, 'source': 'current.md:1'},
            'required_tools': [{'name': 'read', 'arguments': {'path': 'current.md'}, 'min_calls': 1}],
            'updates': {}}


def _signature_turn(discovery=True):
    required = ([{'name': 'skill', 'arguments': {'q': '署名コード'}, 'min_calls': 1},
                 {'name': 'skill', 'arguments': {'q': 'format-contract'}, 'min_calls': 1}]
                if discovery else
                [{'name': 'skill', 'arguments': {'q': 'format-contract'}, 'min_calls': 1}])
    prompt = ('skillで「署名コード」を検索し、見つかった現行skillを取得して確認する。'
              if discovery else 'format-contract skillを取得して確認する。')
    return {'prompt': prompt + '{"value":文字列,"source":"skill"} のJSONだけで返してください。',
            'expected': {'value': SIGNATURE, 'source': 'skill'},
            'required_tools': required, 'updates': {}}


def _short_case(identifier, skill_count, tool_count, example_count):
    if tool_count == 0:
        turn = _direct_turn(identifier)
    elif skill_count == 0:
        turn = _read_turn()
    else:
        turn = _signature_turn()
    return {'id': identifier, 'skill_count': skill_count, 'tool_count': tool_count,
            'examples': _examples(example_count), 'turns': [turn]}


def _store(key, value):
    return {'prompt': 'この対話内だけで記録 %s の値を %s として記憶してください。ファイルは変更せず、以前の値があれば訂正します。{"stored":"%s"} のJSONだけで返してください。' % (key, value, key),
            'expected': {'stored': key}, 'required_tools': [], 'updates': {}}


def _recall(key, value=None):
    return {'prompt': ('記録 %s の現在値を '
                       '{"value":文字列,"source":"conversation"} のJSONだけで返してください。') % key,
            'expected': {'value': VALUES[key] if value is None else value,
                         'source': 'conversation'}, 'required_tools': [], 'updates': {}}


def _long_turns():
    turns = [
        _store('alpha', VALUES['alpha']), _store('beta', VALUES['beta']), _recall('alpha'),
        _store('gamma', VALUES['gamma']), _recall('beta'), _signature_turn(),
        _store('delta', VALUES['delta']), _recall('gamma'), _read_turn(),
        _store('epsilon', VALUES['epsilon']), _recall('alpha'), _store('alpha', REVISED['alpha']),
        _recall('alpha', REVISED['alpha']), _recall('beta'), _store('zeta', VALUES['zeta']), _recall('gamma'),
        _signature_turn(False), _store('beta', REVISED['beta']), _recall('beta', REVISED['beta']), _recall('delta'),
        {'prompt': ('current.mdを再読込し、'
                    '{"value":文字列,"source":"current.md:1"} のJSONだけで返してください。'),
         'expected': {'value': FILE_V2, 'source': 'current.md:1'},
         'required_tools': [{'name': 'read', 'arguments': {'path': 'current.md'}, 'min_calls': 1}],
         'updates': {'current.md': 'value=%s\n' % FILE_V2}},
        _recall('epsilon'), _recall('alpha', REVISED['alpha']), _store('eta', VALUES['eta']), _recall('eta'),
        _recall('gamma'), _signature_turn(), _store('theta', VALUES['theta']), _recall('theta'),
        _recall('alpha', REVISED['alpha']), _recall('delta'), _recall('beta', REVISED['beta']), _recall('zeta'), _recall('epsilon'),
        _recall('eta'),
        {'prompt': ('{"alpha":文字列,"beta":文字列,"gamma":文字列,"delta":文字列,'
                    '"epsilon":文字列,"zeta":文字列,"eta":文字列,"theta":文字列} の8キーだけを使い、'
                    'それぞれの現在値をJSONだけで返してください。'),
         'expected': {key: REVISED[key] for key in VALUES}, 'required_tools': [], 'updates': {}},
    ]
    assert len(turns) == 36
    return turns


def cases():
    """Return a new, JSON-serializable copy of the eleven approved cases."""
    result = [_short_case(*row) for row in MATRIX]
    result.extend([
        {'id': 'L01', 'skill_count': 3, 'tool_count': 3, 'examples': _examples(1), 'turns': _long_turns()},
        {'id': 'L02', 'skill_count': 30, 'tool_count': 6, 'examples': _examples(3), 'turns': _long_turns()},
    ])
    return result


def _skill_source(name, description, body):
    return '---\nname: %s\ndescription: %s\n---\n\n%s\n' % (name, description, body)


def prepare_case(case, fixture):
    """Create one new fixture and its synthetic skills; never replace a file."""
    fixture = Path(fixture)
    if fixture.exists():
        raise ValueError('fixture already exists: %s' % fixture)
    fixture.mkdir(parents=True)
    (fixture / 'current.md').write_text('value=%s\n' % FILE_V1, encoding='utf-8')
    skills = fixture / '.polaris' / 'skills'
    skills.mkdir(parents=True)
    for index in range(case['skill_count']):
        if index == 0:
            name, description, body = ('format-contract', '現行の署名コードを確認する',
                                       '現行の署名コードは %s。JSONのvalueへ使う。' % SIGNATURE)
        else:
            name = 'obsolete-note-%02d' % index
            description = '旧版の署名コード（obsolete）を確認する' if index % 2 else '別件の形式を確認する'
            body = 'obsolete: OLD-%02d。現行の回答には使用しない。' % index
        path = skills / name / 'SKILL.md'
        path.parent.mkdir(parents=True, exist_ok=False)
        path.write_text(_skill_source(name, description, body), encoding='utf-8')


def expected_snapshot(case):
    """Reconstruct mutable regular fixture files, excluding prepared skill files."""
    snapshot = {'current.md': 'value=%s\n' % FILE_V1}
    for turn in case['turns']:
        snapshot.update(turn['updates'])
    return snapshot


def _same(left, right):
    if type(left) is not type(right):
        return False
    if isinstance(left, dict):
        return left.keys() == right.keys() and all(_same(left[key], right[key]) for key in left)
    if isinstance(left, list):
        return len(left) == len(right) and all(_same(a, b) for a, b in zip(left, right))
    return left == right


def _json_text(text):
    if not isinstance(text, str):
        raise ValueError('text must be a JSON string')
    def no_duplicate_keys(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError('duplicate JSON key')
            result[key] = value
        return result
    try:
        value, end = json.JSONDecoder(object_pairs_hook=no_duplicate_keys).raw_decode(text.lstrip())
    except ValueError as error:
        raise ValueError('invalid JSON response') from error
    if text.lstrip()[end:].strip():
        raise ValueError('response is not JSON only')
    return value


def _require(condition, message):
    if not condition:
        raise ValueError(message)


def _argument_subset(actual, needed):
    return (isinstance(actual, dict) and isinstance(needed, dict)
            and all(key in actual and _same(actual[key], value) for key, value in needed.items()))


def _check_tools(tool_count, required, calls):
    _require(isinstance(calls, list), 'tool_calls must be a list')
    allowed = TOOLS[tool_count]
    for call in calls:
        _require(isinstance(call, dict), 'tool call must be an object')
        _require(call.get('success') is True, 'failed tool call')
        _require(call.get('name') in allowed and call.get('name') not in FORBIDDEN_TOOLS,
                 'unadvertised or forbidden tool call')
        _require(isinstance(call.get('arguments'), dict), 'tool arguments must be an object')
    position = 0
    for need in required:
        found = 0
        while position < len(calls) and found < need['min_calls']:
            call = calls[position]
            position += 1
            if call.get('name') == need['name'] and _argument_subset(call.get('arguments'), need['arguments']):
                found += 1
        _require(found == need['min_calls'], 'missing or out-of-order required tool')


def verify_turn_rows(case, rows):
    """Raise ValueError unless saved driver rows meet the quality/scope contract."""
    filtered = []
    for row in rows:
        _require(isinstance(row, dict), 'row must be an object')
        if row.get('type') == 'summary' or row.get('kind') == 'summary':
            continue
        _require('error' in row and row['error'] is None, 'error row or missing error')
        filtered.append(row)
    _require(len(filtered) == len(case['turns']), 'incomplete or duplicate turn rows')
    total_requests = 0
    previous_after = 2 * len(case['examples'])
    for number, (turn, row) in enumerate(zip(case['turns'], filtered), 1):
        _require(row.get('id') == case['id'] and type(row.get('turn')) is int and row['turn'] == number,
                 'wrong turn identity/order')
        _require(row.get('quality_pass') is True, 'quality did not pass')
        _require(row.get('history_preserved') is True, 'history was not preserved')
        before, after = row.get('history_messages_before'), row.get('history_messages_after')
        _require(type(before) is int and before >= 0 and type(after) is int and after > before,
                 'invalid history counts')
        _require(before == previous_after, 'history is not continuous')
        _require(row.get('history_before_includes_current_user') is False
                 and row.get('history_messages_after_user') == before + 1,
                 'current user history count is invalid')
        previous_after = after
        _require(_same(_json_text(row.get('text')), turn['expected']), 'wrong JSON response')
        _check_tools(case['tool_count'], turn['required_tools'], row.get('tool_calls'))
        _require(type(row.get('requests')) is int and 1 <= row['requests'] <= 4, 'invalid requests')
        total_requests += row['requests']
        _require(row.get('request_delta') == row['requests']
                 and row.get('requests_case_total') == total_requests,
                 'request counters disagree')
        usage = row.get('usage')
        _require(isinstance(usage, dict) and set(usage) == {'input', 'output', 'cache', 'total'},
                 'invalid usage shape')
        _require(all(type(usage[key]) is int and usage[key] >= 0 for key in usage), 'invalid usage value')
        _require(usage['input'] > 0 and usage['cache'] <= usage['input']
                 and usage['total'] == usage['input'] + usage['output'], 'inconsistent usage')
        _require(type(row.get('elapsed_seconds')) in (int, float) and math.isfinite(row['elapsed_seconds'])
                 and row['elapsed_seconds'] >= 0,
                 'invalid elapsed_seconds')
    _require(total_requests <= (60 if len(case['turns']) == 36 else 4), 'request limit exceeded')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--prepare-root', type=Path)
    args = parser.parse_args()
    if args.prepare_root is None:
        parser.error('--prepare-root is required')
    if not args.prepare_root.is_absolute():
        parser.error('--prepare-root must be absolute')
    if args.prepare_root.exists():
        parser.error('--prepare-root must be new')
    args.prepare_root.mkdir(parents=True)
    for case in cases():
        prepare_case(case, args.prepare_root / case['id'])
        print(json.dumps(case, ensure_ascii=False, sort_keys=True, separators=(',', ':')))


if __name__ == '__main__':
    main()
