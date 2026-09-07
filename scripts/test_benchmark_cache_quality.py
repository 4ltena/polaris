#!/usr/bin/env python3
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parent))
import benchmark_cache_quality as quality


def good_rows(case):
    rows = []
    history_count = 2 * len(case['examples'])
    for number, turn in enumerate(case['turns'], 1):
        calls = []
        for required in turn['required_tools']:
            calls.extend({'name': required['name'], 'arguments': copy.deepcopy(required['arguments']), 'success': True}
                         for _ in range(required['min_calls']))
        rows.append({'id': case['id'], 'turn': number,
                     'text': json.dumps(turn['expected'], ensure_ascii=False, separators=(',', ':')),
                     'quality_pass': True, 'history_messages_before': history_count,
                     'history_messages_after': history_count + 2, 'history_preserved': True,
                     'history_before_includes_current_user': False,
                     'history_messages_after_user': history_count + 1,
                     'tool_calls': calls, 'requests': 1,
                     'request_delta': 1, 'requests_case_total': number,
                     'usage': {'input': 1, 'output': 1, 'cache': 0, 'total': 2},
                     'elapsed_seconds': 0.1, 'error': None})
        history_count += 2
    return rows


class CacheQualityCasesTest(unittest.TestCase):
    def test_matrix_pair_coverage_and_examples(self):
        all_cases = quality.cases()
        short = all_cases[:9]
        self.assertEqual([(x['id'], x['skill_count'], x['tool_count'], len(x['examples'])) for x in short],
                         list(quality.MATRIX))
        pair_sets = (
            {(x['skill_count'], x['tool_count']) for x in short},
            {(x['skill_count'], len(x['examples'])) for x in short},
            {(x['tool_count'], len(x['examples'])) for x in short},
        )
        expected_pairs = (
            {(left, right) for left in (0, 3, 30) for right in (0, 3, 6)},
            {(left, right) for left in (0, 3, 30) for right in (0, 1, 3)},
            {(left, right) for left in (0, 3, 6) for right in (0, 1, 3)},
        )
        self.assertEqual(pair_sets, expected_pairs)
        self.assertEqual(sum(len(pairs) for pairs in pair_sets), 27)
        self.assertEqual({len(x['examples']) for x in short}, {0, 1, 3})
        self.assertFalse(quality._same(True, 1))
        for case in all_cases:
            self.assertIsNone(quality.verify_turn_rows(case, good_rows(case)))

    def test_prompts_supply_known_values_and_output_contracts(self):
        for case in quality.cases():
            for example in case['examples']:
                value = json.loads(example['assistant'])['value']
                self.assertIn(value, example['user'])
                self.assertIn('"source":"example"', example['user'])
        direct = quality.cases()[0]['turns'][0]
        self.assertIn(direct['expected']['value'], direct['prompt'])
        self.assertIn('"source":"user"', direct['prompt'])
        long_case = quality.cases()[-2]
        self.assertEqual(long_case['turns'][16]['required_tools'],
                         [{'name': 'skill', 'arguments': {'q': 'format-contract'}, 'min_calls': 1}])
        self.assertIn('「署名コード」を検索', long_case['turns'][5]['prompt'])
        self.assertNotIn('format-contract', long_case['turns'][5]['prompt'])
        for turn in long_case['turns']:
            if turn['expected'].get('source') in ('conversation', 'current.md:1', 'skill'):
                self.assertIn('"source"', turn['prompt'])

    def test_long_cases_have_36_turns_and_recall_prompts_hide_values(self):
        for case in quality.cases()[-2:]:
            self.assertEqual(len(case['turns']), 36)
            self.assertEqual(case['turns'][20]['updates'], {'current.md': 'value=FILE-8B70\n'})
            for turn in case['turns']:
                if turn['expected'].get('source') == 'conversation':
                    self.assertNotIn(turn['expected']['value'], turn['prompt'])

    def test_prepare_creates_exact_skill_counts_and_expected_snapshot(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for case in quality.cases():
                fixture = root / case['id']
                quality.prepare_case(case, fixture)
                self.assertTrue((fixture / '.polaris' / 'skills').is_dir())
                files = list((fixture / '.polaris' / 'skills').glob('*/SKILL.md')) if case['skill_count'] else []
                self.assertEqual(len(files), case['skill_count'])
                if case['skill_count']:
                    target = (fixture / '.polaris' / 'skills' / 'format-contract' / 'SKILL.md').read_text()
                    self.assertIn('description: 現行の署名コードを確認する', target)
                    self.assertIn('SIG-7C91', target)
                    self.assertTrue(all('SIG-7C91' not in path.read_text() for path in files
                                        if path.parent.name != 'format-contract'))
                self.assertEqual((fixture / 'current.md').read_text(), 'value=FILE-3F2A\n')
                self.assertEqual(quality.expected_snapshot(case)['current.md'],
                                 'value=%s\n' % ('FILE-8B70' if case['id'].startswith('L') else 'FILE-3F2A'))
            with self.assertRaises(ValueError):
                quality.prepare_case(quality.cases()[0], root / 'Q01')

    def test_verifier_accepts_complete_rows_and_summary(self):
        case = quality.cases()[-1]
        rows = good_rows(case) + [{'type': 'summary', 'id': case['id']}]
        self.assertIsNone(quality.verify_turn_rows(case, rows))

    def test_verifier_rejects_response_contract_breaks(self):
        case = quality.cases()[0]
        for mutate in (
            lambda rows: rows[0].update(text='{}'),
            lambda rows: rows[0].update(text=json.dumps({'value': case['turns'][0]['expected']['value'], 'source': 'user', 'extra': 1})),
            lambda rows: rows[0].update(text=json.dumps({'value': True, 'source': 'user'})),
            lambda rows: rows[0].update(quality_pass=False),
            lambda rows: rows.pop(),
            lambda rows: rows.append(copy.deepcopy(rows[0])),
            lambda rows: rows[0].update(turn=2),
            lambda rows: rows[0].update(error='driver failure'),
            lambda rows: rows[0].pop('error'),
            lambda rows: rows[0].update(text='{"value":"USER-Q01-4C8E","value":"USER-Q01-4C8E","source":"user"}'),
        ):
            rows = good_rows(case); mutate(rows)
            with self.subTest(mutate=mutate), self.assertRaises(ValueError):
                quality.verify_turn_rows(case, rows)

    def test_verifier_accepts_argument_extensions_but_rejects_all_bad_calls(self):
        case = quality.cases()[1]
        rows = good_rows(case)
        rows[0]['tool_calls'][0]['arguments'].update(offset=0, limit=20)
        rows[0]['tool_calls'].append({'name': 'bash', 'arguments': {'command': 'true'}, 'success': True})
        self.assertIsNone(quality.verify_turn_rows(case, rows))
        for call in (
            {'name': 'skill', 'arguments': {'q': 'x'}, 'success': False},
            {'name': 'spawn', 'arguments': {}, 'success': True},
            {'name': 'write', 'arguments': {}, 'success': True},
            {'name': 'edit', 'arguments': {}, 'success': True},
        ):
            bad = good_rows(case)
            bad[0]['tool_calls'].append(call)
            with self.subTest(call=call), self.assertRaises(ValueError):
                quality.verify_turn_rows(case, bad)

    def test_verifier_rejects_failed_and_out_of_order_required_tools(self):
        case = quality.cases()[4]
        for mutate in (
            lambda rows: rows[0]['tool_calls'][0].update(success=False),
            lambda rows: rows[0]['tool_calls'].reverse(),
            lambda rows: rows[0]['tool_calls'][0]['arguments'].update(q='wrong'),
        ):
            rows = good_rows(case); mutate(rows)
            with self.subTest(mutate=mutate), self.assertRaises(ValueError):
                quality.verify_turn_rows(case, rows)

    def test_verifier_rejects_old_value_after_a_correction(self):
        case = quality.cases()[-2]
        rows = good_rows(case)
        rows[12]['text'] = json.dumps({'value': quality.VALUES['alpha'], 'source': 'conversation'})
        with self.assertRaises(ValueError):
            quality.verify_turn_rows(case, rows)


if __name__ == '__main__':
    unittest.main()
