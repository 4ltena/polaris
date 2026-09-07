#!/usr/bin/env python3
"""固定 v0.9 と候補を Algedi 課題で交互に比較する。"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--timeout', type=int, default=600)
    parser.add_argument('--dry-run', action='store_true')
    args = parser.parse_args()
    if not 1 <= args.repeats <= 5 or not 1 <= args.timeout <= 600:
        parser.error('repeatsは1〜5、timeoutは1〜600秒')
    candidate = args.candidate.resolve(strict=True)
    baseline = args.baseline.resolve(strict=True)
    output = args.output.resolve()
    if output.exists() and not args.dry_run:
        parser.error(f'出力先が既に存在します: {output}')
    runner = Path(__file__).with_name('benchmark_algedi.py')
    if not args.dry_run:
        output.mkdir(parents=True, mode=0o700)
    conditions = (('baseline-retrieval', baseline), ('candidate-retrieval', candidate))
    for round_index in range(args.repeats):
        order = conditions if round_index % 2 == 0 else tuple(reversed(conditions))
        for label, binary in order:
            command = [sys.executable, str(runner), '--binary', str(binary), '--output',
                       str(output / f'round-{round_index + 1}-{label}'), '--label', label,
                       '--repeats', '1', '--timeout', str(args.timeout), '--tool-memory', 'retrieval',
                       '--baseline', str(baseline)]
            print(' '.join(command), flush=True)
            if not args.dry_run:
                if subprocess.run(command).returncode:
                    raise SystemExit(f'比較を停止しました。保存済みの結果を確認してください: {output}')
    if args.dry_run:
        return
    summary = {}
    for path in sorted(output.glob('round-*/results.jsonl')):
        condition = path.parent.name.split('-', 2)[2]
        for line in path.read_text().splitlines():
            row = json.loads(line); key = f'{condition}:{row["task"]}'
            bucket = summary.setdefault(key, {'condition': condition, 'task': row['task'], 'runs': 0,
                                              'quality_passes': 0, 'scope_passes': 0, 'complete_runs': 0,
                                              'quality_unassessed_runs': 0,
                                              'known_total_tokens': 0, 'missing_usage_runs': 0,
                                              'combined_known_tokens': 0, 'embedding_missing': 0,
                                              'elapsed_seconds': 0,
                                              'stored_results': 0, 'memory_reads': 0})
            bucket['runs'] += 1; bucket['quality_passes'] += int(row['quality_pass'])
            bucket['scope_passes'] += int(row['scope_pass']); bucket['complete_runs'] += int(row['measurement_complete'])
            bucket['quality_unassessed_runs'] += int(row['quality_assessment'] == 'unassessed')
            if row['usage'] is None: bucket['missing_usage_runs'] += 1
            else: bucket['known_total_tokens'] += row['usage']['total']
            if row['combined_known_tokens'] is not None:
                bucket['combined_known_tokens'] += row['combined_known_tokens']
            bucket['embedding_missing'] += row['embedding_usage']['missing']
            bucket['elapsed_seconds'] += row['elapsed_seconds']
            bucket['stored_results'] += row['exercise']['stored_results']
            bucket['memory_reads'] += row['exercise']['memory_reads']
    metadata = {'baseline': str(baseline), 'baseline_sha256': hashlib.sha256(baseline.read_bytes()).hexdigest(),
                'candidate': str(candidate), 'candidate_sha256': hashlib.sha256(candidate.read_bytes()).hexdigest(),
                'repeats': args.repeats, 'tool_memory': 'retrieval'}
    (output / 'metadata.json').write_text(json.dumps(metadata, ensure_ascii=False, indent=2))
    (output / 'summary.json').write_text(json.dumps(list(summary.values()), ensure_ascii=False, indent=2))


if __name__ == '__main__':
    main()
