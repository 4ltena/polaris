#!/usr/bin/env python3
"""Recount saved conversation benchmarks without running either product.

Each report has its own campaign window. Evidence is copied under
EVIDENCE_ROOT/<stem>/metrics.jsonl or rollout-*.jsonl before entering the common
offline benchmark runner. Original reports and credentials are never changed.
"""
from __future__ import annotations

import argparse
from collections import Counter
from decimal import Decimal
import hashlib
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from benchmark_costs import RATES_SHA256, _load_rates, _quantize, price_usage

FIELDS = ('input', 'cache_read', 'cache_write', 'output', 'reasoning')
POLARIS_FIELDS = ('input_tokens', 'cache_read_tokens', 'cache_write_tokens',
                  'output_tokens', 'reasoning_tokens')
CODEX_FIELDS = ('input_tokens', 'cached_input_tokens', 'cache_write_input_tokens',
                'output_tokens', 'reasoning_output_tokens')


def count(value):
    return type(value) is int and value >= 0


def request(model, effort, usage, names, source):
    usage = usage if isinstance(usage, dict) else {}
    return {'model': model, 'effort': effort, 'source': source,
            'usage': {key: usage.get(name) if count(usage.get(name)) else None
                      for key, name in zip(FIELDS, names)}}


def polaris_requests(path, *, started=None, finished=None):
    """Keep prior attempts separately; do not erase their consumed usage."""
    rows, excluded, unscoped, errors = [], [], [], []
    if not path.is_file():
        return rows, excluded, unscoped, ['missing_metrics']
    for line_no, line in enumerate(path.read_text().splitlines(), 1):
        if not line.strip():
            continue
        source = f'{path.parent.name}/{path.name}:{line_no}'
        try:
            row = json.loads(line)
        except ValueError:
            errors.append(f'invalid_json:{source}')
            continue
        if not isinstance(row, dict):
            errors.append(f'invalid_record:{source}')
            continue
        target = rows
        if started is not None or finished is not None:
            timestamp = row.get('started_unix_ms')
            if not count(timestamp):
                errors.append(f'missing_timestamp:{source}')
                unscoped.append(request(row.get('model'), row.get('effort'), row.get('usage'),
                                        POLARIS_FIELDS, source))
                continue
            if ((started is not None and timestamp < started * 1000)
                    or (finished is not None and timestamp > finished * 1000)):
                target = excluded
        target.append(request(row.get('model'), row.get('effort'), row.get('usage'),
                              POLARIS_FIELDS, source))
    return rows, excluded, unscoped, errors


def codex_requests(paths):
    """Count token snapshots once, preserving per-request model attribution.

    An unchanged cumulative snapshot and last usage is a repeated notification,
    not a second request. Equal last usage with an advanced total still counts.
    Checkpoints are local to each rollout; parent and child are never merged.
    """
    rows, duplicates, errors = [], [], []
    for path in paths:
        if not path.is_file():
            errors.append(f'missing_rollout:{path.parent.name}/{path.name}')
            continue
        model = effort = previous = previous_total = None
        for line_no, line in enumerate(path.read_text().splitlines(), 1):
            if not line.strip():
                continue
            source = f'{path.parent.name}/{path.name}:{line_no}'
            try:
                event = json.loads(line)
            except ValueError:
                errors.append(f'invalid_json:{source}')
                continue
            if not isinstance(event, dict) or not isinstance(event.get('payload'), dict):
                errors.append(f'invalid_record:{source}')
                continue
            payload = event['payload']
            if event.get('type') == 'turn_context':
                model = payload.get('model')
                effort = payload.get('effort', payload.get('reasoning_effort'))
            if payload.get('type') != 'token_count':
                continue
            info = payload.get('info')
            # Quota-only notifications do not claim a new completion.
            if info is None:
                continue
            if not isinstance(info, dict) or not isinstance(info.get('last_token_usage'), dict):
                errors.append(f'missing_usage:{source}')
                continue
            total = info.get('total_token_usage')
            last = info['last_token_usage']
            checkpoint = (total, last)
            if isinstance(total, dict) and total and checkpoint == previous:
                duplicates.append(source)
                continue
            previous = checkpoint if isinstance(total, dict) and total else None
            if not isinstance(total, dict) or any(not count(total.get(k)) for k in CODEX_FIELDS[:4]):
                errors.append(f'cumulative_unavailable:{source}')
            else:
                before = previous_total or dict.fromkeys(CODEX_FIELDS[:4], 0)
                if any(not count(last.get(k)) or total[k] - before[k] != last[k]
                       for k in CODEX_FIELDS[:4]):
                    errors.append(f'cumulative_mismatch:{source}')
                previous_total = total
            rows.append(request(model, effort, info['last_token_usage'], CODEX_FIELDS, source))
    if not paths:
        errors.append('missing_rollouts')
    return rows, duplicates, errors


def known_price_parts(model, usage):
    """Price independently known input/output parts when a request is partial."""
    entry = _load_rates(model)
    rates = entry['api_per_million']
    context = entry.get('long_context')
    if context and not count(usage.get('input')):
        return Decimal(0)  # Even the output multiplier is unknown.
    long = context and usage['input'] > context['input_tokens_above']
    input_multiplier = Decimal(context['input_multiplier']) if long else Decimal(1)
    output_multiplier = Decimal(context['output_multiplier']) if long else Decimal(1)
    amount = Decimal(0)
    if (all(count(usage.get(k)) for k in FIELDS[:3])
            and usage['cache_read'] + usage['cache_write'] <= usage['input']):
        ordinary = usage['input'] - usage['cache_read'] - usage['cache_write']
        amount += (Decimal(rates['billed_input']) * ordinary
                   + Decimal(rates['cache_read']) * usage['cache_read']
                   + Decimal(rates.get('cache_write', '0')) * usage['cache_write']) * input_multiplier
    if count(usage.get('output')):
        amount += Decimal(rates['output']) * usage['output'] * output_multiplier
    return _quantize(amount / Decimal(1000000))


