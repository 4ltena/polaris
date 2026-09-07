#!/usr/bin/env python3
"""Aggregate private-safe Codex metric rows into hypothetical cost summaries.

The calculation is deliberately limited to the approved GPT-6 Astra/medium
comparison.  It consumes only the small usage records emitted by
``codex_metrics`` and never emits request bodies, responses, or identifiers.
"""
import argparse
from decimal import Decimal
import json
from pathlib import Path
import sys


MODEL = 'gpt-6-astra'
EFFORT = 'medium'
LONG_CONTEXT_LIMIT = 272_000
MAX_METRIC_PATHS = 128

MILLION = Decimal('1000000')
API_RATES = {
    'ordinary_input': Decimal('10'),
    'cache_read': Decimal('1'),
    'cache_write': Decimal('12.5'),
    'output': Decimal('50'),
}
CREDIT_RATES = {
    'ordinary_input': Decimal('250'),
    'cache_read': Decimal('25'),
    'output': Decimal('1250'),
}
USAGE_FIELDS = ('input_tokens', 'cache_read_tokens', 'cache_write_tokens',
                'output_tokens', 'total_tokens')


def _integer(value):
    return type(value) is int and value >= 0


def _decimal_text(value):
    """Produce a stable JSON-safe Decimal representation."""
    return format(value, 'f')


def _usage_summary(usage):
    """Return safe known fields, validation errors, and whether usage is complete."""
    known = {field: None for field in USAGE_FIELDS}
    errors = []
    if not isinstance(usage, dict):
        return known, ['usage_missing'], False

    for field in USAGE_FIELDS:
        value = usage.get(field)
        if value is None:
            continue
        if not _integer(value):
            errors.append('invalid_' + field)
            continue
        known[field] = value

    required = USAGE_FIELDS[:4]
    for field in required:
        if known[field] is None:
            errors.append('missing_' + field)

    input_tokens = known['input_tokens']
    cache_read = known['cache_read_tokens']
    cache_write = known['cache_write_tokens']
    output = known['output_tokens']
    total = known['total_tokens']
    if input_tokens is not None and cache_read is not None and cache_write is not None:
        if cache_read + cache_write > input_tokens:
            errors.append('cache_exceeds_input')
    if total is not None and input_tokens is not None and output is not None:
        if total != input_tokens + output:
            errors.append('total_mismatch')
    return known, errors, not errors


def _price(known, rates, long_context_multipliers):
    multiplier = (Decimal('2'), Decimal('1.5')) if (
        long_context_multipliers and known['input_tokens'] > LONG_CONTEXT_LIMIT
    ) else (Decimal('1'), Decimal('1'))
    input_multiplier, output_multiplier = multiplier
    ordinary = known['input_tokens'] - known['cache_read_tokens'] - known['cache_write_tokens']
    return ((rates['ordinary_input'] * ordinary + rates['cache_read'] * known['cache_read_tokens']
             + rates.get('cache_write', Decimal('0')) * known['cache_write_tokens']) * input_multiplier
            + rates['output'] * known['output_tokens'] * output_multiplier) / MILLION


def classify_row(row):
    """Classify one decoded metric record without retaining private record data."""
    if not isinstance(row, dict):
        return {'eligible': False, 'complete': False, 'outcome': None,
                'known_usage': {field: None for field in USAGE_FIELDS},
                'errors': ['invalid_row']}

    known, usage_errors, usage_complete = _usage_summary(row.get('usage'))
    errors = list(usage_errors)
    if row.get('model') != MODEL:
        errors.append('wrong_model')
    if row.get('effort') != EFFORT:
        errors.append('wrong_effort')
    outcome = row.get('outcome')
    if outcome != 'completed':
        errors.append('outcome_not_completed')
    complete = not errors
    result = {'eligible': row.get('model') == MODEL and row.get('effort') == EFFORT,
              'complete': complete, 'outcome': outcome, 'known_usage': known,
              'errors': errors}
    if complete:
        result['standard_api_hypothetical_usd'] = _decimal_text(_price(known, API_RATES, True))
        if known['cache_write_tokens'] == 0:
            result['codex_credit_scenario'] = _decimal_text(_price(known, CREDIT_RATES, False))
            result['codex_credit_scenario_complete'] = True
        else:
            result['codex_credit_scenario'] = None
            result['codex_credit_scenario_complete'] = False
    return result


