#!/usr/bin/env python3
"""停止した Algedi 測定を、保存済みの行を再評価してから限定再開する。"""
import argparse
import hashlib
import json
from pathlib import Path
import shlex
import subprocess
import sys
import uuid

import benchmark_algedi as benchmark


OLD_DIRECTORY = 'round-1-baseline-retrieval'
FIRST_TASK = 'singlefile_repair'
CONDITIONS = ('baseline-retrieval', 'candidate-retrieval')
BENCHMARK = Path(__file__).with_name('benchmark_algedi.py')


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def fail(message):
    raise SystemExit(f'再開を停止しました: {message}')


def rows(path):
    try:
        parsed = [json.loads(line) for line in path.read_text().splitlines()]
    except (OSError, ValueError) as error:
        fail(f'結果を読めません: {path} ({type(error).__name__})')
    if not parsed or not all(isinstance(row, dict) for row in parsed):
        fail(f'結果が空または不正です: {path}')
    return parsed


def expected_fixture(task):
    expected = {
        '.polaris': ('directory', ''),
        **{name: ('file', hashlib.sha256(body.encode()).hexdigest())
           for name, body in benchmark.task_files(task).items()},
    }
    if task == 'changed_source_evidence':
        body = '現行時点: policy_version=4; retention_days=45\n'
        expected['current_source.md'] = ('file', hashlib.sha256(body.encode()).hexdigest())
    return expected


def validate_saved_row(directory, row, baseline, candidate, condition, expected_task, changed_paths, unassessed_reason):
    task, repeat = row.get('task'), row.get('repeat')
    if task != expected_task or repeat != 1:
        fail('保存済み行の課題または反復回数が一致しません')
    if (row.get('quality_pass') is not False or row.get('quality_assessment') != 'unassessed'
            or row.get('quality_unassessed_reason') != unassessed_reason):
        fail('この限定再開器が扱える未評価形状と一致しません')
    metadata_path = directory / 'metadata.json'
    try:
        metadata = json.loads(metadata_path.read_text())
    except (OSError, ValueError) as error:
        fail(f'メタデータを読めません: {type(error).__name__}')
    if not isinstance(metadata, dict) or metadata.get('label') != condition:
        fail('保存済み行の条件ラベルが一致しません')
    original = Path(metadata.get('baseline_binary', '')).expanduser()
    if not original.is_file() or original.is_symlink():
        fail('記録済み比較元バイナリを安全に検証できません')
    baseline_hash, candidate_hash = sha256(baseline), sha256(candidate)
    expected_binary_hash = baseline_hash if condition == 'baseline-retrieval' else candidate_hash
    if (sha256(original) != metadata.get('baseline_sha256')
            or metadata.get('binary_sha256') != expected_binary_hash
            or metadata.get('baseline_sha256') != baseline_hash
            or candidate_hash == baseline_hash):
        fail('記録済みバイナリの SHA256 が現在の指定値と一致しません')
    if metadata.get('model') != 'gpt-6-astra' or metadata.get('effort') != 'medium' or metadata.get('tool_memory') != 'retrieval':
        fail('初回行の固定条件が一致しません')

    fixture = directory / f'{task}-{repeat}.fixture'
    if fixture.is_symlink() or not fixture.is_dir():
        fail('保存済み fixture が通常ディレクトリではありません')
    before, after = expected_fixture(task), benchmark.snapshot(fixture)
    if any(kind == 'link' for kind, _ in after.values()):
        fail('保存済み fixture に symlink があります')
    changed = sorted(key for key in before.keys() | after.keys() if before.get(key) != after.get(key))
    unchanged = [name for name in before if name not in changed and after.get(name) != before[name]]
    if (after.keys() != before.keys() or unchanged or any(after.get(name, ('',))[0] != 'file' for name in changed_paths)
            or changed != row.get('changed_paths') or changed != changed_paths):
        fail('保存済み fixture の範囲または変更記録が一致しません')

    stdout = (directory / f'{task}-{repeat}.out').read_text()
    stderr = (directory / f'{task}-{repeat}.err').read_text()
    totals, traces, model_match, coverage, embedding, complete = benchmark.parse_metrics(
        directory / f'{task}-{repeat}.metrics.jsonl', stderr)
    assessment, reason = benchmark.quality_result(task, stdout, fixture)
    if assessment != 'pass' or reason is not None:
        fail(f'保存済み行を再評価できません: {assessment} {reason or ""}'.strip())
    if (row.get('exit_code') != 0 or row.get('usage') != totals
            or row.get('embedding_usage') != embedding or row.get('recorded_model_match') != model_match
            or row.get('cli_coverage') != coverage or row.get('metrics_recorded') != bool(traces)
            or row.get('measurement_complete') is not True or not complete
            or row.get('scope_pass') is not True or totals is None or totals.get('total', 0) <= 0):
        fail('保存済み行の使用量、完全性、終了状態または範囲が一致しません')
    reassessed = dict(row)
    reassessed.update(original_quality_pass=row.get('quality_pass'),
                       original_quality_assessment=row.get('quality_assessment'),
                       original_quality_unassessed_reason=row.get('quality_unassessed_reason'),
                       original_comparison_status=row.get('comparison_status'),
                       quality_pass=True, quality_assessment='pass', quality_unassessed_reason=None,
                       comparison_status='complete', condition=condition,
                       reassessed_quality='pass', reassessment_reason='current_quality_result')
    return reassessed