def summarize(rows, errors=()):
    """Known subtotals stay visible; incomplete totals never receive a price."""
    known = Counter({k: 0 for k in FIELDS})
    missing_fields, unpriced_models = Counter(), Counter()
    models = {}
    known_price = Decimal(0)
    issues = list(errors)
    priced_requests = 0
    for row in rows:
        usage = row['usage']
        for key in FIELDS:
            if count(usage.get(key)):
                known[key] += usage[key]
            else:
                missing_fields[key] += 1
        name = row.get('model')
        model = name if isinstance(name, str) and name else 'unknown'
        per = models.setdefault(model, {'requests': 0, 'efforts': [],
                                       'known_tokens': {k: 0 for k in FIELDS}})
        per['requests'] += 1
        if row.get('effort') not in per['efforts']:
            per['efforts'].append(row.get('effort'))
        for key in FIELDS:
            if count(usage.get(key)):
                per['known_tokens'][key] += usage[key]
        if any(not count(usage.get(k)) for k in FIELDS[:4]):
            issues.append(f'incomplete_usage:{row["source"]}')
            try:
                known_price += known_price_parts(model, usage)
            except KeyError:
                unpriced_models[model] += 1
            continue
        if count(usage.get('reasoning')) and usage['reasoning'] > usage['output']:
            issues.append(f'reasoning_exceeds_output:{row["source"]}')
            continue
        try:
            known_price += price_usage(model, *(usage[k] for k in FIELDS[:4]))
            priced_requests += 1
        except KeyError:
            unpriced_models[model] += 1
        except ValueError:
            issues.append(f'invalid_usage:{row["source"]}')
            known_price += known_price_parts(model, usage)
    complete_price = bool(rows) and not issues and not unpriced_models
    tokens = {k: known[k] if not missing_fields[k] and not errors else None for k in FIELDS}
    if not rows:
        tokens = {k: None for k in FIELDS}
    return {'requests': len(rows), 'tokens': tokens, 'known_tokens': dict(known),
            'missing_fields': dict(missing_fields), 'issues': issues, 'models': models,
            'usd': str(known_price) if complete_price else None,
            'known_usd': str(known_price), 'priced_requests': priced_requests,
            'unpriced_models': dict(unpriced_models), 'rates_sha256': RATES_SHA256}


def audit_report(report_path, evidence_root):
    data = report_path.read_bytes()
    report = json.loads(data)
    arm = report['arm']
    if arm not in ('polaris', 'codex'):
        raise ValueError('unknown benchmark arm')
    started, finished = report['started_unix'], report['finished_unix']
    if not (isinstance(started, (int, float)) and isinstance(finished, (int, float))
            and 0 < started <= finished):
        raise ValueError('invalid campaign window')
    all_rows, all_excluded, all_unscoped, all_errors, runs = [], [], [], [], []
    fingerprints = {}
    for record in report['rows']:
        stem = record['stem']
        if not isinstance(stem, str) or Path(stem).name != stem or stem in ('.', '..'):
            raise ValueError('invalid evidence stem')
        root = evidence_root / stem
        if arm == 'polaris':
            paths = [root/'metrics.jsonl']
            rows, excluded, unscoped, errors = polaris_requests(paths[0], started=started, finished=finished)
            duplicates = []
        else:
            # Only the explicitly referenced rollout names belong to this run.
            paths = [root/Path(p).name for p in record['rollouts']]
            if len(paths) != len(set(paths)):
                raise ValueError('duplicate rollout path')
            rows, duplicates, errors = codex_requests(paths)
            excluded, unscoped = [], []
        for path in paths:
            if path.is_file():
                fingerprints[str(path.relative_to(evidence_root))] = hashlib.sha256(path.read_bytes()).hexdigest()
        runs.append({'stem': stem, 'pass': record['pass'], 'original_usd': record.get('usd'),
                     'summary': summarize(rows, errors), 'excluded_prior_attempts': summarize(excluded),
                     'unscoped_consumption': summarize(unscoped),
                     'duplicate_notifications': duplicates})
        all_rows += rows
        all_excluded += excluded
        all_unscoped += unscoped
        all_errors += errors
    return {'schema_version': 1, 'arm': arm, 'pattern': report['pattern'],
            'report_sha256': hashlib.sha256(data).hexdigest(), 'evidence_sha256': fingerprints,
            'window': {'started': started, 'finished': finished},
            'summary': summarize(all_rows, all_errors),
            'unscoped_consumption': summarize(all_unscoped),
            'excluded_prior_attempts': summarize(all_excluded), 'runs': runs}


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--report-dir', type=Path, required=True)
    ap.add_argument('--evidence-root', type=Path, required=True)
    ap.add_argument('--output', type=Path, required=True)
    args = ap.parse_args()
    result = [audit_report(args.report_dir/f'{pattern}-{arm}.json', args.evidence_root)
              for pattern in ('analyze', 'small', 'todo') for arm in ('polaris', 'codex')]
    with args.output.open('x') as output:
        json.dump({'api_requests': 0, 'reports': result}, output, indent=2)
        output.write('\n')
    for item in result:
        print(json.dumps({k: item[k] for k in ('arm', 'pattern', 'summary')}))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
