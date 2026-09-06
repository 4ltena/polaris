#!/usr/bin/env python3
"""大きな結果を3回読み、最後に最初の根拠へ戻るGPT-6 medium比較。"""
import json
from pathlib import Path
import re
import sys

import benchmark_efficiency as benchmark


def setup():
    benchmark.README = ('# 資料1\nnext: history.md\nこの資料は台帳Aの取得時点の記録。\n'
        + '\n'.join(f'記録{i:04}: 正常動作を確認。決定変更なし。' for i in range(480))
        + '\n決定R017: 承認済み。台帳Aのretention_days=37。\n'
        + '\n'.join(f'補足{i:04}: 運用状態に変更なし。' for i in range(40))
        + '\n提案P018: 未承認。台帳Aのretention_days=91。適用禁止。\n')
    benchmark.HISTORY = ('# 資料2\nnext: rates.py\n別台帳Bの記録。\n'
        + '\n'.join(f'監査{i:04}: 台帳Bを確認。設定変更なし。' for i in range(520))
        + '\n決定B004: 承認済み。台帳Bのretention_days=110。\n')
    benchmark.RATES = ('# 最終資料\n質問: 最初の資料の台帳Aについて、承認済みretention_daysとdecision_idをJSONだけで回答する。\n'
        + '\n'.join(f'検証{i:04}: 追加の決定変更なし。' for i in range(520)) + '\n')
    benchmark.PROMPTS = {'retention': (
        'readでREADME.mdを読み、各資料のnextに従って次の資料を順に読んでください。'
        '最終資料の質問に、取得済みの根拠を確認して回答してください。変更は禁止。'
        '保存された結果を参照する場合も、古い決定・別台帳・未承認提案を混同しないでください。')}
    benchmark.quality = lambda task, stdout, fixture: benchmark.final_json(stdout) == {
        'retention_days': 37, 'decision_id': 'R017'}


def main():
    setup()
    benchmark.main()
    output = Path(sys.argv[sys.argv.index('--output') + 1])
    reports = []
    for path in sorted(output.glob('*.audit.jsonl')):
        entries = [json.loads(line) for line in path.read_text().splitlines()]
        stored = sum(int(match.group(1)) for entry in entries
                     if entry.get('tool') == 'tool-memory'
                     for match in [re.search(r'stored=(\d+)', entry.get('result', ''))] if match)
        restored = sum(entry.get('tool') == 'read' and 'memory://' in entry.get('detail', '')
                       for entry in entries)
        reports.append({'run': path.name, 'stored_results': stored, 'memory_reads': restored})
    (output / 'exercise.json').write_text(json.dumps(reports, ensure_ascii=False, indent=2))
    print('機能の使用状況: ' + json.dumps(reports, ensure_ascii=False))


if __name__ == '__main__':
    main()
