#!/usr/bin/env python3
"""Aggregate private-safe Codex metric rows into hypothetical cost summaries.

The default contract remains GPT-6 Astra/medium. Other parent/child model
contracts must be explicit; a row never chooses its own allowed model or effort.
It consumes only the small usage records emitted by
``codex_metrics`` and never emits request bodies, responses, or identifiers.
"""
import argparse
from decimal import Decimal, ROUND_HALF_UP
import hashlib
import json
from pathlib import Path
import sys


MODEL = 'gpt-6-astra'
EFFORT = 'medium'
MAX_METRIC_PATHS = 128

MILLION = Decimal('1000000')

# The one price list.  The Rust measurement code embeds this same file, so
# the two sides cannot drift into pricing the same usage differently.
RATES_PATH = Path(__file__).resolve().parent / 'rates.json'
_RATES_BYTES = RATES_PATH.read_bytes()
RATES_SHA256 = hashlib.sha256(_RATES_BYTES).hexdigest()
_RATES_TABLE = json.loads(_RATES_BYTES)


def _load_rates(model=MODEL):
    entry = _RATES_TABLE['models'].get(model)
    if entry is None:
        # Another model's figures would look like a measurement and be none.
        raise KeyError(f'rates.json does not name {model}')
    return entry


def _cache_applies(entry):
    """Whether the model in the table has a cache at all.

    ``not_applicable`` makes both cache shares zero by definition when a cost
    is worked out; the record keeps saying whatever it said.  The Rust
    measurement code reads the same field from the same file, so the two
    sides cannot price a cacheless model differently.  A word nobody defined
    is refused rather than read as either answer.
    """
    declared = entry.get('cache')
    if declared is None:
        return True
    if declared == 'not_applicable':
        return False
    raise ValueError(f'rates.json declares an unreadable cache value: {declared!r}')


_RATES = _load_rates()
CACHE_APPLIES = _cache_applies(_RATES)
LONG_CONTEXT_LIMIT = _RATES['long_context']['input_tokens_above']
LONG_INPUT_MULTIPLIER = Decimal(_RATES['long_context']['input_multiplier'])
LONG_OUTPUT_MULTIPLIER = Decimal(_RATES['long_context']['output_multiplier'])
RATES_AS_OF = _RATES['rates_as_of']
PRICING_SOURCES = list(_RATES['sources'])
API_RATES = {name: Decimal(value)
             for name, value in _RATES['api_per_million'].items()}
CREDIT_RATES = {name: Decimal(value)
                for name, value in _RATES['codex_credit_per_million'].items()}
USAGE_FIELDS = ('input_tokens', 'cache_read_tokens', 'cache_write_tokens',
                'output_tokens', 'total_tokens')


def _integer(value):
    return type(value) is int and value >= 0


def _decimal_text(value):
    """Produce a stable JSON-safe Decimal representation.

    Quantizing to the unit of account pads with zeros; they are removed so
    the emitted text keeps the shape earlier records already have.
    """
    text = format(value, 'f')
    if '.' not in text:
        return text
    return text.rstrip('0').rstrip('.') or '0'


def _usage_summary(usage, cache_applies=None):
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

    cache_fields = ('cache_read_tokens', 'cache_write_tokens')
    if not (CACHE_APPLIES if cache_applies is None else cache_applies):
        # There is no cache to report, so an absent figure is not a missing
        # measurement: it is zero by the table's rule, and the record above
        # still says nothing was reported.  One above zero is the table and
        # the provider disagreeing, left showing rather than absorbed.
        for field in cache_fields:
            if known[field] is None:
                known[field] = 0
        if any(known[field] > 0 for field in cache_fields):
            errors.append('cache_declared_absent')
    for field in USAGE_FIELDS[:4]:
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


def _quantize(value):
    """Round to the unit of account, one micro-USD, half away from zero.

    The Rust measurement code counts in whole micro-USD, so the two agree
    only if the same rounding is applied here.  Every rate in the table lands
    on a whole micro-USD except the cache write, which is half a one per
    token.
    """
    return value.quantize(Decimal('0.000001'), rounding=ROUND_HALF_UP)


