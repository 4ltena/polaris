"""Bounded offline native-worker smoke; synthetic input only, no network."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import resource
import select
import subprocess
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--worker', type=Path, required=True)
parser.add_argument('--model', type=Path, required=True, help='Verified Clef-Flash-Q4_K_M.gguf')
args = parser.parse_args()
WORKER = args.worker.resolve(strict=True)
MODEL = args.model.resolve(strict=True)
DIGEST = '3626116623ca8d89641952471fac9ce12af84fb3517520029a2ac6f5146868a2'
OWNER = 'native-acceptance'
GENERATION = 'flash-q4-cpu-context1024'
MAX = 1024 * 1024
for directory in ['worker-home', 'worker-tmp']:
    Path(directory).mkdir(mode=0o700, exist_ok=True)
ENV = {'PATH': '/usr/bin:/bin', 'HOME': str(Path.cwd() / 'worker-home'),
       'TMPDIR': str(Path.cwd() / 'worker-tmp'), 'OMP_NUM_THREADS': '4'}

ERROR_LOG = open("native-worker.stderr", "wb")

def spawn():
    return subprocess.Popen([str(WORKER)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=ERROR_LOG, env=ENV, cwd=Path.cwd(), bufsize=0)

def send(p, value):
    body = json.dumps(value, ensure_ascii=False, allow_nan=False).encode() + b'\n'
    assert len(body) <= MAX + 1
    p.stdin.write(body)
    p.stdin.flush()

def receive(p, seconds):
    deadline = time.monotonic() + seconds
    data = bytearray()
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([p.stdout], [], [], remaining)[0]:
            raise TimeoutError('worker frame deadline')
        b = os.read(p.stdout.fileno(), 1)
        if not b:
            raise RuntimeError('worker ended before complete frame; exit=' + str(p.wait(timeout=10)))
        if b == b'\n':
            return json.loads(data)
        data += b
        assert len(data) <= MAX

def finish(p):
    if p.poll() is None:
        p.kill()
    p.wait(timeout=10)

# No valid model is given in these cases: malformed frames must not load one.
refusals = []
for name, body in [
    ('missing_fields', b'{}\n'),
    ('duplicate_key', b'{"protocol":1,"protocol":1}\n'),
    ('partial_frame', b'{"protocol":1}'),
    ('depth_overflow', b'{"x":' + b'[' * 40 + b'0' + b']' * 40 + b'}\n'),
    ('frame_overflow', b' ' * (MAX + 1) + b'\n'),
]:
    p = spawn()
    try:
        output, _ = p.communicate(body, timeout=10)
        assert p.returncode == 2 and output == b'{"protocol":1,"status":"invalid_or_unavailable"}\n', name
        refusals.append(name)
    finally:
        finish(p)

asked = {
    'is/billing?': {'type': 'noul', 'instructions': 'Is this a billing or refund problem?'},
    'route/担当': {'type': 'choice', 'instructions': 'Which team should handle this?', 'criteria': {
        'billing/請求': 'payments, duplicate charges, refunds',
        'shipping/配送': 'parcels, delivery, tracking',
        'engineering/開発': 'software bugs and code changes'}},
    'urgency': {'type': 'score', 'instructions': 'How urgent is the request?',
                'criteria': ['can wait', 'today', 'right now']},
}
states = [
    ('I was charged twice for one order. This is a billing problem. Please refund the duplicate charge right now.', 'billing/請求'),
    ('My package is missing and its tracking has stopped. This is a delivery problem, not a billing or refund request.', 'shipping/配送'),
]

def checked(decision):
    assert set(decision['answers']) == set(asked)
    assert decision['usage']['input_tokens'] > 0 and decision['usage']['output_tokens'] == 0
    for key, question in asked.items():
        answer = decision['answers'][key]
        assert answer['type'] == question['type']
        if question['type'] == 'noul':
            assert math.isfinite(answer['noul']) and 0 <= answer['noul'] <= 1
        else:
            probabilities = answer['probabilities']
            expected = set(question['criteria']) if question['type'] == 'choice' else {'0', '1', '2'}
            assert set(probabilities) == expected
            assert all(math.isfinite(x) and 0 <= x <= 1 for x in probabilities.values())
            assert abs(sum(probabilities.values()) - 1) < 0.001
            if question['type'] == 'choice':
                assert answer['choice'] in expected
            else:
                assert math.isfinite(answer['score']) and 0 <= answer['score'] <= 2
                assert abs(answer['score'] - sum(int(k) * v for k, v in probabilities.items())) < 0.001

p = spawn()
started = time.monotonic()
try:
    send(p, {'protocol': 1, 'owner': OWNER, 'generation': GENERATION, 'model': str(MODEL),
             'model_bytes': 6486448288, 'model_sha256': DIGEST, 'context_tokens': 1024,
             'device': 'cpu', 'request_timeout_ms': 120000})
    ready = receive(p, 120)
    assert ready['status'] == 'ready' and ready['model_sha256'] == DIGEST
    assert ready['runtime_revision'] == '620d2225cc436ab0b001e3200e590e3b286ef9ab+polaris-fileptr-v1'
    assert ready['owner'] == OWNER and ready['generation'] == GENERATION and ready['device'] == 'cpu'
    load_seconds = time.monotonic() - started
    observations = []
    for number, (state, expected) in enumerate([states[0], states[1], states[0]], 1):
        began = time.monotonic()
        send(p, {'protocol': 1, 'owner': OWNER, 'generation': GENERATION, 'request_id': number,
                 'op': 'decide', 'state': state, 'questions': asked})
        reply = receive(p, 125)
        assert reply['status'] == 'ok', reply['status']
        assert reply['owner'] == OWNER and reply['generation'] == GENERATION and reply['request_id'] == number
        checked(reply['decision'])
        assert reply['decision']['answers']['route/担当']['choice'] == expected
        observations.append({'request_id': number, 'elapsed_seconds': round(time.monotonic() - began, 3),
                             'decision': reply['decision']})
        print(json.dumps({'stage': 'answered', 'request_id': number}), flush=True)
    assert observations[0]['decision']['answers']['is/billing?']['noul'] > observations[1]['decision']['answers']['is/billing?']['noul']
    assert abs(observations[0]['decision']['answers']['is/billing?']['noul'] - observations[2]['decision']['answers']['is/billing?']['noul']) < 0.001
    send(p, {'protocol': 1, 'owner': OWNER, 'generation': GENERATION, 'request_id': 4, 'op': 'shutdown'})
    closed = receive(p, 10)
    assert closed['status'] == 'closed' and closed['request_id'] == 4
    p.stdin.close()
    assert p.wait(timeout=10) == 0
    assert p.stdout.read() == b''
    result = {'status': 'passed', 'device': 'cpu', 'model_sha256': DIGEST,
              'worker_sha256': hashlib.sha256(WORKER.read_bytes()).hexdigest(),
              'model_loads': 1, 'requests': 3, 'question_answers': 9, 'protocol_refusals': refusals,
              'cold_load_seconds': round(load_seconds, 3), 'observations': observations,
              'children_maxrss_bytes_macos': resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss,
              'limits': 'Synthetic smoke only; no GPU proof, accuracy benchmark or probability calibration; not activation approval.'}
    Path('native-worker-result.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({k: v for k, v in result.items() if k != 'observations'}), flush=True)
finally:
    finish(p)
