#!/usr/bin/env python3
"""Algedi の実作業課題を GPT-6 medium 固定で測定する。

各実行は新規 fixture で一発のみ実行する。対話再開は測定しない。モデルの
生成物は Python として実行せず、静的な AST 検査または JSON 応答だけで採点する。
"""
import argparse
import ast
from decimal import Decimal, DecimalException, ROUND_FLOOR
import hashlib
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import uuid

import benchmark_efficiency as efficiency

TASKS = ('singlefile_repair', 'multifile_repair', 'long_log_rootcause',
         'superseded_decision', 'changed_source_evidence')


def task_files(task):
    if task == 'singlefile_repair':
        return {
            'README.md': '料金は1000g超過ごとに200円、端数は切り上げ。負数はValueError。\n',
            'rates.py': "def shipping_fee(weight_g):\n    if weight_g < 0:\n        raise ValueError('negative')\n    return 500 + max(0, weight_g - 1000) // 1000 * 200\n",
        }
    if task == 'multifile_repair':
        return {
            'README.md': '税込価格は小計に10%を加算し、円未満を切り捨てる。変更は pricing.py と receipt.py のみ。\n',
            'pricing.py': 'def total(subtotal):\n    return subtotal\n',
            'receipt.py': 'def render(subtotal):\n    return f"total={subtotal}"\n',
            'unrelated.py': 'KEEP = True\n',
        }
    if task == 'long_log_rootcause':
        return {
            'README.md': 'ログから根本原因を特定する。変更禁止。JSONだけで回答する。\n',
            'service.log': ('\n'.join(f'2026-09-06T00:{i % 60:02}:00Z INFO worker healthy request={i}' for i in range(700))
                            + '\n2026-09-06T01:02:03Z ERROR code=TLS_CERT_EXPIRED db connect failed serial=AL-42\n'
                            + '\n'.join(f'2026-09-06T02:{i % 60:02}:00Z WARN retry skipped request={i}' for i in range(700))),
        }
    if task == 'superseded_decision':
        return {
            'README.md': '承認済みの最終決定だけを回答する。変更禁止。\n',
            'decisions.md': ('決定 D010: 承認済み。retention_days=14。\n'
                             + '\n'.join(f'監査 {i}: 変更なし。' for i in range(450))
                             + '\n決定 D011: 承認済み。retention_days=30。D010を置換する。\n'
                             + '\n'.join(f'観測 {i}: 通常稼働。' for i in range(450))
                             + '\n提案 P012: 未承認。retention_days=90。適用禁止。\n'),
        }
    return {
        'README.md': '保存済み根拠と現行資料の差を確認する。変更禁止。JSONだけで回答する。\n',
        'saved_evidence.md': '保存時点: policy_version=3; retention_days=30; evidence_id=EV-7\n',
        'current_source.md': '現行時点: policy_version=3; retention_days=30\n',
    }


PROMPTS = {
    'singlefile_repair': 'README.md と rates.py を読み、送料計算だけを rates.py で修正してください。変更は rates.py だけです。',
    'multifile_repair': 'README.md、pricing.py、receipt.py を読み、税込価格を正しく表示するよう修正してください。変更は pricing.py と receipt.py だけです。',
    'long_log_rootcause': 'README.md と service.log を読み、原因となった ERROR の code をそのまま root_cause に、serial を JSON だけで {"root_cause":文字列,"serial":文字列} と回答してください。',
    'superseded_decision': 'README.md と decisions.md を読み、最終承認済み決定を JSON だけで {"decision_id":文字列,"retention_days":整数} と回答してください。',
    'changed_source_evidence': 'README.md、saved_evidence.md、current_source.md を読み、保存時点と現行時点の相違を JSON だけで {"evidence_id":文字列,"saved_retention_days":整数,"current_retention_days":整数,"changed":真偽値} と回答してください。',
}


class SafeEvaluationError(Exception):
    pass


