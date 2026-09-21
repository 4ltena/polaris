#!/usr/bin/env python3
"""Freeze and run one M23 pair per common-runner command, never retry a pair.

Preparation makes no model calls and does not grant permission. The caller must
record scoped user authorization before running this through benchmark-execution.
Each process fits the runner's 600-second command limit; campaign costs carry
across processes through hash-checked, ordered completion receipts.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time


MAX_FEE = 40_000_000
PAIR_SENDS = 15
PAIR_TIMEOUT = 500


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
        raise ValueError('invalid non-negative integer')
    return value


def pairs(schedule):
    if schedule == 'pilot':
        return [{'index': 0, 'case': 'R03', 'repeat': 1,
                 'arms': ['direct', 'file-reader']}]
    if schedule != 'full':
        raise ValueError('unknown schedule')
    result = []
    for number in range(1, 7):
        for repeat in range(1, 4):
            index = len(result)
            arms = ['direct', 'file-reader']
            result.append({'index': index, 'case': f'R{number:02}', 'repeat': repeat,
                           'arms': arms if index % 2 == 0 else arms[::-1]})
    return result


def local_file(work, relative):
    path = work / relative
    if not path.is_relative_to(work) or path.resolve(strict=True) != path or not path.is_file():
        raise ValueError('input must be a regular file inside the work directory')
    return path


def case_project(work, case):
    """Check the fixture project marker the driver requires for one case.

    `m23_read_eval` refuses a case whose source does not sit directly inside a
    directory holding a `.polaris` marker, because without it `resolve_root`
    walks up and adopts the real repository. The marker is an empty directory,
    so a preparation that copies the frozen file list alone drops it in
    silence — and the driver then fails after `run_pair` has written the
    one-shot claim, ending the campaign for a condition visible beforehand.
    """
    project = local_file(work, case['path']).parent
    if not (project / '.polaris').is_dir():
        raise ValueError('fixture project marker missing')


def prepare(work, binary, auth_file, schedule, fee):
    integer(fee, 1)
    if fee > MAX_FEE or not auth_file.is_absolute():
        raise ValueError('fee ceiling or auth path invalid')
    case_file = local_file(work, 'cases.json')
    cases = json.loads(case_file.read_text())['cases']
    if [case['id'] for case in cases] != [f'R{n:02}' for n in range(1, 7)]:
        raise ValueError('expected the six fixed M23 cases')
    files = {'cases.json': digest(case_file)}
    for case in cases:
        path = local_file(work, case['path'])
        if digest(path) != case['sha256']:
            raise ValueError('case source hash mismatch')
        case_project(work, case)
        files[case['path']] = case['sha256']
    # These copies are explicit frozen inputs, prepared by the parent.
    for name in ('rates.json', 'run_m23_eval.py'):
        files[name] = digest(local_file(work, name))
    if files['run_m23_eval.py'] != digest(Path(__file__).resolve()):
        raise ValueError('work script differs from the executing launcher')
    contract = {
        'version': 1, 'schedule': schedule, 'pairs': pairs(schedule),
        'root': {'model': 'gpt-6-astra', 'effort': 'xhigh'},
        'child': {'model': 'gpt-5.6-luna', 'effort': 'xhigh'},
        'max_physical_sends': len(pairs(schedule)) * PAIR_SENDS,
        'observed_fee_ceiling_microusd': fee,
        'fee_ceiling_is_strict_billing_limit': False,
        'auth_file': str(auth_file),
    }
    write_new(work / 'm23-contract.json', contract)
    files['m23-contract.json'] = digest(work / 'm23-contract.json')
    receipt = {'binary_path': str(binary), 'binary_sha256': digest(binary), 'files': files}
    write_new(work / 'm23-freeze.json', receipt)
    return {'status': 'prepared_not_authorized', 'contract': contract,
            'freeze_sha256': digest(work / 'm23-freeze.json')}


def verify(work):
    frozen = json.loads(local_file(work, 'm23-freeze.json').read_text())
    authorization = json.loads(local_file(work, 'm23-authorization.json').read_text())
    if (authorization.get('status') != 'approved'
            or authorization.get('freeze_sha256') != digest(work / 'm23-freeze.json')):
        raise ValueError('scoped authorization does not match the frozen campaign')
    for name, expected in frozen['files'].items():
        if digest(local_file(work, name)) != expected:
            raise ValueError('frozen input changed')
    contract = json.loads(local_file(work, 'm23-contract.json').read_text())
    required = {'cases.json', 'rates.json', 'run_m23_eval.py', 'm23-contract.json'}
    cases = json.loads(local_file(work, 'cases.json').read_text())['cases']
    required.update(case['path'] for case in cases)
    for case in cases:
        case_project(work, case)
    if not required <= frozen['files'].keys():
        raise ValueError('frozen inputs missing')
    if frozen['files']['run_m23_eval.py'] != digest(Path(__file__).resolve()):
        raise ValueError('executing launcher is not the frozen version')
    fee = integer(contract['observed_fee_ceiling_microusd'], 1)
    if (contract.get('version') != 1 or fee > MAX_FEE
            or contract['pairs'] != pairs(contract['schedule'])
            or contract['max_physical_sends'] != len(contract['pairs']) * PAIR_SENDS
            or contract['root'] != {'model': 'gpt-6-astra', 'effort': 'xhigh'}
            or contract['child'] != {'model': 'gpt-5.6-luna', 'effort': 'xhigh'}):
        raise ValueError('campaign contract mismatch')
    binary = Path(frozen['binary_path'])
    if (not binary.is_absolute() or binary.resolve(strict=True) != binary
            or digest(binary) != frozen['binary_sha256']):
        raise ValueError('binary changed')
    if os.environ.get('POLARIS_AUTH_READ_ONLY') != '1':
        raise ValueError('common-runner readonly authentication required')
    return contract, binary


def checked_totals(work, contract, index):
    sends = fee = 0
    for pair in contract['pairs'][:index]:
        checked = json.loads(local_file(work, f"pair-{pair['index']:02}.checked.json").read_text())
        if checked.get('pair') != pair or checked.get('state') != 'complete':
            raise ValueError('previous pair incomplete or mismatched')
        for name, expected in checked['files'].items():
            if digest(local_file(work, name)) != expected:
                raise ValueError('previous pair evidence changed')
        actual = check_pair(work, pair, work / f"pair-{pair['index']:02}")
        for field, value in actual.items():
            if checked.get(field) != value:
                raise ValueError('previous pair receipt mismatch')
        sends += actual['physical_sends']
        fee += actual['observed_cost_upper_microusd']
    if sends > index * PAIR_SENDS or fee >= contract['observed_fee_ceiling_microusd']:
        raise ValueError('campaign observed limit reached')
    return sends, fee


def check_pair(work, pair, output):
    complete = json.loads(local_file(work, str(output.relative_to(work) / 'complete.json')).read_text())
    if (complete.get('state') != 'complete' or complete.get('trials') != 2
            or complete.get('price_table_sha256') != digest(work / 'rates.json')):
        raise ValueError('pair did not complete under the frozen rates')
    sends = fee = 0
    reports = []
    for arm in pair['arms']:
        name = f"{pair['case']}-r{pair['repeat']}-{arm}.result.json"
        path = local_file(work, str(output.relative_to(work) / name))
        report = json.loads(path.read_text())
        if report.get('observation_complete') is not True:
            raise ValueError('incomplete physical measurement')
        count = integer(report['physical_sends'], 1)
        if count > (5 if arm == 'direct' else 10):
            raise ValueError('arm physical limit exceeded')
        sends += count
        fee += integer(report['accounting']['upper_cost_microusd'])
        reports.append(report)
    if (sends != complete.get('physical_sends') or sends > PAIR_SENDS
            or fee != complete.get('observed_cost_upper_microusd')):
        raise ValueError('pair aggregate mismatch')
    # Retain every evidence file, not just the success marker. Changed metrics,
    # results, or ledger stop the next pair even if complete.json is unchanged.
    files = {str(path.relative_to(work)): digest(local_file(work, str(path.relative_to(work))))
             for path in output.iterdir() if path.is_file()}
    if not {'attempts.json', 'metrics.jsonl', 'started.json', 'plan.json'} <= {Path(p).name for p in files}:
        raise ValueError('pair evidence missing')
    return {'state': 'complete', 'pair': pair, 'physical_sends': sends,
            'observed_cost_upper_microusd': fee, 'files': files,
            'measurement_complete': all(report.get('measurement_complete') is True for report in reports),
            'scores': [report['score'] for report in reports]}


def run_pair(work, index):
    contract, binary = verify(work)
    integer(index)
    if index >= len(contract['pairs']) or (work / 'm23-stopped.json').exists():
        raise ValueError('unknown pair or campaign stopped')
    before_sends, before_fee = checked_totals(work, contract, index)
    for pending in contract['pairs'][index:]:
        prefix = f"pair-{pending['index']:02}"
        if any((work / name).exists() for name in (prefix, prefix + '.claim.json', prefix + '.checked.json')):
            raise ValueError('unexpected started pair; no retry or reordering')
    pair = contract['pairs'][index]
    output = work / f'pair-{index:02}'
    if output.exists():
        raise ValueError('pair output already exists; no retry')
    # Exclusive claim handles overlapping launches and survives a process crash.
    write_new(work / f'pair-{index:02}.claim.json', pair)
    remaining = contract['observed_fee_ceiling_microusd'] - before_fee
    env = os.environ.copy()
    env['POLARIS_WS'] = 'off'
    env['POLARIS_METRICS_PATH'] = str(output / 'metrics.jsonl')
    env['POLARIS_READ_OUTPUT_BYTES'] = '24576'
    for name in ('POLARIS_CACHE_NAMESPACE', 'POLARIS_CACHE_MODE', 'POLARIS_CACHE_PREFIX',
                 'POLARIS_CACHE_PACING', 'POLARIS_TURN_AFFINITY'):
        env.pop(name, None)
    command = [str(binary), '--work-dir', str(work), '--cases', str(work / 'cases.json'),
               '--output', str(output), '--schedule', contract['schedule']]
    if contract['schedule'] == 'full':
        command.extend(['--pair-index', str(index)])
    command.extend(['--live', '--auth-file', contract['auth_file'], '--fee-cap-microusd', str(remaining)])
    started = time.monotonic()
    try:
        # Native runner owns stdout/stderr; do not retain arbitrary credentials
        # or response text in an exception message.
        result = subprocess.run(command, cwd=work, env=env, timeout=PAIR_TIMEOUT, check=False)
        if result.returncode:
            raise ValueError('pair driver failed; no retry')
        checked = check_pair(work, pair, output)
        if (before_fee + checked['observed_cost_upper_microusd'] > contract['observed_fee_ceiling_microusd']
                or before_sends + checked['physical_sends'] > contract['max_physical_sends']):
            raise ValueError('campaign observed limit exceeded')
        checked['elapsed_seconds'] = time.monotonic() - started
        write_new(work / f'pair-{index:02}.checked.json', checked)
        return checked
    except (ValueError, OSError, KeyError, TypeError, subprocess.TimeoutExpired):
        write_new(work / 'm23-stopped.json', {'pair': pair, 'reason': 'pair_failed_or_incomplete',
                                           'elapsed_seconds': time.monotonic() - started})
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--work-dir', type=Path, required=True)
    commands = parser.add_subparsers(dest='command', required=True)
    prep = commands.add_parser('prepare')
    prep.add_argument('--binary', type=Path, required=True)
    prep.add_argument('--auth-file', type=Path, required=True)
    prep.add_argument('--schedule', choices=('pilot', 'full'), default='pilot')
    prep.add_argument('--fee-cap-microusd', type=int, required=True)
    run = commands.add_parser('run-pair')
    run.add_argument('index', type=int)
    args = parser.parse_args()
    work = args.work_dir.resolve(strict=True)
    try:
        result = (prepare(work, args.binary.resolve(strict=True), args.auth_file,
                          args.schedule, args.fee_cap_microusd)
                  if args.command == 'prepare' else run_pair(work, args.index))
        print(json.dumps(result, ensure_ascii=False))
    except (ValueError, OSError, KeyError, TypeError, subprocess.TimeoutExpired) as error:
        # Paths and raw exception details can contain private input.
        parser.exit(1, f'M23 campaign stopped ({type(error).__name__}); inspect local evidence.\n')


if __name__ == '__main__':
    main()