def validate_completed_row(directory, row, baseline, candidate, condition, task):
    if (row.get('quality_pass') is not True or row.get('quality_assessment') != 'pass'
            or row.get('quality_unassessed_reason') is not None or row.get('comparison_status') != 'complete'):
        fail('保存済み完了行の品質状態が一致しません')
    expected_changes = {'singlefile_repair': ['rates.py'], 'multifile_repair': ['pricing.py', 'receipt.py']}.get(task, [])
    probe = dict(row, quality_pass=False, quality_assessment='unassessed',
                 quality_unassessed_reason='completed_prefix_probe')
    validate_saved_row(directory, probe, baseline, candidate, condition, task,
                       expected_changes, 'completed_prefix_probe')
    return dict(row, condition=condition, reassessed_quality='pass', reassessment_reason='recorded_quality_result')


def validate_initial_layout(output):
    if output.is_symlink() or not output.is_dir():
        fail('出力先は既存の通常ディレクトリでなければなりません')
    names = {path.name for path in output.iterdir()}
    if OLD_DIRECTORY not in names or any(not (name == OLD_DIRECTORY or name.startswith('resume-')) for name in names):
        fail('限定再開の対象外の既存 artifact または未完了 artifact があります')
    directory = output / OLD_DIRECTORY
    required = {'metadata.json', 'results.jsonl', f'{FIRST_TASK}-1.fixture',
                f'{FIRST_TASK}-1.out', f'{FIRST_TASK}-1.err', f'{FIRST_TASK}-1.metrics.jsonl'}
    if {path.name for path in directory.iterdir()} != required | {f'{FIRST_TASK}-1.audit.jsonl'}:
        fail('初回ディレクトリに想定外または不足 artifact があります')
    return directory


def plan():
    return ((1, 'baseline-retrieval'), (1, 'candidate-retrieval'),
            (2, 'candidate-retrieval'), (2, 'baseline-retrieval'),
            (3, 'baseline-retrieval'), (3, 'candidate-retrieval'))


def planned_rows():
    return [(number, condition, task) for number, condition in plan() for task in benchmark.TASKS]


def directory_key(path):
    for number, condition in plan():
        prefix = f'round-{number}-{condition}'
        if path.name == prefix or path.name == prefix + '-remaining':
            return number, condition
    fail(f'保存済み再開の round 名が予定表と一致しません: {path.name}')