class SafeProgram:
    """Bounded evaluator for fixture functions, numeric operations and allowlisted imports."""
    MAX_SOURCE = 16 * 1024
    MAX_NODES = 400
    MAX_DEPTH = 48
    MAX_STEPS = 2_000
    MAX_BITS = 128
    MAX_STRING = 4_096

    def __init__(self, modules):
        self.functions, self.imports, self.steps = {}, {}, 0
        allowed = (ast.Module, ast.FunctionDef, ast.arguments, ast.arg, ast.Assign, ast.If,
                   ast.Raise, ast.Return, ast.Name, ast.Load, ast.Store, ast.Tuple, ast.Constant, ast.BinOp,
                   ast.UnaryOp, ast.Add, ast.Sub, ast.Mult, ast.Div, ast.FloorDiv, ast.Mod, ast.USub, ast.Compare,
                   ast.Lt, ast.LtE, ast.Gt, ast.GtE, ast.Eq, ast.NotEq, ast.Call, ast.JoinedStr,
                   ast.FormattedValue, ast.ImportFrom, ast.Import, ast.alias, ast.Attribute, ast.keyword)
        for module, source in modules.items():
            if len(source.encode()) > self.MAX_SOURCE:
                raise SafeEvaluationError('source_size_limit')
            tree = ast.parse(source)
            if sum(1 for _ in ast.walk(tree)) > self.MAX_NODES or any(not isinstance(node, allowed) for node in ast.walk(tree)):
                raise SafeEvaluationError('unsupported_syntax')
            for node in tree.body:
                if isinstance(node, ast.FunctionDef):
                    if (node.decorator_list or node.args.defaults or node.args.kw_defaults
                            or node.args.posonlyargs or node.args.kwonlyargs or node.args.vararg or node.args.kwarg or node.returns
                            or any(arg.annotation for arg in node.args.args)):
                        raise SafeEvaluationError('unsupported_function_signature')
                    if node.name in {'max', 'int', 'str', 'divmod'} or any(arg.arg in {'max', 'int', 'str', 'divmod'} for arg in node.args.args):
                        raise SafeEvaluationError('reserved_name_binding')
                    self.functions[(module, node.name)] = node
                elif (isinstance(node, ast.ImportFrom) and node.level == 0 and node.module == 'math'
                      and all(alias.name in {'ceil', 'floor'} and alias.asname is None for alias in node.names)):
                    for alias in node.names:
                        self.imports[(module, alias.name)] = ('math', alias.name)
                elif isinstance(node, ast.Import) and all(alias.name == 'math' and alias.asname is None for alias in node.names):
                    self.imports[(module, 'math')] = ('math', None)
                elif (isinstance(node, ast.ImportFrom) and node.level == 0 and node.module == 'decimal'
                      and {alias.name for alias in node.names} == {'Decimal', 'ROUND_FLOOR'}
                      and all(alias.asname is None for alias in node.names)):
                    self.imports[(module, 'Decimal')] = ('decimal', 'Decimal')
                    self.imports[(module, 'ROUND_FLOOR')] = ('decimal', 'ROUND_FLOOR')
                elif (module == 'receipt' and isinstance(node, ast.ImportFrom) and node.level == 0 and node.module == 'pricing'
                      and all(alias.name == 'total' and alias.asname is None for alias in node.names)):
                    self.imports[(module, 'total')] = ('pricing', 'total')
                elif (module == 'receipt' and isinstance(node, ast.Import)
                      and all(alias.name == 'pricing' and alias.asname is None for alias in node.names)):
                    self.imports[(module, 'pricing')] = ('pricing', None)
                else:
                    raise SafeEvaluationError('unsupported_module_statement')
            reserved = {'max', 'int', 'str', 'divmod'} | {name for current, name in self.functions if current == module} | {name for current, name in self.imports if current == module}
            if any(isinstance(node, ast.Assign) and any(name in reserved for target in node.targets for name in self.target_names(target))
                   for node in ast.walk(tree)):
                raise SafeEvaluationError('reserved_name_binding')

    @staticmethod
    def target_names(target):
        if isinstance(target, ast.Name):
            return (target.id,)
        if isinstance(target, ast.Tuple):
            return tuple(name for element in target.elts for name in SafeProgram.target_names(element))
        return ()

    def bounded(self, value):
        if type(value) is int and value.bit_length() <= self.MAX_BITS:
            return value
        if type(value) is float and math.isfinite(value) and abs(value) <= 2 ** self.MAX_BITS:
            return value
        if type(value) is Decimal:
            try:
                if value.is_finite() and abs(value) <= Decimal(2) ** self.MAX_BITS:
                    return value
            except DecimalException as error:
                raise SafeEvaluationError('invalid_decimal') from error
        if type(value) is str and len(value) <= self.MAX_STRING:
            return value
        raise SafeEvaluationError('value_limit')

    def call(self, module, name, values, depth=0):
        self.tick(depth)
        function = self.functions.get((module, name))
        if function is None or len(function.args.args) != len(values):
            raise SafeEvaluationError('undefined_function')
        env = dict(zip((arg.arg for arg in function.args.args), map(self.bounded, values)))
        for statement in function.body:
            result = self.statement(module, statement, env, depth)
            if result is not None:
                return result
        raise SafeEvaluationError('missing_return')

    def tick(self, depth):
        if depth >= self.MAX_DEPTH or self.steps >= self.MAX_STEPS:
            raise SafeEvaluationError('evaluation_limit')
        self.steps += 1

    def statement(self, module, node, env, depth):
        self.tick(depth)
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            env[node.targets[0].id] = self.expression(module, node.value, env, depth)
        elif (isinstance(node, ast.Assign) and len(node.targets) == 1
              and isinstance(node.targets[0], ast.Tuple)):
            target = node.targets[0]
            if (len(target.elts) != 2 or not all(isinstance(element, ast.Name) for element in target.elts)
                    or target.elts[0].id == target.elts[1].id):
                raise SafeEvaluationError('unsupported_tuple_target')
            values = self.divmod_values(module, node.value, env, depth)
            env[target.elts[0].id], env[target.elts[1].id] = values
        elif isinstance(node, ast.If):
            for child in node.body if self.expression(module, node.test, env, depth) else node.orelse:
                result = self.statement(module, child, env, depth)
                if result is not None:
                    return result
        elif isinstance(node, ast.Raise) and isinstance(node.exc, ast.Call) and isinstance(node.exc.func, ast.Name) and node.exc.func.id == 'ValueError':
            if any(not isinstance(argument, ast.Constant) or type(argument.value) not in (int, str) for argument in node.exc.args):
                raise SafeEvaluationError('unsupported_raise_argument')
            raise ValueError
        elif isinstance(node, ast.Return):
            return self.expression(module, node.value, env, depth)
        else:
            raise SafeEvaluationError('unsupported_statement')
        return None

    def expression(self, module, node, env, depth):
        self.tick(depth)
        if isinstance(node, ast.Constant) and type(node.value) in (int, float, str):
            return self.bounded(node.value)
        if isinstance(node, ast.Name) and node.id in env:
            return env[node.id]
        if isinstance(node, ast.Name) and self.imports.get((module, node.id)) == ('decimal', 'ROUND_FLOOR'):
            return ROUND_FLOOR
        if isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.USub):
            return self.bounded(-self.expression(module, node.operand, env, depth + 1))
        if isinstance(node, ast.BinOp):
            left, right = self.expression(module, node.left, env, depth + 1), self.expression(module, node.right, env, depth + 1)
            if type(left) is str or type(right) is str:
                if isinstance(node.op, ast.Add) and type(left) is type(right) is str:
                    return self.bounded(left + right)
                raise SafeEvaluationError('unsupported_string_operation')
            if type(left) not in (int, float, Decimal, bool) or type(right) not in (int, float, Decimal, bool):
                raise SafeEvaluationError('unsupported_operand')
            operators = {ast.Add: lambda: left + right, ast.Sub: lambda: left - right,
                         ast.Mult: lambda: left * right, ast.Div: lambda: left / right,
                         ast.FloorDiv: lambda: left // right, ast.Mod: lambda: left % right}
            for kind, operation in operators.items():
                if isinstance(node.op, kind):
                    try:
                        return self.bounded(operation())
                    except (ArithmeticError, DecimalException, TypeError) as error:
                        raise SafeEvaluationError('invalid_decimal_operation') from error
        if isinstance(node, ast.Compare) and len(node.ops) == len(node.comparators) == 1:
            left, right = self.expression(module, node.left, env, depth + 1), self.expression(module, node.comparators[0], env, depth + 1)
            operators = {ast.Lt: lambda: left < right, ast.LtE: lambda: left <= right,
                         ast.Gt: lambda: left > right, ast.GtE: lambda: left >= right,
                         ast.Eq: lambda: left == right, ast.NotEq: lambda: left != right}
            for kind, operation in operators.items():
                if isinstance(node.ops[0], kind):
                    return operation()
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
            if node.keywords:
                raise SafeEvaluationError('unsupported_keyword_call')
            if node.func.id in env:
                raise SafeEvaluationError('shadowed_callable')
            values = [self.expression(module, arg, env, depth + 1) for arg in node.args]
            if node.func.id == 'max' and len(values) == 2 and all(type(value) in (int, float, Decimal, bool) for value in values):
                return max(values)
            if node.func.id == 'int' and len(values) == 1 and type(values[0]) in (int, float, Decimal):
                return self.bounded(int(self.bounded(values[0])))
            if node.func.id == 'str' and len(values) == 1 and type(values[0]) in (int, float):
                return self.bounded(str(values[0]))
            target = self.imports.get((module, node.func.id), (module, node.func.id))
            if target == ('decimal', 'Decimal') and len(values) == 1 and type(values[0]) in (int, float, str):
                try:
                    return self.bounded(Decimal(values[0]))
                except (DecimalException, ValueError) as error:
                    raise SafeEvaluationError('invalid_decimal_literal') from error
            if target[0] == 'math' and target[1] in {'ceil', 'floor'}:
                return self.math_function(target[1], values)
            return self.call(target[0], target[1], values, depth + 1)
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
            if (node.func.attr == 'to_integral_value' and not node.args and len(node.keywords) == 1
                    and node.keywords[0].arg == 'rounding'
                    and isinstance(node.keywords[0].value, ast.Name)
                    and self.imports.get((module, node.keywords[0].value.id)) == ('decimal', 'ROUND_FLOOR')):
                value = self.expression(module, node.func.value, env, depth + 1)
                if type(value) is Decimal:
                    try:
                        return self.bounded(value.to_integral_value(rounding=ROUND_FLOOR))
                    except DecimalException as error:
                        raise SafeEvaluationError('invalid_decimal_operation') from error
            if not isinstance(node.func.value, ast.Name) or node.keywords:
                raise SafeEvaluationError('unsupported_attribute')
            imported = self.imports.get((module, node.func.value.id))
            if node.func.value.id in env:
                raise SafeEvaluationError('shadowed_module')
            if imported == ('math', None) and node.func.attr in {'ceil', 'floor'}:
                return self.math_function(node.func.attr, [self.expression(module, arg, env, depth + 1) for arg in node.args])
            if imported != ('pricing', None):
                raise SafeEvaluationError('unsupported_attribute')
            return self.call('pricing', node.func.attr, [self.expression(module, arg, env, depth + 1) for arg in node.args], depth + 1)
        if isinstance(node, ast.JoinedStr):
            if any(isinstance(value, ast.FormattedValue) and (value.conversion != -1 or value.format_spec is not None) for value in node.values):
                raise SafeEvaluationError('unsupported_fstring_format')
            return self.bounded(''.join(value.value if isinstance(value, ast.Constant) else str(self.expression(module, value.value, env, depth + 1))
                                       for value in node.values))
        raise SafeEvaluationError('許可外の式')

    def divmod_values(self, module, node, env, depth):
        if (not isinstance(node, ast.Call) or not isinstance(node.func, ast.Name)
                or node.func.id != 'divmod' or node.keywords or len(node.args) != 2):
            raise SafeEvaluationError('unsupported_tuple_value')
        values = [self.expression(module, argument, env, depth + 1) for argument in node.args]
        if any(type(value) not in (int, float, Decimal, bool) for value in values):
            raise SafeEvaluationError('unsupported_divmod_argument')
        try:
            quotient, remainder = divmod(*values)
        except (ArithmeticError, DecimalException, TypeError) as error:
            raise SafeEvaluationError('invalid_divmod') from error
        return self.bounded(quotient), self.bounded(remainder)

    def math_function(self, name, values):
        if len(values) != 1 or type(values[0]) not in (int, float):
            raise SafeEvaluationError(f'unsupported_math_{name}_argument')
        return self.bounded(getattr(math, name)(self.bounded(values[0])))


def static_singlefile(root):
    try:
        function = SafeProgram({'rates': (root / 'rates.py').read_text()})
        if set(function.functions) != {('rates', 'shipping_fee')}:
            return False
        for value, expected in ((0, 500), (1000, 500), (1001, 700), (2000, 700), (2001, 900), (1000001, 200500)):
            if function.call('rates', 'shipping_fee', [value]) != expected:
                return False
        try:
            function.call('rates', 'shipping_fee', [-1])
        except ValueError:
            return True
        return False
    except (OSError, SyntaxError, ArithmeticError, TypeError, ValueError):
        return False


def quality_raw(task, stdout, root):
    if task == 'singlefile_repair':
        return static_singlefile(root)
    if task == 'multifile_repair':
        try:
            functions = SafeProgram({'pricing': (root / 'pricing.py').read_text(), 'receipt': (root / 'receipt.py').read_text()})
            if set(functions.functions) != {('pricing', 'total'), ('receipt', 'render')}:
                return False
            for value in (0, 1, 9, 10, 99, 100, 999, 10 ** 12):
                expected = value * 110 // 100
                if functions.call('pricing', 'total', [value]) != expected or functions.call('receipt', 'render', [value]) != f'total={expected}':
                    return False
            return True
        except (OSError, SyntaxError, ArithmeticError, TypeError, ValueError):
            return False
    result = efficiency.final_json(stdout)
    expected = {
        'long_log_rootcause': {'root_cause': 'TLS_CERT_EXPIRED', 'serial': 'AL-42'},
        'superseded_decision': {'decision_id': 'D011', 'retention_days': 30},
        'changed_source_evidence': {'evidence_id': 'EV-7', 'saved_retention_days': 30,
                                    'current_retention_days': 45, 'changed': True},
    }
    return result == expected[task]


def quality(task, stdout, root):
    try:
        return quality_raw(task, stdout, root)
    except SafeEvaluationError:
        return False


def quality_result(task, stdout, root):
    """Keep unsupported bounded-grammar output distinct from a checked wrong answer."""
    try:
        if task == 'singlefile_repair':
            SafeProgram({'rates': (root / 'rates.py').read_text()})
        elif task == 'multifile_repair':
            SafeProgram({'pricing': (root / 'pricing.py').read_text(), 'receipt': (root / 'receipt.py').read_text()})
    except SafeEvaluationError as error:
        return 'unassessed', str(error)
    except (OSError, SyntaxError) as error:
        return 'incorrect', type(error).__name__
    try:
        checked = quality_raw(task, stdout, root)
    except SafeEvaluationError as error:
        return 'unassessed', str(error)
    return ('pass', None) if checked else ('incorrect', None)


def snapshot(root):
    return efficiency.snapshot(root)


def parse_metrics(path, stderr):
    try:
        traces = [json.loads(line) for line in path.read_text().splitlines()]
        if not all(isinstance(row, dict) for row in traces):
            traces = []
    except (OSError, ValueError):
        traces = []
    usage = re.search(r'tokens: in (\d+) / out (\d+) / cache (\d+) / total (\d+)', stderr)
    totals = dict(zip(('input', 'output', 'cache', 'total'), map(int, usage.groups()))) if usage else None
    fields = {'input': 'input_tokens', 'output': 'output_tokens', 'cache': 'cache_read_tokens'}
    valid_usage = all(isinstance(row.get('usage'), dict)
                      and all(type(row['usage'].get(field)) is int and row['usage'][field] >= 0
                              for field in fields.values())
                      and row['usage']['cache_read_tokens'] <= row['usage']['input_tokens']
                      for row in traces)
    trace_totals = {key: sum(row['usage'][value] for row in traces) for key, value in fields.items()} if valid_usage else {}
    coverage_match = re.search(r'使用量の計測: 応答あり (\d+) / 欠測 (\d+) / 失敗 (\d+)', stderr)
    coverage = (dict(zip(('reported', 'missing', 'failed'), map(int, coverage_match.groups())))
                if coverage_match else None)
    model_match = (all(row.get('model') == 'gpt-6-astra' and row.get('effort') == 'medium'
                       for row in traces) if traces else None)
    totals_valid = (totals is not None and totals['total'] == totals['input'] + totals['output']
                    and 0 <= totals['cache'] <= totals['input'])
    embedding = efficiency.embedding_usage(stderr)
    complete = (bool(traces) and valid_usage and model_match is True
                and all(row.get('usage_missing') is False and row.get('outcome') == 'completed' for row in traces)
                and totals_valid and all(totals[key] == trace_totals[key] for key in fields)
                and coverage == {'reported': len(traces), 'missing': 0, 'failed': 0}
                and embedding['missing'] == 0 and embedding['failed'] == 0
                and '診断ログの保存に失敗' not in stderr)
    return totals, traces, model_match, coverage, embedding, complete


def exercise(audit):
    try:
        entries = [json.loads(line) for line in audit.read_text().splitlines()]
    except (OSError, ValueError):
        entries = []
    stored = sum(int(match.group(1)) for entry in entries if entry.get('tool') == 'tool-memory'
                 for match in [re.search(r'stored=(\d+)', entry.get('result', ''))] if match)
    reads = sum(entry.get('tool') == 'read' and 'memory://' in entry.get('detail', '') for entry in entries)
    return {'stored_results': stored, 'memory_reads': reads}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True, help='測定する Polaris バイナリ')
    parser.add_argument('--baseline', type=Path, help='比較元バイナリ。指定時のみハッシュを記録する')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--label', required=True)
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--tasks', nargs='+', choices=TASKS, default=list(TASKS))
    parser.add_argument('--timeout', type=int, default=600)
    parser.add_argument('--tool-memory', choices=('off', 'history', 'retrieval'), default='retrieval')
    parser.add_argument('--cache-prefix', choices=('compact', 'stable'), default='compact',
                        help='固定prefixの比較条件。親環境のprofileは引き継がない')
    parser.add_argument('--cache-pacing', choices=('off', 'on'), default='off',
                        help='モデル要求を最低5秒間隔にする実験設定')
    parser.add_argument('--turn-affinity', choices=('off', 'on'), default='off',
                        help='同一ターンの通信継続条件。親環境の設定は引き継がない')
    args = parser.parse_args()
    if not 1 <= args.repeats <= 5 or not 1 <= args.timeout <= 600:
        parser.error('repeatsは1〜5、timeoutは1〜600秒')
    if 'POLARIS_CACHE_MODE' in os.environ:
        parser.error('この比較ではPOLARIS_CACHE_MODEを未指定にしてください')
    binary = args.binary.resolve(strict=True)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    metadata = {'model': 'gpt-6-astra', 'effort': 'medium', 'label': args.label,
                'tool_memory': args.tool_memory, 'repeats': args.repeats,
                'cache_prefix': args.cache_prefix,
                'turn_affinity': args.turn_affinity,
                'cache_pacing': args.cache_pacing,
                'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                'baseline_binary': str(args.baseline.resolve(strict=True)) if args.baseline else None,
                'baseline_sha256': hashlib.sha256(args.baseline.resolve(strict=True).read_bytes()).hexdigest() if args.baseline else None,
                'resume_measurement': 'not_measured: each row is a new one-shot process',
                'source_change_measurement': 'fixture_only_before_model: current_source.md changes during fixture setup; saved_evidence.md is fixture evidence, not a memory snapshot'}
    (output / 'metadata.json').write_text(json.dumps(metadata, ensure_ascii=False, indent=2))
    for task in args.tasks:
        namespace = uuid.uuid4().hex
        for repeat in range(1, args.repeats + 1):
            fixture = output / f'{task}-{repeat}.fixture'
            fixture.mkdir(mode=0o700)
            (fixture / '.polaris').mkdir(mode=0o700)
            for name, body in task_files(task).items():
                (fixture / name).write_text(body)
            audit = output / f'{task}-{repeat}.audit.jsonl'
            timeline = [{'sequence': 1, 'event': 'fixture_ready'}]
            if task == 'changed_source_evidence':
                (fixture / 'current_source.md').write_text('現行時点: policy_version=4; retention_days=45\n')
                timeline.append({'sequence': 2, 'event': 'source_changed', 'path': 'current_source.md'})
            timeline.append({'sequence': 3, 'event': 'process_started'})
            with audit.open('a') as handle:
                for event in timeline:
                    handle.write(json.dumps({'benchmark_timeline': event}, ensure_ascii=False) + '\n')
            before = snapshot(fixture)
            stem = f'{task}-{repeat}'
            metrics = output / f'{stem}.metrics.jsonl'
            env = os.environ.copy()
            env.update(POLARIS_PROVIDER='codex', POLARIS_METRICS_PATH=str(metrics),
                       POLARIS_CACHE_NAMESPACE=namespace, POLARIS_CACHE_PREFIX=args.cache_prefix,
                       POLARIS_TURN_AFFINITY=args.turn_affinity, POLARIS_CACHE_PACING=args.cache_pacing,
                       POLARIS_AUTH_READ_ONLY='1')
            command = [str(binary), '--model', 'gpt-6-astra', '--effort', 'medium',
                       '--sandbox', 'workspace-write' if task.endswith('repair') else 'read-only',
                       '--approval', 'never', '--max-turns', '12', '--audit', str(audit),
                       '--prompt', PROMPTS[task]]
            command.extend(('--tool-memory', args.tool_memory))
            start = time.monotonic()
            try:
                run = subprocess.run(command, cwd=fixture, env=env, capture_output=True, text=True,
                                     timeout=args.timeout, stdin=subprocess.DEVNULL)
                stdout, stderr, code = run.stdout, run.stderr, run.returncode
            except subprocess.TimeoutExpired as error:
                stdout = (error.stdout or b'').decode(errors='replace') if isinstance(error.stdout, bytes) else (error.stdout or '')
                stderr = (error.stderr or b'').decode(errors='replace') if isinstance(error.stderr, bytes) else (error.stderr or '')
                code = None
            (output / f'{stem}.out').write_text(stdout)
            (output / f'{stem}.err').write_text(stderr)
            after = snapshot(fixture)
            changed = sorted(key for key in before.keys() | after.keys() if before.get(key) != after.get(key))
            permitted = {'singlefile_repair': {'rates.py'}, 'multifile_repair': {'pricing.py', 'receipt.py'}}
            scope_ok = (set(changed).issubset(permitted.get(task, set()))
                        and all(after.get(path, ('missing', ''))[0] == 'file' for path in changed)
                        and not any(value[0] == 'link' for value in after.values()))
            totals, traces, model_match, coverage, embedding, complete = parse_metrics(metrics, stderr)
            quality_state, quality_reason = quality_result(task, stdout, fixture) if scope_ok else ('incorrect', 'scope_violation')
            row = {'task': task, 'repeat': repeat, 'exit_code': code,
                   'elapsed_seconds': round(time.monotonic() - start, 3),
                   'quality_pass': code == 0 and quality_state == 'pass',
                   'quality_assessment': quality_state, 'quality_unassessed_reason': quality_reason if quality_state == 'unassessed' else None,
                   'scope_pass': scope_ok, 'changed_paths': changed, 'usage': totals,
                   'embedding_usage': embedding,
                   'combined_known_tokens': (totals['total'] + embedding['known_input_tokens']) if totals else None,
                   'embedding_measurement_complete': embedding['missing'] == 0 and embedding['failed'] == 0,
                   'recorded_model_match': model_match, 'cli_coverage': coverage,
                   'metrics_recorded': bool(traces), 'measurement_complete': complete,
                   'comparison_status': 'complete' if complete and code == 0 and scope_ok and quality_state == 'pass' else 'incomplete',
                   'exercise': exercise(audit), 'timeline': timeline, 'resume_measurement': 'not_measured'}
            with (output / 'results.jsonl').open('a') as handle:
                handle.write(json.dumps(row, ensure_ascii=False) + '\n')
            print(json.dumps(row, ensure_ascii=False), flush=True)
            if code != 0 or not row['quality_pass'] or not complete or not totals or totals['total'] == 0:
                raise SystemExit('実測を停止しました。errとresults.jsonlを確認してください。')


if __name__ == '__main__':
    main()