def _price(known, rates, long_context_multipliers, long_context=None):
    context = _RATES['long_context'] if long_context is None else long_context
    multiplier = (Decimal(context['input_multiplier']), Decimal(context['output_multiplier'])) if (
        long_context_multipliers and context is not None
        and known['input_tokens'] > context['input_tokens_above']
    ) else (Decimal('1'), Decimal('1'))
    input_multiplier, output_multiplier = multiplier
    ordinary = known['input_tokens'] - known['cache_read_tokens'] - known['cache_write_tokens']
    return _quantize(
        ((rates['billed_input'] * ordinary + rates['cache_read'] * known['cache_read_tokens']
          + rates.get('cache_write', Decimal('0')) * known['cache_write_tokens']) * input_multiplier
         + rates['output'] * known['output_tokens'] * output_multiplier) / MILLION)


def price_usage(model, input_tokens, cache_read_tokens, cache_write_tokens,
                output_tokens, *, long_context_multipliers=True):
    """Price one model's observed usage, for an arm that has no metric rows.

    `classify_row` prices a polaris `metrics.jsonl` record and validates the
    model and effort it declares. The stock-codex arm has no such record: its
    usage comes from `turn.completed` events and rollout `token_usage_record`
    payloads, which carry no effort and no polaris row shape. Both arms must be
    priced by the same table and the same arithmetic or the comparison measures
    the two pricing paths instead of the two products, so the formula lives
    here once and each arm supplies only its four counts.

    Raises `KeyError` when `rates.json` does not name the model. A model whose
    entry declares no cache tier has its cache counts folded into the ordinary
    input, per the `cache_rule` the table itself states.
    """
    entry = _load_rates(model)
    rates = {name: Decimal(value) for name, value in entry['api_per_million'].items()}
    context = entry.get('long_context', {})
    counts = {'input_tokens': _require_count(input_tokens),
              'cache_read_tokens': _require_count(cache_read_tokens),
              'cache_write_tokens': _require_count(cache_write_tokens),
              'output_tokens': _require_count(output_tokens)}
    if counts['cache_read_tokens'] + counts['cache_write_tokens'] > counts['input_tokens']:
        raise ValueError('cache tokens exceed the reported input tokens')
    if not _cache_applies(entry):
        counts['cache_read_tokens'] = 0
        counts['cache_write_tokens'] = 0
    return _price(counts, rates, long_context_multipliers and bool(context), context or None)


def _require_count(value):
    if not _integer(value) or value < 0:
        raise ValueError('token counts must be non-negative integers')
    return value


def _validate_expected_models(expected_models):
    if expected_models is not None and (
        not isinstance(expected_models, dict) or not expected_models
        or any(not isinstance(name, str) or not name.strip()
               or not isinstance(effort, str) or not effort.strip()
               for name, effort in expected_models.items())
    ):
        raise ValueError('expected models must be a non-empty model-to-effort mapping')


def classify_row(row, *, expected_models=None):
    """Classify one decoded metric record without retaining private record data."""
    if not isinstance(row, dict):
        return {'eligible': False, 'complete': False, 'outcome': None,
                'known_usage': {field: None for field in USAGE_FIELDS},
                'errors': ['invalid_row']}

    _validate_expected_models(expected_models)
    models = {MODEL: EFFORT} if expected_models is None else expected_models
    model = row.get('model')
    declared = isinstance(model, str) and model in models
    entry = None
    if declared:
        try:
            entry = _load_rates(model)
        except KeyError:
            pass
    cache_applies = (None if expected_models is None else
                     _cache_applies(entry) if entry is not None else True)
    known, usage_errors, _ = _usage_summary(row.get('usage'), cache_applies)
    errors = list(usage_errors)
    if not declared:
        errors.append('wrong_model')
    elif entry is None:
        errors.append('unknown_model_rates')
    if declared and row.get('effort') != models[model]:
        errors.append('wrong_effort')
    usage = row.get('usage')
    reasoning = usage.get('reasoning_tokens') if isinstance(usage, dict) else None
    reasoning_state = 'missing'
    if reasoning is not None:
        if not _integer(reasoning):
            errors.append('invalid_reasoning_tokens')
            reasoning_state = 'invalid'
        elif known['output_tokens'] is not None and reasoning > known['output_tokens']:
            errors.append('reasoning_exceeds_output')
            reasoning_state = 'invalid'
        else:
            reasoning_state = 'known'
    outcome = row.get('outcome')
    if outcome != 'completed':
        errors.append('outcome_not_completed')
    complete = not errors
    result = {'eligible': declared and row.get('effort') == models[model],
              'complete': complete, 'outcome': outcome, 'known_usage': known,
              'errors': errors, 'reasoning_state': reasoning_state,
              'reasoning_tokens': reasoning if reasoning_state == 'known' else None}
    if complete:
        rates = {name: Decimal(value) for name, value in entry['api_per_million'].items()}
        # An empty context means this model has no long-context surcharge;
        # never inherit Astra's boundary for another model.
        context = entry.get('long_context', {})
        result['standard_api_hypothetical_usd'] = _decimal_text(
            _price(known, rates, bool(context), context))
        credit = entry.get('codex_credit_per_million')
        if credit is not None and known['cache_write_tokens'] == 0:
            credit = {name: Decimal(value) for name, value in credit.items()}
            result['codex_credit_scenario'] = _decimal_text(_price(known, credit, False))
            result['codex_credit_scenario_complete'] = True
        else:
            result['codex_credit_scenario'] = None
            result['codex_credit_scenario_complete'] = False
    return result


