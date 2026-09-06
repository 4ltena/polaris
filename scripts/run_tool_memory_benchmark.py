#!/usr/bin/env python3
"""GPT-6 mediumで原文再取得の3条件を順番を交代して測定する。"""
import argparse
from datetime import datetime
import hashlib
import json
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--embedding-url')
    parser.add_argument('--embedding-model')
    parser.add_argument('--dry-run', action='store_true')
    args = parser.parse_args()
    if bool(args.embedding_url) != bool(args.embedding_model):
        parser.error('埋め込みURLとモデルは両方指定してください')
    binary = args.binary.resolve(strict=True)
    runner = Path(__file__).with_name('benchmark_tool_memory.py')
    conditions = [('off', 'off'), ('history', 'history'), ('retrieval', 'retrieval')]
    if args.embedding_url:
        conditions.append(('semantic', 'retrieval'))
    schedule = [(round_index + 1, label, mode)
                for round_index in range(3)
                for label, mode in conditions[round_index:] + conditions[:round_index]]
    if args.dry_run:
        print(json.dumps({'model': 'gpt-6-astra', 'effort': 'medium', 'schedule': schedule}, ensure_ascii=False))
        return
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    (output / 'schedule.json').write_text(json.dumps({
        'schedule': schedule, 'model': 'gpt-6-astra', 'effort': 'medium',
        'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
        'started_at': datetime.now().isoformat(),
    }, ensure_ascii=False, indent=2))
    rows = []
    print('保存先: ' + str(output), flush=True)
    for round_index, label, mode in schedule:
        destination = output / f'round-{round_index}-{label}'
        command = [sys.executable, str(runner), '--binary', str(binary),
                   '--output', str(destination), '--label', label,
                   '--tool-memory', mode, '--repeats', '1', '--timeout', '600']
        if label == 'semantic':
            command.extend(['--tool-memory-embedding-url', args.embedding_url,
                            '--tool-memory-embedding-model', args.embedding_model])
        if subprocess.run(command).returncode:
            raise SystemExit(f'測定停止。保存済み結果を確認してください: {output}')
        record = json.loads((destination / 'results.jsonl').read_text().strip())
        exercise = json.loads((destination / 'exercise.json').read_text())[0]
        record.update(round=round_index, label=label, exercise=exercise)
        rows.append(record)
        with (output / 'all-results.jsonl').open('a') as file:
            file.write(json.dumps(record, ensure_ascii=False) + '\n')
    summary = []
    for label, _ in conditions:
        selected = [r for r in rows if r['label'] == label]
        summary.append({
            'condition': label, 'runs': len(selected),
            'model_tokens': sum(r['usage']['total'] for r in selected),
            'combined_known_tokens': sum(r['combined_known_tokens'] for r in selected),
            'quality_passes': sum(r['quality_pass'] for r in selected),
            'complete_runs': sum(r['measurement_complete'] for r in selected),
            'stored_results': sum(r['exercise']['stored_results'] for r in selected),
            'memory_reads': sum(r['exercise']['memory_reads'] for r in selected),
        })
    (output / 'summary.json').write_text(json.dumps(summary, ensure_ascii=False, indent=2))
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    print('結果: ' + str(output))


if __name__ == '__main__':
    main()