def summarize_rows(rows, label):
    """Summarize decoded per-request metrics for a caller or the JSON CLI."""
    if not isinstance(label, str) or not label:
        raise ValueError('label must be a non-empty string')
    summary = {
        'label': label,
        'contract': {
            'model': MODEL,
            'effort': EFFORT,
            'rates_as_of': '2026-09-07',
            'pricing_sources': [
                'https://developers.openai.com/api/docs/models/gpt-6-astra',
                'https://developers.openai.com/api/docs/guides/prompt-caching',
                'https://learn.chatgpt.com/docs/pricing',
            ],
            'standard_api_rates_per_million_tokens': {
                'ordinary_input_usd': '10', 'cache_read_usd': '1',
                'cache_write_usd': '12.5', 'output_usd': '50',
            },
            'standard_api_hypothetical': True,
            'codex_credit_scenario_is_not_invoice_or_rate_limit': True,
            'codex_credit_scenario_requires_zero_cache_write_tokens': True,
        },
        'requests': {'seen': 0, 'complete': 0, 'incomplete': 0, 'failed': 0},
        'measurement_complete': False,
        'standard_api_hypothetical_usd': None,
        'known_complete_standard_api_hypothetical_usd': '0',
        'codex_credit_scenario': None,
        'known_complete_codex_credit_scenario': '0',
        'codex_credit_scenario_complete': False,
        'credit_supported_complete_requests': 0,
        'failed_partial_usage': {'requests': 0, **{field: 0 for field in USAGE_FIELDS}},
        'incomplete_reasons': {},
    }
    api_total = Decimal('0')
    credit_total = Decimal('0')
    for row in rows:
        classified = classify_row(row)
        summary['requests']['seen'] += 1
        if classified['outcome'] != 'completed':
            summary['requests']['failed'] += 1
            summary['failed_partial_usage']['requests'] += 1
            for field, value in classified['known_usage'].items():
                if value is not None:
                    summary['failed_partial_usage'][field] += value
        if classified['complete']:
            summary['requests']['complete'] += 1
            api_total += Decimal(classified['standard_api_hypothetical_usd'])
            if classified['codex_credit_scenario_complete']:
                summary['credit_supported_complete_requests'] += 1
                credit_total += Decimal(classified['codex_credit_scenario'])
        else:
            summary['requests']['incomplete'] += 1
            for error in classified['errors']:
                summary['incomplete_reasons'][error] = summary['incomplete_reasons'].get(error, 0) + 1
    summary['known_complete_standard_api_hypothetical_usd'] = _decimal_text(api_total)
    summary['known_complete_codex_credit_scenario'] = _decimal_text(credit_total)
    _finalize_summary(summary, invalid_json_lines=0)
    return summary


def _finalize_summary(summary, invalid_json_lines, empty_files=0):
    """Mark totals complete only when every requested metric record is usable."""
    requests = summary['requests']
    summary['measurement_complete'] = (
        requests['seen'] > 0 and requests['incomplete'] == 0
        and invalid_json_lines == 0 and empty_files == 0
    )
    if summary['measurement_complete']:
        summary['standard_api_hypothetical_usd'] = summary['known_complete_standard_api_hypothetical_usd']
    else:
        summary['standard_api_hypothetical_usd'] = None
    summary['codex_credit_scenario_complete'] = (
        summary['measurement_complete']
        and summary['credit_supported_complete_requests'] == requests['complete']
    )
    if summary['codex_credit_scenario_complete']:
        summary['codex_credit_scenario'] = summary['known_complete_codex_credit_scenario']
    else:
        summary['codex_credit_scenario'] = None


def load_metric_paths(paths):
    """Read JSONL metric files, returning safe parse counts rather than raw failures."""
    rows = []
    invalid_json_lines = 0
    empty_files = 0
    for path in paths:
        nonempty_lines = 0
        try:
            with path.open(encoding='utf-8') as source:
                for line in source:
                    if not line.strip():
                        continue
                    nonempty_lines += 1
                    try:
                        rows.append(json.loads(line))
                    except ValueError:
                        invalid_json_lines += 1
            if nonempty_lines == 0:
                empty_files += 1
        except (OSError, UnicodeError):
            rows.append(None)
    return rows, invalid_json_lines, empty_files


def _metric_paths(arguments):
    paths = [Path(value) for value in arguments.metrics]
    if arguments.metrics_folder is not None:
        folder = Path(arguments.metrics_folder)
        if not folder.is_dir():
            raise ValueError('metrics folder must exist')
        folder_paths = sorted(folder.glob('*.metrics.jsonl'))
        if not folder_paths:
            raise ValueError('metrics folder contains no metric files')
        paths.extend(folder_paths)
    if not paths:
        raise ValueError('at least one metric path or --metrics-folder is required')
    if len(paths) > MAX_METRIC_PATHS:
        raise ValueError('too many metric paths')
    if len({path.resolve() for path in paths}) != len(paths):
        raise ValueError('duplicate metric paths')
    return paths


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--label', required=True, help='user-provided comparison label')
    parser.add_argument('--metrics', action='append', default=[], metavar='PATH',
                        help='explicit codex_metrics JSONL path; repeat up to 128 paths')
    parser.add_argument('--metrics-folder', metavar='DIR',
                        help='explicit folder; only its direct *.metrics.jsonl files are read')
    arguments = parser.parse_args(argv)
    try:
        paths = _metric_paths(arguments)
        rows, invalid_json_lines, empty_files = load_metric_paths(paths)
        summary = summarize_rows(rows, arguments.label)
    except ValueError as error:
        parser.error(str(error))
    summary['sources'] = {'metric_files': len(paths), 'invalid_json_lines': invalid_json_lines,
                          'empty_files': empty_files}
    _finalize_summary(summary, invalid_json_lines, empty_files)
    print(json.dumps(summary, sort_keys=True, separators=(',', ':')))
    return 0 if summary['measurement_complete'] else 1


if __name__ == '__main__':
    sys.exit(main())