def validate_existing_resume(output, old, baseline, candidate):
    resumes = [path for path in output.iterdir() if path.name.startswith('resume-')]
    if not resumes:
        return None, []
    if len(resumes) != 1 or resumes[0].is_symlink() or not resumes[0].is_dir():
        fail('既存の再開 artifact を一意かつ安全に検証できません')
    continuation = resumes[0]
    fixed = {'reassessment.jsonl', 'resume-metadata.json'}
    names = {path.name for path in continuation.iterdir()}
    if not fixed <= names or any(name not in fixed and not name.startswith('round-') for name in names):
        fail('既存の再開 artifact に想定外または不足 artifact があります')
    reassessment = rows(continuation / 'reassessment.jsonl')
    if reassessment != [{key: value for key, value in old.items() if key != 'round'}]:
        fail('既存の再評価行が元の検証結果と一致しません')
    try:
        metadata = json.loads((continuation / 'resume-metadata.json').read_text())
    except (OSError, ValueError) as error:
        fail(f'既存の再開メタデータを読めません: {type(error).__name__}')
    if metadata != {'baseline': str(baseline), 'baseline_sha256': sha256(baseline),
                    'candidate': str(candidate), 'candidate_sha256': sha256(candidate),
                    'source_results': str(output), 'repeats': 3, 'pending_task_invocations': 29}:
        fail('既存の再開メタデータまたは SHA256 が一致しません')
    directories = sorted((path for path in continuation.iterdir() if path.name.startswith('round-')),
                         key=lambda path: (plan().index(directory_key(path)), path.name.endswith('-remaining')))
    recorded = []
    for directory in directories:
        number, condition = directory_key(directory)
        result = rows(directory / 'results.jsonl')
        expected_names = {'metadata.json', 'results.jsonl'}
        for row in result:
            task, repeat = row.get('task'), row.get('repeat')
            expected_names |= {f'{task}-{repeat}.{suffix}' for suffix in ('out', 'err', 'metrics.jsonl', 'audit.jsonl')}
            expected_names.add(f'{task}-{repeat}.fixture')
            if row.get('quality_assessment') == 'unassessed':
                changed = {'singlefile_repair': ['rates.py'], 'multifile_repair': ['pricing.py', 'receipt.py']}.get(task, [])
                recorded.append(dict(validate_saved_row(directory, row, baseline, candidate, condition, task, changed,
                                                        row.get('quality_unassessed_reason')), round=number))
            else:
                recorded.append(dict(validate_completed_row(directory, row, baseline, candidate, condition, task), round=number))
        if {path.name for path in directory.iterdir()} != expected_names:
            fail(f'保存済み再開の artifact 集合が一致しません: {directory.name}')
    prefix = [(row['round'], row['condition'], row['task']) for row in [old, *recorded]]
    expected_prefix = planned_rows()
    if prefix != expected_prefix[:len(prefix)]:
        fail('保存済み再開行は予定表の連続した完了プレフィックスではありません')
    return continuation, recorded


def schedule(baseline, candidate, continuation, timeout, reused):
    completed = [(row['round'], row['condition'], row['task']) for row in reused]
    expected = planned_rows()
    if completed != expected[:len(completed)]:
        fail('再利用行は予定表の連続した完了プレフィックスではありません')
    pending = planned_rows()[len(completed):]
    rounds = []
    for number, condition in plan():
        tasks = [task for planned_number, planned_condition, task in pending
                 if (planned_number, planned_condition) == (number, condition)]
        rounds.append((number, ((condition, baseline if condition == 'baseline-retrieval' else candidate, tasks),)))
    commands = []
    for number, entries in rounds:
        for label, binary, tasks in entries:
            if not tasks:
                continue
            directory_name = f'round-{number}-{label}'
            if (continuation / directory_name).exists():
                directory_name += '-remaining'
                if (continuation / directory_name).exists():
                    fail(f'継続出力先が既にあります: {directory_name}')
            commands.append([sys.executable, str(BENCHMARK), '--binary', str(binary), '--baseline', str(baseline),
                             '--output', str(continuation / directory_name), '--label', label,
                             '--repeats', '1', '--timeout', str(timeout), '--tool-memory', 'retrieval', '--tasks', *tasks])
    return commands


def validate_new_rows(continuation, commands):
    combined = []
    for command in commands:
        directory = Path(command[command.index('--output') + 1])
        label = command[command.index('--label') + 1]
        requested = command[command.index('--tasks') + 1:]
        result = rows(directory / 'results.jsonl')
        if len(result) != len(requested) or {row.get('task') for row in result} != set(requested):
            fail(f'継続結果の課題集合が一致しません: {directory.name}')
        for row in result:
            if (row.get('exit_code') != 0 or row.get('quality_pass') is not True
                    or row.get('scope_pass') is not True or row.get('measurement_complete') is not True
                    or row.get('comparison_status') != 'complete' or row.get('usage') is None
                    or row['usage'].get('total', 0) <= 0):
                fail(f'継続結果が完全ではありません: {directory.name}/{row.get("task")}')
            combined.append(dict(row, condition=label, source='continuation'))
    return combined


def write_new(path, content):
    with path.open('x') as handle:
        handle.write(content)


