#!/usr/bin/env python3
"""GPT-6 mediumで固定課題を実測し、使用量と正答を別々に記録する。"""
import argparse
import ast
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import time
import uuid

README = '''# Parcel Ledger
入力はUTF-8 CSV。列はid、weight_g、destination。同一id、負の重量、未知の配送先は入力エラー。
配送先は東京と大阪。1バッチ200件。全検証成功まで結果を書き込まない。
送料は基本500円。1000gを超える重量は1000gごと200円、端数は切り上げ。
保存はJSONL。ネットワーク不使用。終了コードは成功0、入力エラー2。
'''
RATES = '''def shipping_fee(weight_g):
    if weight_g < 0:
        raise ValueError('negative weight')
    extra = max(0, weight_g - 1000)
    return 500 + (extra // 1000) * 200
'''
PROMPTS = {
    'overview': 'README.mdを読み、要件を確認してください。変更は禁止。最終回答はJSONだけで、{"batch_limit":整数,"destinations":[文字列],"format":文字列,"error_exit":整数,"atomic":真偽値}を返してください。',
    'repair': 'README.mdとrates.pyを確認し、仕様と異なる送料計算だけをrates.py内で直してください。負の重量の例外を維持してください。変更はrates.pyだけ。最後は修正の要点を一文で返してください。',
    'investigate': 'README.mdとhistory.mdを調べ、最終承認済みのバッチ上限、再試行回数、保持日数を特定してください。古い決定や未承認提案を採用せず、変更は禁止。最終回答はJSONだけで{"batch_limit":整数,"retry_limit":整数,"retention_days":整数,"decision_id":文字列}を返してください。',
}
HISTORY = ('# 決定履歴\n決定D001: 承認済み。バッチ上限100、再試行2回、保持7日。\n'
           + '\n'.join(f'調査記録{i:03d}: 計測を確認。設定変更なし。' for i in range(160))
           + '\n決定D002: 承認済み。バッチ上限200、再試行3回、保持30日。D001を置き換える。\n'
           + '\n'.join(f'監視記録{i:03d}: 通常稼働。変更なし。' for i in range(160))
           + '\n提案P003: 未承認。バッチ上限500、再試行5回、保持90日。適用禁止。\n')


def task_prompt(task, strategy):
    prompt = PROMPTS[task]
    if strategy != 'default':
        prompt += ('\n作業領域の資料はREADME.md、rates.py、history.mdのみ。'
                   'docs/filemap.mdとGitリポジトリは存在しないため、探索・Git操作は不要。'
                   'Pythonで検証する場合はpython3を使い、PYTHONDONTWRITEBYTECODE=1を指定する。')
    if strategy == 'search-first' and task == 'investigate':
        prompt += ('\n長い履歴はまず決定・承認・置換に関連する行を前後の文脈付きで検索し、'
                   '必要なら該当範囲を追加取得する。根拠が揃う前に回答しない。')
    return prompt


def final_json(text):
    for start, char in enumerate(text):
        if char == '{':
            try:
                value, _ = json.JSONDecoder().raw_decode(text[start:])
                if isinstance(value, dict):
                    return value
            except ValueError:
                pass
    return None


