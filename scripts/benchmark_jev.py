"""Account only physical Jev attempts; logical meter totals are not added again."""
from decimal import Decimal
import json

from benchmark_costs import RATES_SHA256, _RATES_TABLE


def count(value):
    return type(value) is int and value >= 0


def audit(path, *, expected_logical_calls):
    """Preserve failed/unfinished attempts and unknown usage, without raw payloads."""
    if not count(expected_logical_calls):
        raise ValueError('expected logical call count must be a nonnegative integer')
    held, issues, structural = {}, [], []
    try:
        lines = path.read_text().splitlines()
    except (OSError, UnicodeError):
        lines = []
        structural.append('missing_or_unreadable_journal')
    invariant = ('model', 'logical_id', 'attempt_id', 'attempt_number', 'started_unix_ms')
    keys = set(invariant) | {'schema_version', 'provider', 'event', 'finished_unix_ms',
                             'http_status', 'outcome', 'usage'}
    for number, line in enumerate(lines, 1):
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except ValueError:
            structural.append(f'invalid_json:{number}')
            continue
        if (not isinstance(row, dict) or set(row) != keys
                or row.get('schema_version') != 1 or row.get('provider') != 'jev'
                or any(not isinstance(row.get(k), str) or not row[k]
                       for k in ('model', 'logical_id', 'attempt_id'))
                or not count(row.get('attempt_number')) or row['attempt_number'] < 1
                or not count(row.get('started_unix_ms'))
                or not isinstance(row.get('usage'), dict)
                or set(row['usage']) != {'input_tokens', 'output_tokens'}
                or any(v is not None and not count(v) for v in row['usage'].values())):
            structural.append(f'invalid_record:{number}')
            continue
        identity = row['attempt_id']
        if row['event'] == 'started':
            if (identity in held or row['outcome'] != 'running'
                    or any(row[k] is not None for k in ('finished_unix_ms', 'http_status'))
                    or any(v is not None for v in row['usage'].values())):
                structural.append(f'invalid_start:{number}')
                continue
            held[identity] = {'start': row, 'end': None}
        elif row['event'] == 'finished':
            attempt = held.get(identity)
            if (attempt is None or attempt['end'] is not None
                    or any(row[k] != attempt['start'][k] for k in invariant)
                    or not count(row['finished_unix_ms'])
                    or row['finished_unix_ms'] < row['started_unix_ms']
                    or row['outcome'] not in ('completed', 'too_large', 'invalid_answer',
                                              'malformed', 'auth_error', 'unavailable', 'cancelled')
                    or (row['http_status'] is not None
                        and (not count(row['http_status']) or not 100 <= row['http_status'] <= 599))):
                structural.append(f'invalid_terminal:{number}')
                continue
            attempt['end'] = row
        else:
            structural.append(f'invalid_event:{number}')
    logical = {a['start']['logical_id'] for a in held.values()}
    if len(logical) != expected_logical_calls:
        structural.append('logical_count_mismatch')
    # This benchmark explicitly disables transport retries. Keep all observed
    # attempts for accounting, but never accept retries or duplicated starts as
    # a complete run under that contract.
    if len(held) != expected_logical_calls or any(
            a['start']['attempt_number'] != 1
            or a['start']['attempt_id'] != a['start']['logical_id'] + '-1'
            for a in held.values()):
        structural.append('unexpected_physical_attempt_count')
    known = {'input': 0, 'output': 0}
    missing = {'input': 0, 'output': 0}
    known_usd = Decimal(0)
    unpriced = 0
    attempts = []
    for identity, attempt in held.items():
        row = attempt['end'] or attempt['start']
        usage = row['usage']
        for key in known:
            value = usage[key + '_tokens']
            if count(value):
                known[key] += value
            else:
                missing[key] += 1
        if attempt['end'] is None:
            issues.append(f'unfinished:{identity}')
        elif row['outcome'] != 'completed':
            issues.append(f'failed:{identity}')
        entry = _RATES_TABLE.get('typed_decision_models', {}).get(row['model'])
        priceable = (entry is not None and entry['billing'] == 'published_flat_input'
                     and entry['output_per_million'] == '0' and count(usage['input_tokens'])
                     and attempt['end'] is not None and row['http_status'] is not None
                     and 200 <= row['http_status'] < 300)
        if priceable:
            known_usd += Decimal(entry['input_per_million']) * usage['input_tokens'] / Decimal(1000000)
        else:
            unpriced += 1
        attempts.append(row)
    for key, value in missing.items():
        if value:
            issues.append('missing_' + key)
    return {'physical_attempts': len(held), 'logical_calls': len(logical),
            'status': 'exercised' if held else 'not_exercised',
            'measurement_complete': not structural and not issues and not unpriced,
            'tokens': {k: known[k] if not structural and not missing[k] else None for k in known},
            'known_tokens': known, 'missing_fields': missing,
            'usd': str(known_usd) if not structural and not unpriced else None,
            'known_usd': str(known_usd), 'unpriced_attempts': unpriced,
            'issues': structural + issues, 'rates_sha256': RATES_SHA256,
            'cache_observation': 'not_reported', 'attempts': attempts}