def summarize_rows(rows, label, *, expected_models=None):
    """Summarize decoded per-request metrics for a caller or the JSON CLI."""
    if not isinstance(label, str) or not label:
        raise ValueError('label must be a non-empty string')
    _validate_expected_models(expected_models)
    summary = {
        'label': label,
        'contract': {
            'model': MODEL,
            'effort': EFFORT,
            'rates_as_of': RATES_AS_OF,
            'rates_sha256': RATES_SHA256,
            'pricing_sources': PRICING_SOURCES,
            'standard_api_rates_per_million_tokens': {
                f'{name}_usd': _decimal_text(value)
                for name, value in API_RATES.items()
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
        'reasoning': {'known_tokens': 0, 'missing_requests': 0, 'invalid_requests': 0,
                      'included_in_output': True},
    }
    if expected_models is not None:
        contracts = {}
        for model, effort in expected_models.items():
            try:
                entry = _load_rates(model)
            except KeyError:
                entry = {}
            contracts[model] = {
                'effort': effort, 'rates_as_of': entry.get('rates_as_of'),
                'pricing_sources': entry.get('sources', []),
                'standard_api_rates_per_million_tokens': entry.get('api_per_million'),
            }
        # Mixed model output must not label every row with the legacy model.
        summary['contract'] = {
            'models': contracts, 'standard_api_hypothetical': True,
            'rates_sha256': RATES_SHA256,
            'codex_credit_scenario_is_not_invoice_or_rate_limit': True,
            'codex_credit_scenario_requires_zero_cache_write_tokens': True,
        }
        summary['by_model'] = {
            model: {'requests': 0, 'complete_requests': 0,
                    'known_complete_standard_api_hypothetical_usd': '0'}
            for model in expected_models
        }
    api_total = Decimal('0')
    credit_total = Decimal('0')
    for row in rows:
        classified = classify_row(row, expected_models=expected_models)
        reasoning_state = classified.get('reasoning_state', 'missing')
        if reasoning_state == 'known':
            summary['reasoning']['known_tokens'] += classified['reasoning_tokens']
        else:
            summary['reasoning'][reasoning_state + '_requests'] += 1
        model = row.get('model') if isinstance(row, dict) else None
        model_summary = (summary.get('by_model', {}).get(model)
                         if isinstance(model, str) else None)
        if model_summary is not None:
            model_summary['requests'] += 1
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
            if model_summary is not None:
                model_summary['complete_requests'] += 1
                field = 'known_complete_standard_api_hypothetical_usd'
                model_summary[field] = _decimal_text(
                    Decimal(model_summary[field]) + Decimal(classified['standard_api_hypothetical_usd']))
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
    parser.add_argument('--expect-model', action='append', metavar='MODEL=EFFORT',
                        help='explicit allowed model/effort; repeat for parent and child')
    arguments = parser.parse_args(argv)
    try:
        paths = _metric_paths(arguments)
        rows, invalid_json_lines, empty_files = load_metric_paths(paths)
        models = None
        if arguments.expect_model is not None:
            models = {}
            for spec in arguments.expect_model:
                name, separator, effort = spec.partition('=')
                if not separator or not name or not effort or name in models or '=' in effort:
                    raise ValueError('expected unique MODEL=EFFORT arguments')
                models[name] = effort
        summary = summarize_rows(rows, arguments.label, expected_models=models)
    except ValueError as error:
        parser.error(str(error))
    summary['sources'] = {'metric_files': len(paths), 'invalid_json_lines': invalid_json_lines,
                          'empty_files': empty_files}
    _finalize_summary(summary, invalid_json_lines, empty_files)
    print(json.dumps(summary, sort_keys=True, separators=(',', ':')))
    return 0 if summary['measurement_complete'] else 1


if __name__ == '__main__':
    sys.exit(main())