def quality(task, stdout, fixture):
    if task == 'repair':
        try:
            tree = ast.parse((fixture / 'rates.py').read_text())
            allowed = (ast.Module, ast.FunctionDef, ast.arguments, ast.arg, ast.If,
                       ast.Compare, ast.Lt, ast.LtE, ast.Gt, ast.GtE, ast.Eq, ast.NotEq,
                       ast.Name, ast.Load, ast.Store, ast.Constant, ast.Raise, ast.Call,
                       ast.Assign, ast.Return, ast.BinOp, ast.Add, ast.Sub, ast.Mult,
                       ast.FloorDiv, ast.Mod, ast.UnaryOp, ast.USub)
            if any(not isinstance(n, allowed) for n in ast.walk(tree)):
                return False
            if any(isinstance(n, ast.Call) and
                   (not isinstance(n.func, ast.Name) or n.func.id not in ('max', 'ValueError'))
                   for n in ast.walk(tree)):
                return False
            if len(tree.body) != 1 or not isinstance(tree.body[0], ast.FunctionDef):
                return False
            scope = {'__builtins__': {}, 'max': max, 'ValueError': ValueError}
            exec(compile(tree, 'rates.py', 'exec'), scope)
            f = scope['shipping_fee']
            if [f(x) for x in (0, 1000, 1001, 2000, 2001)] != [500, 500, 700, 700, 900]:
                return False
            try:
                f(-1)
            except ValueError:
                return True
            return False
        except Exception:
            return False
    value = final_json(stdout)
    if task == 'overview':
        return isinstance(value, dict) and value.get('batch_limit') == 200 and value.get('destinations') in (['東京', '大阪'], ['大阪', '東京']) and str(value.get('format', '')).upper() == 'JSONL' and value.get('error_exit') == 2 and value.get('atomic') is True
    return value == {'batch_limit': 200, 'retry_limit': 3, 'retention_days': 30, 'decision_id': 'D002'}


def snapshot(root):
    """Do not follow links created by a benchmark run."""
    result = {}
    for directory, dirs, files in os.walk(root, followlinks=False):
        for name in dirs + files:
            path = Path(directory) / name
            mode = path.lstat().st_mode
            key = str(path.relative_to(root))
            if stat.S_ISLNK(mode):
                result[key] = ('link', os.readlink(path))
            elif stat.S_ISREG(mode):
                result[key] = ('file', hashlib.sha256(path.read_bytes()).hexdigest())
            else:
                result[key] = ('directory' if stat.S_ISDIR(mode) else 'special', '')
    return result


