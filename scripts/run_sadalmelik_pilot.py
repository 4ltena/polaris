#!/usr/bin/env python3
"""Launch the approved two-send canary; only the common runner executes this."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import subprocess
import time

ARMS = ('candidate-features-off', 'candidate-workflow-only')


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(chunk)
    return value.hexdigest()


def write_new(path, value):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, 'w') as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())


def verify(binary, work):
    receipt = json.loads((work / 'freeze.json').read_text())
    proposal = json.loads((work / 'pilot-proposal.json').read_text())
    if proposal['status'] != 'approved' or proposal['model'] != 'gpt-6-astra' or proposal['reasoning_effort'] != 'medium':
        raise ValueError('approved model contract missing')
    if proposal['max_physical_sends_total'] != 2 or proposal['arms'] != list(ARMS):
        raise ValueError('approved request count changed')
    if str(binary) != receipt['binary_path'] or digest(binary) != receipt['binary_sha256']:
        raise ValueError('binary receipt mismatch')
    for name, expected in receipt['files'].items():
        path = work / name
        if path.resolve() != path or not path.is_file() or digest(path) != expected:
            raise ValueError('frozen input mismatch')
    if os.environ.get('POLARIS_AUTH_READ_ONLY') != '1':
        raise ValueError('common runner readonly authentication required')


def run_batch(binary, work):
    # This claim is never cleared, including on timeout, crash, or missing usage.
    write_new(work / 'pilot.started.json', {'state': 'claimed', 'max_physical_sends': 2})
    started = time.monotonic()
    records = []
    observed_upper_usd = 0.0
    for arm in ARMS:
        remaining = min(120.0, 238.0 - (time.monotonic() - started))
        if remaining <= 0:
            raise ValueError('batch timeout')
        env = os.environ.copy()
        env['POLARIS_METRICS_PATH'] = str(work / (arm + '.metrics.jsonl'))
        # Freeze optional transport experiments; retain all managed TLS/proxy env.
        for name in ('POLARIS_CACHE_NAMESPACE', 'POLARIS_CACHE_MODE', 'POLARIS_CACHE_PREFIX', 'POLARIS_CACHE_PACING', 'POLARIS_TURN_AFFINITY'):
            env.pop(name, None)
        try:
            result = subprocess.run([str(binary), str(work), arm], env=env, cwd=work,
                                    capture_output=True, text=True, timeout=remaining)
        except subprocess.TimeoutExpired:
            write_new(work / 'pilot.stopped.json', {'arm': arm, 'reason': 'timeout'})
            raise ValueError('pilot timeout; no retry') from None
        # Driver errors are sanitized and never print provider error bodies.
        (work / (arm + '.stdout')).write_text(result.stdout)
        (work / (arm + '.stderr')).write_text(result.stderr)
        if result.returncode:
            write_new(work / 'pilot.stopped.json', {'arm': arm, 'reason': 'driver_failed', 'exit_code': result.returncode})
            raise ValueError('pilot stopped; inspect sanitized driver output')
        record = json.loads((work / (arm + '.result.json')).read_text())
        if record.get('pass') is not True or record.get('physical_sends') != 1:
            raise ValueError('pilot result incomplete; no next arm')
        interval = record.get('api_equivalent_usd_interval')
        if not isinstance(interval, list) or len(interval) != 2 or not all(isinstance(v, (int, float)) and math.isfinite(v) and v >= 0 for v in interval):
            raise ValueError('cost observation missing; no next arm')
        observed_upper_usd += interval[1]
        if observed_upper_usd > 40.0:
            write_new(work / 'pilot.stopped.json', {'arm': arm, 'reason': 'observed_cumulative_cost_limit', 'upper_usd': observed_upper_usd})
            raise ValueError('observed cumulative cost exceeded; no next arm')
        records.append(record)
    summary = {'kind': 'live_connectivity_canary', 'model': 'gpt-6-astra', 'effort': 'medium',
               'completed_arms': len(records), 'physical_sends': sum(r['physical_sends'] for r in records),
               'elapsed_seconds': time.monotonic() - started, 'records': records,
               'efficiency_comparison': False}
    write_new(work / 'summary.json', summary)
    print(json.dumps(summary, ensure_ascii=False))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--work', type=Path, required=True)
    args = parser.parse_args()
    if args.binary.resolve() != args.binary or args.work.resolve() != args.work:
        raise ValueError('absolute canonical paths required')
    # No tool broker is needed for this tool-free pilot. Its auth environment
    # is therefore not installed by Broker.environment; tighten it here too.
    os.environ['POLARIS_AUTH_READ_ONLY'] = '1'
    os.environ['POLARIS_DATA_DIR'] = str(args.work / 'data')
    verify(args.binary, args.work)
    run_batch(args.binary, args.work)


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, KeyError) as error:
        # Avoid paths, environment, provider response bodies and credentials.
        print('予備測定を停止: ' + type(error).__name__)
        raise SystemExit(1) from None
