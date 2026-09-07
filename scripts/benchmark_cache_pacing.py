"""モデル要求の間隔を、保存済みの非機密metricsから検証する。"""
import statistics


def summarize_pacing(traces, mode):
    if mode not in ('off', 'on') or not traces:
        raise ValueError('invalid pacing condition or empty trace')
    interval = 5000 if mode == 'on' else 0
    offsets, waits, wall = [], [], []
    for trace in traces:
        pacing = trace.get('cache_pacing')
        if not isinstance(pacing, dict) or pacing.get('mode') != mode:
            raise ValueError('pacing mode mismatch')
        for key in ('interval_ms', 'wait_ms', 'dispatch_offset_ms'):
            if type(pacing.get(key)) is not int or pacing[key] < 0:
                raise ValueError('missing or invalid pacing measurement')
        if pacing['interval_ms'] != interval or (mode == 'off' and pacing['wait_ms'] != 0):
            raise ValueError('pacing interval mismatch')
        stamp = trace.get('started_unix_ms')
        if type(stamp) is not int or stamp < 0:
            raise ValueError('missing wall-clock start')
        offsets.append(pacing['dispatch_offset_ms'])
        waits.append(pacing['wait_ms'])
        wall.append(stamp)
    if waits[0] != 0:
        raise ValueError('first request was delayed')
    # この集計器は逐次実測の1プロセス・1providerだけを受け取る。
    gaps = [b - a for a, b in zip(offsets, offsets[1:])]
    if any(gap < interval for gap in gaps):
        raise ValueError('dispatch spacing violated or rows out of order')
    first, maximum = 0, 0
    for last, stamp in enumerate(offsets):
        while stamp - offsets[first] >= 60000:
            first += 1
        maximum = max(maximum, last - first + 1)
    if mode == 'on' and maximum > 12:
        raise ValueError('more than 12 starts in a half-open 60s window')
    return {'requests': len(traces), 'mode': mode, 'interval_ms': interval,
            'total_wait_ms': sum(waits), 'waited_requests': sum(wait > 0 for wait in waits),
            'min_dispatch_gap_ms': min(gaps) if gaps else None,
            'median_dispatch_gap_ms': statistics.median(gaps) if gaps else None,
            'max_starts_in_60s': maximum,
            'wall_clock_min_gap_ms': min(b-a for a,b in zip(wall,wall[1:])) if gaps else None,
            'input_ge_1024_requests': sum(t['usage']['input_tokens'] >= 1024 for t in traces)}
