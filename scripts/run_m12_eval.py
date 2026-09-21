#!/usr/bin/env python3
"""Freeze and run the four authorized M12 pairs without automatic retries.

Preparation never reads authentication or grants permission. A matching scoped
authorization receipt and common-runner readonly environment are required for
execution. Observed fee ceilings are not strict service billing limits.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess


MAX_FEE = 5_000_000
PAIR_SENDS = 2
PAIR_TIMEOUT = 260
CASE_IDS = [f'I{number:02}' for number in range(1, 5)]
CASE_FIELDS = {
    'I01': {'project', 'rename_variable', 'timeout', 'unit', 'production_write',
            'local_condition', 'database_password'},
    'I02': {'occurrences', 'name', 'identifier'},
    'I03': {'final_limit'},
    'I04': {'last_entry_number'},
}
SCORE_FLAGS = ('json_valid', 'exact_keys', 'answer_correct', 'evidence_correct',
               'quality_complete', 'all_correct')
FROZEN_INPUTS = {'cases.json', 'rates.json', 'run_m12_eval.py',
                 'm12-model-harness-contract.md', 'm12-contract.json'}


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_new(path, value):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w', encoding='utf-8') as output:
        json.dump(value, output, ensure_ascii=False, indent=2)
        output.write('\n')
        output.flush()
        os.fsync(output.fileno())


def integer(value, minimum=0):
    if type(value) is not int or value < minimum:
        raise ValueError('invalid integer')
    return value


def pairs():
    return [{'index': index, 'case': case,
             'arms': ['off', 'repeat_refs'] if index % 2 == 0 else ['repeat_refs', 'off']}
            for index, case in enumerate(CASE_IDS)]


def local_file(work, relative):
    relative = Path(relative)
    if relative.is_absolute() or '..' in relative.parts:
        raise ValueError('evidence must remain in the work directory')
    path = work / relative
    if path.resolve(strict=True) != path or not path.is_file():
        raise ValueError('evidence must be an ordinary local file')
    return path


def validate_cases(work):
    cases = json.loads(local_file(work, 'cases.json').read_text(encoding='utf-8'))
    rows = cases['cases']
    if [case['id'] for case in rows] != CASE_IDS:
        raise ValueError('expected the four fixed M12 cases')
    for case in rows:
        source = case['input'].encode('utf-8')
        if (not source or len(source) > 256 * 1024
                or integer(case['input_bytes'], 1) != len(source)
                or hashlib.sha256(source).hexdigest() != case['sha256']
                or type(case['expected']) is not dict
                or set(case['expected']) != CASE_FIELDS[case['id']]
                or type(case['expected_evidence']) is not dict
                or set(case['expected_evidence']) != CASE_FIELDS[case['id']]):
            raise ValueError('case source or expectation mismatch')
    return {case['id']: case for case in rows}


def campaign(auth_file, fee):
    integer(fee, 1)
    if fee > MAX_FEE or not auth_file.is_absolute() or '..' in auth_file.parts:
        raise ValueError('fee ceiling or authentication path invalid')
    return {
        'version': 1, 'pairs': pairs(),
        'root': {'model': 'gpt-6-astra', 'effort': 'xhigh'},
        'max_physical_sends': 8, 'max_sends_per_arm': 1,
        'arm_timeout_seconds': 120, 'pair_timeout_seconds': PAIR_TIMEOUT,
        'request_bytes_cap': 131072, 'observed_output_tokens_cap': 4096,
        'observed_fee_ceiling_microusd': fee,
        'fee_ceiling_is_strict_billing_limit': False,
        'auth_file': str(auth_file),
    }


def prepare(work, binary, auth_file, fee):
    contract = campaign(auth_file, fee)
    if (not binary.is_absolute() or binary.resolve(strict=True) != binary
            or not binary.is_file() or not os.access(binary, os.X_OK)):
        raise ValueError('binary must be an absolute ordinary executable')
    if (any(work.glob('pair-*'))
            or any((work / name).exists() for name in (
                'm12-contract.json', 'm12-freeze.json', 'm12-authorization.json', 'm12-stopped.json'))):
        raise ValueError('campaign already prepared or started; no reset')
    validate_cases(work)
    files = {name: digest(local_file(work, name))
             for name in sorted(FROZEN_INPUTS - {'m12-contract.json'})}
    if files['run_m12_eval.py'] != digest(Path(__file__).resolve()):
        raise ValueError('work script differs from the executing launcher')
    write_new(work / 'm12-contract.json', contract)
    files['m12-contract.json'] = digest(work / 'm12-contract.json')
    write_new(work / 'm12-freeze.json', {
        'version': 1, 'binary_path': str(binary), 'binary_sha256': digest(binary), 'files': files,
    })
    return {'status': 'prepared_not_authorized', 'pairs': contract['pairs'],
            'max_physical_sends': 8, 'observed_fee_ceiling_microusd': fee,
            'freeze_sha256': digest(work / 'm12-freeze.json')}


def verify(work):
    frozen = json.loads(local_file(work, 'm12-freeze.json').read_text(encoding='utf-8'))
    authorization = json.loads(local_file(work, 'm12-authorization.json').read_text(encoding='utf-8'))
    if (authorization.get('status') != 'approved'
            or authorization.get('freeze_sha256') != digest(work / 'm12-freeze.json')):
        raise ValueError('authorization does not match this frozen campaign')
    if (integer(frozen['version']) != 1 or set(frozen['files']) != FROZEN_INPUTS
            or set(frozen) != {'version', 'binary_path', 'binary_sha256', 'files'}):
        raise ValueError('frozen inputs missing or unexpected')
    for name, expected in frozen['files'].items():
        if digest(local_file(work, name)) != expected:
            raise ValueError('frozen input changed')
    if frozen['files']['run_m12_eval.py'] != digest(Path(__file__).resolve()):
        raise ValueError('executing launcher is not the frozen version')
    validate_cases(work)
    contract = json.loads(local_file(work, 'm12-contract.json').read_text(encoding='utf-8'))
    expected = campaign(Path(contract['auth_file']), contract['observed_fee_ceiling_microusd'])
    # JSON comparison preserves bool versus integer distinctions for every fixed cap.
    if json.dumps(contract, sort_keys=True) != json.dumps(expected, sort_keys=True):
        raise ValueError('campaign contract mismatch')
    binary = Path(frozen['binary_path'])
    if (not binary.is_absolute() or binary.resolve(strict=True) != binary
            or not binary.is_file() or not os.access(binary, os.X_OK)
            or digest(binary) != frozen['binary_sha256']):
        raise ValueError('binary changed')
    if os.environ.get('POLARIS_AUTH_READ_ONLY') != '1':
        raise ValueError('common-runner readonly authentication required')
    return contract, binary


def claim_for(work, pair):
    return {'version': 1, 'pair': pair, 'freeze_sha256': digest(work / 'm12-freeze.json')}


def score_summary(score, case_id):
    if type(score) is not dict or any(type(score.get(name)) is not bool for name in SCORE_FLAGS):
        raise ValueError('complete typed score flags required')
    result = {name: score[name] for name in SCORE_FLAGS}
    # Only fixed field names and booleans can reach launcher stdout.
    for name in ('field_answer_correct', 'field_evidence_correct'):
        fields = score.get(name)
        if (type(fields) is not dict or set(fields) != CASE_FIELDS[case_id]
                or any(type(value) is not bool for value in fields.values())):
            raise ValueError('score field set or type mismatch')
        result[name] = fields
    if (score['quality_complete'] != (score['exact_keys'] and score['answer_correct']
                                     and score['evidence_correct'])
            or score['all_correct'] != score['quality_complete']
            or (score['exact_keys'] and not score['json_valid'])
            or (score['answer_correct'] and not all(score['field_answer_correct'].values()))
            or (score['evidence_correct'] and not all(score['field_evidence_correct'].values()))):
        raise ValueError('score aggregate mismatch')
    return result


def sha256(value):
    if (type(value) is not str or len(value) != 64
            or any(character not in '0123456789abcdef' for character in value)):
        raise ValueError('sha256 missing or invalid')
    return value


def check_report(work, prefix, case, arm, report):
    if (integer(report['schema_version']) != 1 or report['case'] != case['id']
            or report['mode'] != arm or report['raw_original_verified_after_reopen'] is not True
            or report.get('observation_complete') is not True
            or type(report.get('measurement_complete')) is not bool
            or integer(report['physical_sends']) != 1
            or any(report[name] is not None for name in ('error', 'outcome_error', 'accounting_error'))):
        raise ValueError('result identity, raw verification or physical measurement mismatch')
    response_name = f"{case['id']}-{arm}.response.json"
    if (report['response_file'] != response_name
            or sha256(report['response_sha256']) != digest(local_file(work, f'{prefix}/{response_name}'))):
        raise ValueError('response evidence identity or hash mismatch')

    preparation = report['preparation']
    metrics = preparation['preparation']
    request = report['request']
    expected_reason = 'off' if arm == 'off' else case['expected_projection']
    if (preparation['source_sha256'] != case['sha256']
            or metrics['reason'] != expected_reason
            or integer(metrics['input_bytes'], 1) != case['input_bytes']
            or request['source'] != 'actual_provider_request'
            or integer(request['tool_schemas']) != 0
            or request['endpoint_output_cap_verified'] is not False):
        raise ValueError('provider-boundary input source or preparation mismatch')
    user_hash = sha256(request['user_sha256'])
    user_bytes = integer(request['user_bytes'], 1)
    user_tokens = integer(request['user_tokens'], 1)
    serialized_bytes = integer(request['serialized_bytes'], 1)
    sha256(request['system_sha256'])
    sha256(request['serialized_sha256'])
    if not user_bytes <= serialized_bytes <= 131072:
        raise ValueError('serialized provider request byte limit mismatch')
    if expected_reason == 'applied':
        if (user_hash == case['sha256']
                or user_bytes != integer(metrics['output_bytes'], 1)
                or user_tokens != integer(metrics['output_tokens'], 1)
                or user_tokens >= integer(metrics['input_tokens'], 1)):
            raise ValueError('applied provider projection differs from preparation')
    elif (user_hash != case['sha256'] or user_bytes != case['input_bytes']
          or (arm != 'off' and user_tokens != integer(metrics['input_tokens'], 1))):
        raise ValueError('unprojected provider input differs from frozen original')
    if arm == 'off' and any(metrics[name] is not None for name in
                            ('input_tokens', 'output_bytes', 'output_tokens')):
        raise ValueError('off preparation unexpectedly reports projection measurements')

    accounting = report['accounting']
    lower = integer(accounting['lower_cost_microusd'])
    upper = integer(accounting['upper_cost_microusd'])
    exact = accounting['exact_cost_microusd']
    exact_complete = accounting['exact_fee_complete']
    if (accounting['source'] != 'physical_metrics_only' or lower > upper
            or type(exact_complete) is not bool
            or report['measurement_complete'] != exact_complete
            or integer(report['known_cost_upper_microusd']) != upper
            or (exact_complete and not lower <= integer(exact) <= upper)
            or (not exact_complete and exact is not None)):
        raise ValueError('cost observation or exact-fee completeness mismatch')
    by_model = accounting['by_model']
    if (type(by_model) is not dict or set(by_model) != {'gpt-6-astra'}
            or integer(by_model['gpt-6-astra']['physical_sends']) != 1
            or integer(by_model['gpt-6-astra']['lower_cost_microusd']) != lower
            or integer(by_model['gpt-6-astra']['upper_cost_microusd']) != upper):
        raise ValueError('per-model cost aggregate mismatch')
    return upper, score_summary(report['score'], case['id'])


def check_pair(work, pair, output):
    prefix = f"pair-{pair['index']:02}"
    claim = json.loads(local_file(work, prefix + '.claim.json').read_text(encoding='utf-8'))
    if json.dumps(claim, sort_keys=True) != json.dumps(claim_for(work, pair), sort_keys=True):
        raise ValueError('pair claim changed')
    if output != work / prefix or output.resolve(strict=True) != output or not output.is_dir():
        raise ValueError('pair output path mismatch')
    complete = json.loads(local_file(work, f'{prefix}/complete.json').read_text(encoding='utf-8'))
    if (complete.get('state') != 'complete' or integer(complete['trials']) != 2
            or integer(complete['physical_sends']) != PAIR_SENDS
            or complete.get('price_table_sha256') != digest(work / 'rates.json')):
        raise ValueError('pair did not complete under the frozen rates')
    fee = 0
    scores = []
    measurement_complete = True
    case = validate_cases(work)[pair['case']]
    for arm in pair['arms']:
        name = f"{prefix}/{pair['case']}-{arm}.result.json"
        report = json.loads(local_file(work, name).read_text(encoding='utf-8'))
        upper, score = check_report(work, prefix, case, arm, report)
        fee += upper
        measurement_complete &= report['measurement_complete']
        scores.append({'arm': arm, **score})
    if fee != integer(complete['observed_cost_upper_microusd']):
        raise ValueError('pair fee aggregate mismatch')
    files = {}
    for path in output.rglob('*'):
        if path.is_symlink() or path.resolve(strict=True) != path:
            raise ValueError('pair evidence cannot contain symlinks')
        if path.is_file():
            relative = str(path.relative_to(work))
            files[relative] = digest(local_file(work, relative))
        elif not path.is_dir():
            raise ValueError('pair evidence must be ordinary files')
    required = {f'{prefix}/{name}' for name in
                ('attempts.json', 'metrics.jsonl', 'started.json', 'plan.json', 'complete.json')}
    if not required <= files.keys():
        raise ValueError('pair evidence missing')
    for suffix in ('.claim.json', '.stdout.log', '.stderr.log'):
        name = prefix + suffix
        files[name] = digest(local_file(work, name))
    return {'state': 'complete', 'pair': pair, 'physical_sends': PAIR_SENDS,
            'observed_cost_upper_microusd': fee, 'files': files,
            'observation_complete': True, 'measurement_complete': measurement_complete,
            'scores': scores}


def checked_totals(work, contract, index):
    sends = fee = 0
    for pair in contract['pairs'][:index]:
        prefix = f"pair-{pair['index']:02}"
        checked = json.loads(local_file(work, prefix + '.checked.json').read_text(encoding='utf-8'))
        actual = check_pair(work, pair, work / prefix)
        if json.dumps(checked, sort_keys=True) != json.dumps(actual, sort_keys=True):
            raise ValueError('previous receipt or evidence changed')
        sends += actual['physical_sends']
        fee += actual['observed_cost_upper_microusd']
    if sends != index * PAIR_SENDS or fee >= contract['observed_fee_ceiling_microusd']:
        raise ValueError('campaign observed limit reached')
    return sends, fee


def run_pair(work, index):
    contract, binary = verify(work)
    integer(index)
    if index >= len(contract['pairs']) or (work / 'm12-stopped.json').exists():
        raise ValueError('unknown pair or campaign stopped')
    before_sends, before_fee = checked_totals(work, contract, index)
    for pending in contract['pairs'][index:]:
        if any(work.glob(f"pair-{pending['index']:02}*")):
            raise ValueError('unexpected started pair; no retry or reordering')
    pair = contract['pairs'][index]
    prefix = f'pair-{index:02}'
    output = work / prefix
    write_new(work / (prefix + '.claim.json'), claim_for(work, pair))
    remaining = contract['observed_fee_ceiling_microusd'] - before_fee
    env = os.environ.copy()
    env['POLARIS_AUTH_READ_ONLY'] = '1'
    env['POLARIS_WS'] = 'off'
    env['POLARIS_METRICS_PATH'] = str(output / 'metrics.jsonl')
    for name in ('POLARIS_CACHE_NAMESPACE', 'POLARIS_CACHE_MODE', 'POLARIS_CACHE_PREFIX',
                 'POLARIS_CACHE_PACING', 'POLARIS_TURN_AFFINITY'):
        env.pop(name, None)
    command = [str(binary), '--work-dir', str(work), '--cases', str(work / 'cases.json'),
               '--output', str(output), '--case', pair['case'], '--live',
               '--auth-file', contract['auth_file'], '--fee-cap-microusd', str(remaining)]
    try:
        # Private logs are durable evidence. No driver text reaches launcher stdout.
        with (work / (prefix + '.stdout.log')).open('xb') as stdout:
            os.chmod(stdout.fileno(), 0o600)
            with (work / (prefix + '.stderr.log')).open('xb') as stderr:
                os.chmod(stderr.fileno(), 0o600)
                result = subprocess.run(command, cwd=work, env=env, timeout=PAIR_TIMEOUT,
                                        stdout=stdout, stderr=stderr, check=False)
                stdout.flush()
                stderr.flush()
                os.fsync(stdout.fileno())
                os.fsync(stderr.fileno())
        if result.returncode:
            raise ValueError('pair driver failed; no retry')
        checked = check_pair(work, pair, output)
        if (before_fee + checked['observed_cost_upper_microusd']
                > contract['observed_fee_ceiling_microusd']
                or before_sends + checked['physical_sends'] > contract['max_physical_sends']):
            raise ValueError('campaign observed limit exceeded')
        write_new(work / (prefix + '.checked.json'), checked)
        return checked
    except (ValueError, OSError, KeyError, TypeError, subprocess.TimeoutExpired):
        write_new(work / 'm12-stopped.json', {'pair': pair, 'reason': 'pair_failed_or_incomplete'})
        raise


def public_result(result):
    return {key: value for key, value in result.items() if key != 'files'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--work-dir', type=Path, required=True)
    commands = parser.add_subparsers(dest='command', required=True)
    prep = commands.add_parser('prepare')
    prep.add_argument('--binary', type=Path, required=True)
    prep.add_argument('--auth-file', type=Path, required=True)
    prep.add_argument('--fee-cap-microusd', type=int, required=True)
    run = commands.add_parser('run-pair')
    run.add_argument('index', type=int)
    args = parser.parse_args()
    try:
        work = args.work_dir
        if not work.is_absolute() or work.resolve(strict=True) != work or not work.is_dir():
            raise ValueError('work directory must be absolute and canonical')
        result = (prepare(work, args.binary, args.auth_file, args.fee_cap_microusd)
                  if args.command == 'prepare' else run_pair(work, args.index))
        print(json.dumps(public_result(result), ensure_ascii=False))
    except (ValueError, OSError, KeyError, TypeError, subprocess.TimeoutExpired) as error:
        parser.exit(1, f'M12 campaign を停止しました ({type(error).__name__})。専用 work の記録を確認してください。\n')


if __name__ == '__main__':
    main()
