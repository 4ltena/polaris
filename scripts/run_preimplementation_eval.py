#!/usr/bin/env python3
"""Run frozen two-turn planning trials inside the common benchmark runner."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import subprocess
import time

ARMS = ('candidate-features-off', 'candidate-workflow-only')
REQUIRED_FILES = frozenset({'execution-contract.json', 'benchmark_preimplementation.py',
                            'run_preimplementation_eval.py', 'pricing.json', 'source-receipt.json',
                            'fixtures/preimplementation-cases.json',
                            'fixtures/preimplementation-receipt.json'}
                           | {'inputs/PRE%02d.json' % n for n in range(1, 9)})


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_new(path, value):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w') as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())


def trial_plan():
    trials = []
    for repeat in range(1, 4):
        for number in range(1, 9):
            case = 'PRE%02d' % number
            order = ARMS if (repeat + number) % 2 == 0 else ARMS[::-1]
            for arm in order:
                trials.append({'id': 'r%d-%s-%s' % (repeat, case, arm),
                               'repeat': repeat, 'case': case, 'arm': arm})
    return trials


def verify(work, binary):
    receipt = json.loads((work / 'freeze.json').read_text())
    contract = json.loads((work / 'execution-contract.json').read_text())
    resuming = contract.get('continuation') == 'remaining_after_pre01_timeout'
    expected_plan = trial_plan()[2:] if resuming else trial_plan()
    cap = 92 if resuming else 96
    if (contract.get('status') != 'approved' or contract.get('model') != 'gpt-6-astra'
            or contract.get('effort') != 'medium' or contract.get('max_physical_requests') != cap
            or contract.get('trials') != expected_plan):
        raise ValueError('approved scope mismatch')
    if str(binary) != receipt['binary_path'] or digest(binary) != receipt['binary_sha256']:
        raise ValueError('binary receipt mismatch')
    if not REQUIRED_FILES <= set(receipt['files']):
        raise ValueError('required frozen inputs missing')
    sources = json.loads((work / 'source-receipt.json').read_text())
    if not sources or any(receipt['files'].get('source/' + name) != expected
                          for name, expected in sources.items()):
        raise ValueError('source receipt incomplete')
    for name, expected in receipt['files'].items():
        path = work / name
        if not path.is_relative_to(work) or path.resolve() != path or digest(path) != expected:
            raise ValueError('frozen input mismatch')
    if resuming:
        required = {'resume-evidence/attempts.json', 'resume-evidence/evaluation.stopped.json',
                    'prior.turn-1.result.json', 'prior.turn-2.result.json'}
        if not required <= set(receipt['files']):
            raise ValueError('continuation evidence missing')
        prior = json.loads((work / 'resume-evidence/attempts.json').read_text())['records']
        stopped = json.loads((work / 'resume-evidence/evaluation.stopped.json').read_text())
        if (len(prior) != 3 or [r['status'] for r in prior] != ['succeeded', 'succeeded', 'cancelled']
                or [r['parent_id'] for r in prior] != [trial_plan()[0]['id']] * 2 + [trial_plan()[1]['id']]
                or any(r['model'] != 'gpt-6-astra' or r['retry_reason'] is not None for r in prior)
                or stopped.get('trial') != trial_plan()[1]['id']
                or prior[2]['usage']['state'] != 'missing'):
            raise ValueError('unexpected prior attempts')
        cost = 0.0
        for n, record in enumerate(prior[:2], 1):
            result = json.loads((work / ('prior.turn-%d.result.json' % n)).read_text())
            u = record['usage']
            if u['state'] != 'known' or not result['measurement_complete']:
                raise ValueError('prior complete result missing')
            for key in ('input_tokens', 'output_tokens', 'total_tokens'):
                if result['usage'][key] != u[key]:
                    raise ValueError('prior usage mismatch')
            if result['usage']['cache_read_tokens'] != u['cached_tokens']:
                raise ValueError('prior cache mismatch')
            cost += result['observed_cost_upper_usd']
        if not math.isfinite(cost) or cost < 0 or contract.get('prior_known_cost_upper_usd') != cost:
            raise ValueError('prior cost mismatch')
    elif contract.get('prior_known_cost_upper_usd', 0) != 0 or contract.get('continuation') is not None:
        raise ValueError('unexpected continuation')
    if os.environ.get('POLARIS_AUTH_READ_ONLY') != '1':
        raise ValueError('readonly authentication required')
    return contract


def ledger_status(work, cap=96, initial_cost=0.0):
    path = work / 'attempts.json'
    if not path.exists():
        if initial_cost > 40:
            raise ValueError('observed cumulative limit')
        return 0, initial_cost
    ledger = json.loads(path.read_text())
    if ledger['budget'] != {'max_physical_attempts': cap} or ledger['halted']:
        raise ValueError('ledger halted or budget mismatch')
    records = ledger['records']
    cost = initial_cost
    for record in records:
        u = record['usage']
        if (record['status'] != 'succeeded' or record['model'] != 'gpt-6-astra'
                or record['retry_reason'] is not None or u.get('state') != 'known'):
            raise ValueError('unfinished or invalid attempt; no retry')
        values = [u[k] for k in ('input_tokens', 'output_tokens', 'total_tokens', 'cached_tokens')]
        if any(type(v) is not int or v < 0 for v in values):
            raise ValueError('invalid usage')
        i, o, total, cache = values
        if i + o != total or cache > i or o > 4096:
            raise ValueError('usage or observed output limit')
        cost += ((i - cache) * 12.5 + cache) * (2 if i > 272000 else 1) / 1e6
        cost += o * 50 * (1.5 if i > 272000 else 1) / 1e6
    if len(records) > cap or not math.isfinite(cost) or cost > 40:
        raise ValueError('observed cumulative limit')
    return len(records), cost


def run_trial(binary, work, trial_id):
    contract = verify(work, binary)
    trials = contract['trials']
    matches = [t for t in trials if t['id'] == trial_id]
    if len(matches) != 1:
        raise ValueError('unknown trial')
    trial = matches[0]
    index = trials.index(trial)
    # Never replay an existing claim or skip over an incomplete predecessor.
    if (work / 'evaluation.stopped.json').exists():
        raise ValueError('evaluation already stopped')
    for previous in trials[:index]:
        result = json.loads((work / (previous['id'] + '.checked.json')).read_text())
        if not result.get('measurement_complete'):
            raise ValueError('previous trial incomplete')
    cap = contract['max_physical_requests']
    prior_cost = contract.get('prior_known_cost_upper_usd', 0.0)
    before, _ = ledger_status(work, cap, prior_cost)
    if before != index * 2:
        raise ValueError('ledger count does not match trial position')
    write_new(work / (trial_id + '.launcher-claim.json'), trial)
    env = os.environ.copy()
    env['POLARIS_METRICS_PATH'] = str(work / (trial_id + '.metrics.jsonl'))
    env['POLARIS_PREIMPLEMENTATION_WORK'] = str(work)
    env['POLARIS_EVAL_MAX_REQUESTS'] = str(cap)
    for name in ('POLARIS_CACHE_NAMESPACE', 'POLARIS_CACHE_MODE', 'POLARIS_CACHE_PREFIX',
                 'POLARIS_CACHE_PACING', 'POLARIS_TURN_AFFINITY'):
        env.pop(name, None)
    started = time.monotonic()
    try:
        result = subprocess.run([str(binary), '--trial-id', trial_id, '--case-path',
                                 str(work / 'inputs' / (trial['case'] + '.json')), '--arm',
                                 'baseline' if trial['arm'] == ARMS[0] else 'workflow'],
                                cwd=work, env=env, capture_output=True, text=True, timeout=240)
        (work / (trial_id + '.stdout')).write_text(result.stdout)
        (work / (trial_id + '.stderr')).write_text(result.stderr)
        if result.returncode:
            raise ValueError('driver failed; no retry')
        after, cost = ledger_status(work, cap, prior_cost)
        if after - before != 2:
            raise ValueError('physical attempt mismatch')
        report = json.loads((work / (trial_id + '.result.json')).read_text())
        if report.get('measurement_complete') is not True or report.get('physical_sends') != 2:
            raise ValueError('incomplete trial report')
        # Objective checks do not stop later cases and never imply semantic quality.
        import importlib.util
        spec = importlib.util.spec_from_file_location('frozen_checker', work / 'benchmark_preimplementation.py')
        checker = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(checker)
        turns = report['turns']
        if len(turns) != 2:
            raise ValueError('turn count mismatch')
        checks = [checker.validate_answer(trial['case'], n, turn['answer'])
                  for n, turn in enumerate(turns, 1)]
        checked = {**trial, 'measurement_complete': True, 'physical_sends': 2,
                   'elapsed_seconds': time.monotonic() - started, 'objective_checks': checks,
                   'semantic_review_required': True, 'cumulative_cost_upper_usd': cost}
        write_new(work / (trial_id + '.checked.json'), checked)
        print(json.dumps(checked, ensure_ascii=False))
    except (ValueError, OSError, KeyError, subprocess.TimeoutExpired):
        write_new(work / 'evaluation.stopped.json', {'trial': trial_id, 'reason': 'trial_incomplete_or_limit',
                                                   'elapsed_seconds': time.monotonic() - started})
        raise


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--work', type=Path, required=True)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--trial', required=True)
    args = parser.parse_args()
    if args.work.resolve() != args.work or args.binary.resolve() != args.binary:
        raise ValueError('canonical absolute paths required')
    os.environ['POLARIS_AUTH_READ_ONLY'] = '1'
    os.environ['POLARIS_DATA_DIR'] = str(args.work / 'data')
    run_trial(args.binary, args.work, args.trial)


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, KeyError, subprocess.TimeoutExpired) as error:
        print('実装前試験を停止: ' + type(error).__name__)
        raise SystemExit(1) from None