def embedding_usage(stderr):
    """Count only explicit backend attempt records; unknown usage stays unknown."""
    result = {'requests': 0, 'known_input_tokens': 0, 'missing': 0, 'failed': 0}
    for line in stderr.splitlines():
        if not line.startswith('tool-memory-embedding: '):
            continue
        result['requests'] += 1
        try:
            record = json.loads(line.removeprefix('tool-memory-embedding: '))
        except ValueError:
            record = {}
        if not isinstance(record, dict):
            record = {}
        value = record.get('input_tokens')
        if type(value) is int and value >= 0:
            result['known_input_tokens'] += value
        else:
            result['missing'] += 1
        if record.get('ok') is not True:
            result['failed'] += 1
    return result


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--binary', type=Path, required=True)
    ap.add_argument('--output', type=Path, required=True)
    ap.add_argument('--label', required=True)
    ap.add_argument('--repeats', type=int, default=3)
    ap.add_argument('--tasks', nargs='+', choices=list(PROMPTS), default=list(PROMPTS))
    ap.add_argument('--timeout', type=int, default=180)
    ap.add_argument('--read-output-bytes', type=int)
    ap.add_argument('--cache-mode', choices=['implicit', 'explicit'])
    ap.add_argument('--tool-memory', choices=['off', 'history', 'retrieval'], default='off')
    ap.add_argument('--tool-memory-embedding-url')
    ap.add_argument('--tool-memory-embedding-model')
    ap.add_argument('--strategy', choices=['default', 'scoped', 'search-first'], default='default')
    ap.add_argument('--allow-legacy-metrics', action='store_true',
                    help='旧バイナリの計測ログ欠落を許容する。比較完全性は未確認と記録する')
    args = ap.parse_args()
    if args.tool_memory_embedding_url is not None or args.tool_memory_embedding_model is not None:
        if (args.tool_memory == 'off' or not (args.tool_memory_embedding_url or '').strip()
                or not (args.tool_memory_embedding_model or '').strip()):
            ap.error('埋め込みURLとモデルを両方指定し、tool-memoryをhistoryまたはretrievalにしてください')
    if not 1 <= args.repeats <= 10 or not 1 <= args.timeout <= 600:
        ap.error('repeatsは1〜10、timeoutは1〜600秒')
    binary = args.binary.resolve(strict=True)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    fixture = output / 'fixture'
    metadata = {'model': 'gpt-6-astra', 'effort': 'medium', 'label': args.label,
                'strategy': args.strategy, 'tool_memory': args.tool_memory,
                'prompts': {task: task_prompt(task, args.strategy) for task in args.tasks},
                'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                'fixture_sha256': hashlib.sha256((README + RATES + HISTORY).encode()).hexdigest(),
                'repeats': args.repeats, 'cache_warmth': '同一課題内はキーを共有。cold/warmは実測ログで確認し、保証しない。'}
    (output / 'metadata.json').write_text(json.dumps(metadata, ensure_ascii=False, indent=2))
    for task in args.tasks:
        namespace = uuid.uuid4().hex
        for repeat in range(args.repeats):
            fixture.mkdir(mode=0o700)
            (fixture / '.polaris').mkdir(mode=0o700)
            # resolve_root selects the nearest .git/.polaris marker.
            if not (fixture / '.polaris').is_dir() or fixture.resolve() != fixture:
                raise SystemExit('測定用ルートの隔離を確認できません。')
            for name, body in [('README.md', README), ('rates.py', RATES), ('history.md', HISTORY)]:
                (fixture / name).write_text(body)
            before = snapshot(fixture)
            stem = f'{task}-{repeat + 1}'
            metrics = output / f'{stem}.metrics.jsonl'
            env = os.environ.copy()
            for key in ('POLARIS_CACHE_MODE', 'POLARIS_READ_OUTPUT_BYTES'):
                env.pop(key, None)
            env.update(POLARIS_PROVIDER='codex', POLARIS_METRICS_PATH=str(metrics), POLARIS_CACHE_NAMESPACE=namespace)
            if args.read_output_bytes is not None:
                env['POLARIS_READ_OUTPUT_BYTES'] = str(args.read_output_bytes)
            if args.cache_mode:
                env['POLARIS_CACHE_MODE'] = args.cache_mode
            cmd = [str(binary), '--model', 'gpt-6-astra', '--effort', 'medium',
                   '--sandbox', 'workspace-write' if task == 'repair' else 'read-only',
                   '--approval', 'never', '--max-turns', '12',
                   '--audit', str(output / f'{stem}.audit.jsonl'), '--prompt', task_prompt(task, args.strategy)]
            if args.tool_memory != 'off':
                cmd.extend(['--tool-memory', args.tool_memory])
            if args.tool_memory_embedding_url is not None:
                cmd.extend(['--tool-memory-embedding-url', args.tool_memory_embedding_url,
                            '--tool-memory-embedding-model', args.tool_memory_embedding_model])
            start = time.monotonic()
            try:
                result = subprocess.run(cmd, cwd=fixture, env=env, capture_output=True,
                                        text=True, timeout=args.timeout, stdin=subprocess.DEVNULL)
                stdout, stderr, code = result.stdout, result.stderr, result.returncode
            except subprocess.TimeoutExpired as exc:
                stdout = exc.stdout or b''
                stderr = exc.stderr or b''
                stdout = stdout.decode(errors='replace') if isinstance(stdout, bytes) else stdout
                stderr = stderr.decode(errors='replace') if isinstance(stderr, bytes) else stderr
                code = None
            (output / f'{stem}.out').write_text(stdout)
            (output / f'{stem}.err').write_text(stderr)
            usage = re.search(r'tokens: in (\d+) / out (\d+) / cache (\d+) / total (\d+)', stderr)
            try:
                traces = [json.loads(line) for line in metrics.read_text().splitlines()] if metrics.exists() else []
                if not all(isinstance(r, dict) for r in traces):
                    traces = []
            except (ValueError, OSError):
                traces = []
            model_match = all(r.get('model') == 'gpt-6-astra' and r.get('effort') == 'medium' for r in traces) if traces else None
            after = snapshot(fixture)
            changed = sorted(k for k in before.keys() | after.keys() if before.get(k) != after.get(k))
            scope_ok = all(k == 'rates.py' for k in changed) if task == 'repair' else not changed
            missing = sum(r.get('usage_missing') is not False for r in traces)
            failed = sum(r.get('outcome') != 'completed' for r in traces)
            totals = dict(zip(('input', 'output', 'cache', 'total'), map(int, usage.groups()))) if usage else None
            fields = {'input': 'input_tokens', 'output': 'output_tokens', 'cache': 'cache_read_tokens'}
            trace_totals = {k: sum((r.get('usage') or {}).get(v) or 0 for r in traces) for k, v in fields.items()}
            reconciled = totals is not None and all(totals[k] == trace_totals[k] for k in fields)
            cache_complete = bool(traces) and all((r.get('usage') or {}).get('cache_read_tokens') is not None for r in traces)
            cache_write_complete = bool(traces) and all((r.get('usage') or {}).get('cache_write_tokens') is not None for r in traces)
            coverage_match = re.search(r'使用量の計測: 応答あり (\d+) / 欠測 (\d+) / 失敗 (\d+)', stderr)
            coverage = dict(zip(('reported', 'missing', 'failed'), map(int, coverage_match.groups()))) if coverage_match else None
            coverage_ok = coverage is not None and coverage == {'reported': len(traces), 'missing': 0, 'failed': 0}
            log_warning = '診断ログの保存に失敗' in stderr
            embedding = embedding_usage(stderr)
            embedding_complete = embedding['missing'] == 0 and embedding['failed'] == 0
            complete = embedding_complete and bool(traces) and model_match is True and missing == 0 and failed == 0 and reconciled and cache_complete and coverage_ok and not log_warning
            row = {'task': task, 'repeat': repeat + 1, 'exit_code': code,
                   'strategy': args.strategy, 'tool_memory': args.tool_memory,
                   'elapsed_seconds': round(time.monotonic() - start, 3),
                   'quality_pass': code == 0 and scope_ok and quality(task, stdout, fixture),
                   'scope_pass': scope_ok, 'changed_paths': changed,
                   'usage': totals, 'trace_totals_match': reconciled,
                   'embedding_usage': embedding,
                   'embedding_measurement_complete': embedding_complete,
                   'combined_known_tokens': (totals['total'] + embedding['known_input_tokens']) if totals else None,
                   'cache_read_measurement_complete': cache_complete,
                   'cache_write_measurement_complete': cache_write_complete,
                   'cost_comparison_ready': complete and cache_write_complete,
                   'cli_coverage': coverage, 'coverage_matches': coverage_ok,
                   'diagnostic_write_warning': log_warning,
                   'metrics_recorded': bool(traces), 'recorded_model_match': model_match,
                   'http_attempts': len(traces) if traces else None,
                   'usage_missing_attempts': missing if traces else None,
                   'failed_attempts': failed if traces else None,
                   'measurement_complete': complete,
                   'comparison_status': 'complete' if complete else 'incomplete'}
            # Preserve evidence and recreate the same path with pristine inputs.
            fixture.rename(output / f'{stem}.fixture')
            with (output / 'results.jsonl').open('a') as f:
                f.write(json.dumps(row, ensure_ascii=False) + '\n')
            print(json.dumps(row, ensure_ascii=False), flush=True)
            legacy = args.allow_legacy_metrics and not traces
            if code != 0 or row['usage'] is None or row['usage']['total'] == 0 or (not complete and not legacy):
                raise SystemExit('実測を停止しました。errとresults.jsonlを確認してください。')


if __name__ == '__main__':
    main()