def summarize(rows_to_summarize):
    summary = []
    for condition in CONDITIONS:
        for task in benchmark.TASKS:
            selected = [row for row in rows_to_summarize if row['condition'] == condition and row['task'] == task]
            if len(selected) != 3:
                fail(f'{condition}:{task} の件数が 3 ではありません')
            summary.append({'condition': condition, 'task': task, 'runs': len(selected),
                            'quality_passes': sum(row['quality_pass'] for row in selected),
                            'scope_passes': sum(row['scope_pass'] for row in selected),
                            'complete_runs': sum(row['measurement_complete'] for row in selected),
                            'quality_unassessed_runs': sum(row.get('quality_assessment') == 'unassessed' for row in selected),
                            'known_total_tokens': sum(row['usage']['total'] for row in selected),
                            'missing_usage_runs': sum(row.get('usage') is None for row in selected),
                            'combined_known_tokens': sum(row.get('combined_known_tokens', row['usage']['total']) for row in selected),
                            'embedding_missing': sum(row.get('embedding_usage', {}).get('missing', 0) for row in selected),
                            'elapsed_seconds': sum(row.get('elapsed_seconds', 0) for row in selected),
                            'stored_results': sum(row.get('exercise', {}).get('stored_results', 0) for row in selected),
                            'memory_reads': sum(row.get('exercise', {}).get('memory_reads', 0) for row in selected)})
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', '--source-results', dest='source_results', type=Path, required=True,
                        help='再評価する既存の results-* ルート')
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--timeout', type=int, default=600)
    parser.add_argument('--dry-run', action='store_true')
    args = parser.parse_args()
    if args.repeats != 3 or not 1 <= args.timeout <= 600:
        parser.error('この限定再開器は repeats=3、timeout=1〜600秒だけを受け付けます')
    if any(path.is_symlink() for path in (args.source_results, args.baseline, args.candidate)):
        fail('source-results とバイナリに symlink は使えません')
    output, baseline, candidate = args.source_results.resolve(strict=True), args.baseline.resolve(strict=True), args.candidate.resolve(strict=True)
    if any(path.is_symlink() or not path.is_file() for path in (baseline, candidate)):
        fail('バイナリは通常ファイルでなければなりません')
    old_directory = validate_initial_layout(output)
    old_rows = rows(old_directory / 'results.jsonl')
    if len(old_rows) != 1:
        fail('この限定再開器は保存済みの初回一行だけを扱います')
    old = dict(validate_saved_row(old_directory, old_rows[0], baseline, candidate,
                                  'baseline-retrieval', FIRST_TASK, ['rates.py'], 'unsupported_syntax'), round=1)
    continuation, resumed = validate_existing_resume(output, old, baseline, candidate)
    reused = [old, *resumed]
    continuation = continuation or output / f'resume-{uuid.uuid4().hex}'
    commands = schedule(baseline, candidate, continuation, args.timeout, reused)
    print(f'保存済み {len(reused)} 行: 再評価 pass、使用量・fixture・SHA256 を確認済み')
    for command in commands:
        print(shlex.join(command))
    if args.dry_run:
        return
    if not resumed:
        continuation.mkdir(mode=0o700)
        write_new(continuation / 'reassessment.jsonl', json.dumps(old, ensure_ascii=False) + '\n')
        write_new(continuation / 'resume-metadata.json', json.dumps({
            'baseline': str(baseline), 'baseline_sha256': sha256(baseline),
            'candidate': str(candidate), 'candidate_sha256': sha256(candidate),
            'source_results': str(output), 'repeats': 3, 'pending_task_invocations': 29,
        }, ensure_ascii=False, indent=2) + '\n')
    try:
        for command in commands:
            subprocess.run(command, check=True)
    except subprocess.CalledProcessError as error:
        fail(f'継続測定が失敗しました: {Path(error.cmd[error.cmd.index("--output") + 1]).name} (exit {error.returncode})')
    new_rows = validate_new_rows(continuation, commands)
    all_rows = [dict(row, source='reassessed-original' if row is old else 'reassessed-resume') for row in reused] + new_rows
    summary = summarize(all_rows)
    write_new(output / 'all-results.jsonl', ''.join(json.dumps(row, ensure_ascii=False) + '\n' for row in all_rows))
    write_new(output / 'summary.json', json.dumps(summary, ensure_ascii=False, indent=2) + '\n')


if __name__ == '__main__':
    main()
