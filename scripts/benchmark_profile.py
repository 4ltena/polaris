"""Explicit opt-in comparison settings and observations; never loads secrets.

This profile is an experimental arm, not a change to product defaults. Decision
counts cover completed logical calls only; retries and cancelled physical calls
are not accounted for, so a complete campaign price must remain unknown.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path

PREFIX = "polaris-benchmark: "
TIERS = {
    "dwarf": {"model": "gpt-5.6-luna", "effort": "xhigh"},
    "subgiant": {"model": "gpt-5.6-terra", "effort": "medium"},
    "giant": {"model": "gpt-5.6-terra", "effort": "xhigh"},
    "supergiant": {"model": "gpt-5.6-sol", "effort": "xhigh"},
}


class Profile:
    def __init__(self, history: str, embedding: dict | None = None, phase_bodies: bool = True):
        if history not in ("legacy", "strict10"):
            raise ValueError("explicit legacy or strict10 history is required")
        if history == "strict10":
            if not isinstance(embedding, dict) or set(embedding) != {"runtime", "model_path", "revision"}:
                raise ValueError("strict10 requires exactly runtime/model_path/revision pins")
            if not all(isinstance(v, str) and v.strip() for v in embedding.values()):
                raise ValueError("embedding pins must be nonempty strings")
            if (not Path(embedding["runtime"]).is_absolute()
                    or not Path(embedding["runtime"]).is_file()
                    or not Path(embedding["model_path"]).is_absolute()
                    or not Path(embedding["model_path"]).is_dir()):
                raise ValueError("embedding pins must point to existing absolute local resources")
        elif embedding is not None:
            raise ValueError("legacy history does not consume embedding pins")
        self.history = history
        self.embedding = dict(embedding) if embedding is not None else None
        self.phase_bodies = phase_bodies

    def expected(self) -> dict:
        return {"root_model": "gpt-6-astra", "root_effort": "medium",
                "decision": {"enabled": True, "active": True, "model": "jev-latest"},
                "tiers": {name: dict(value) for name, value in TIERS.items()},
                "history_mode": self.history, "tool_memory": "retrieval",
                "workflow": {"enabled": True, "mode": "manual"}}

    def toml(self) -> str:
        # `always` and `phase_skills` are left out on purpose: omitting them
        # keeps the product's own defaults (config.rs: `builtin:workflow-core`
        # always, `builtin:<phase>` per phase). Writing them empty, as this
        # did until 2026-09-20, measured a polaris whose phase guidance had
        # been removed — including the implement phase's instruction about
        # what to delegate, which is the text under test.
        lines = [f'history_mode = "{self.history}"', 'input_preparation = "off"',
                 '', '[files_md]', 'auto_regenerate = false',
                 '', '[decision]', 'enabled = true',
                 'base_url = "https://api.typesafe.ai/v1"', 'model = "jev-latest"',
                 '', '[workflow]', 'enabled = true', 'mode = "manual"',
                 'initial_phase = "general"']
        if self.phase_bodies is False:
            # The phase machinery stays on — it is what classifies the turn
            # and decides which tools the turn may offer — but its prose
            # does not ride along. 227 tokens on every request of a review
            # turn, measured 2026-09-21, against a 637-token always-on.
            lines += ['always = []', 'phase_skills = {}']
        for tier, settings in TIERS.items():
            lines += ['', f'[tier.{tier}]']
            lines += [f'{k} = {json.dumps(v)}' for k, v in settings.items()]
        if self.embedding is not None:
            lines += ['', '[embedding]']
            lines += [f'{k} = {json.dumps(v)}' for k, v in self.embedding.items()]
        return '\n'.join(lines) + '\n'

    def polaris_args(self) -> list[str]:
        return ['--model', 'gpt-6-astra', '--effort', 'medium',
                '--tool-memory', 'retrieval', '--history-mode', self.history,
                '--workflow-mode', 'manual']

    def codex_args(self) -> list[str]:
        return ['--model', 'gpt-6-astra', '-c', 'model_reasoning_effort="medium"']

    def preflight(self, env: dict) -> None:
        if not env.get('POLARIS_JEV_API_KEY', '').strip():
            raise ValueError('jev-routed requires nonempty POLARIS_JEV_API_KEY before dispatch')

    def manifest(self) -> dict:
        config = self.toml()
        return {"name": "jev-routed", "root_comparison": "matched model and effort",
                "expected": self.expected(), "config_toml": config,
                "config_sha256": hashlib.sha256(config.encode()).hexdigest(),
                "total_usd": None,
                "cost_scope": "provider subtotal; physical Jev attempts and rates incomplete",
                "excluded_paths": ["MCP without a server catalog", "RAG when history is legacy"],
                "product_defaults_changed": False}

    def observe(self, stderr: str) -> dict:
        try:
            events = [json.loads(line[len(PREFIX):]) for line in stderr.splitlines()
                      if line.startswith(PREFIX)]
        except (ValueError, TypeError) as e:
            raise ValueError('malformed benchmark observation') from e
        if (len(events) != 2 or any(not isinstance(e, dict) or e.get('schema_version') != 1
                                    for e in events)
                or [e.get('event') for e in events] != ['configured', 'completed']):
            raise ValueError('one configured/completed observation pair is required')
        configured, completed = events
        for name, expected in self.expected().items():
            if configured.get(name) != expected:
                raise ValueError(f'effective {name} differs from the frozen profile')
        if completed.get('succeeded') is not True:
            raise ValueError('the observed run did not complete')
        decision, coverage = completed.get('decision'), completed.get('coverage')
        for name, counts in (('decision', decision), ('coverage', coverage)):
            if (not isinstance(counts, dict)
                    or any(type(counts.get(k)) is not int or counts[k] < 0
                           for k in ('reported', 'missing', 'failed'))):
                raise ValueError(f'incomplete {name} observation')
            if counts['missing'] or counts['failed']:
                raise ValueError(f'{name} has missing or failed requests')
        scope = decision.get('accounting_scope')
        if (scope not in ('completed_logical_decisions', 'settled_typed_adapter_observations')
                or decision.get('physical_attempts_known') is not False
                or any(type(decision.get(k)) is not int or decision[k] < 0
                       for k in ('input_tokens', 'output_tokens'))):
            raise ValueError('unknown decision accounting contract')
        for name in ('abandoned_stages', 'observer_failures'):
            # Legacy receipts omitted these fields. New observations must state
            # them; an unsettled stage cannot be read as a zero-consumption run.
            value = decision.get(name, 0 if scope == 'completed_logical_decisions' else None)
            if type(value) is not int or value != 0:
                raise ValueError('decision has unsettled stages or failed observation writes')
        return {'configured': configured, 'completed': completed,
                'decision_state': 'observed' if decision['reported'] else 'not_exercised',
                'all_classifier_paths_verified': False, 'total_usd': None}

    def observe_codex(self, paths: list[Path], thread: str, previous_requests: int) -> dict:
        """Bind observations to the root session, leaving child models independent."""
        from benchmark_usage_audit import codex_requests

        roots = []
        for path in paths:
            for line in path.read_text(encoding='utf-8').splitlines():
                try:
                    event = json.loads(line)
                except ValueError:
                    continue  # The common parser retains malformed accounting rows.
                if (isinstance(event, dict) and event.get('type') == 'session_meta'
                        and isinstance(event.get('payload'), dict)
                        and event['payload'].get('id') == thread):
                    roots.append(path)
                    break
        if len(roots) != 1:
            raise ValueError('exactly one root rollout must match the observed Codex thread')
        rows, _, errors = codex_requests(roots)
        if errors or len(rows) <= previous_requests:
            raise ValueError('fresh complete root usage observations are required after each turn')
        for row in rows:
            if row.get('model') != 'gpt-6-astra' or row.get('effort') != 'medium':
                raise ValueError('effective Codex root model/effort differs from the frozen profile')
        return {'root_model': 'gpt-6-astra', 'root_effort': 'medium',
                'observed_requests': len(rows), 'rollout': str(roots[0])}


def verify_binary(path: Path, expected_sha256: str | None) -> str:
    if (not expected_sha256 or len(expected_sha256) != 64
            or any(c not in '0123456789abcdef' for c in expected_sha256)):
        raise ValueError('live jev-routed requires an explicit --binary-sha256')
    if not path.is_file() or not path.is_absolute():
        raise ValueError('live jev-routed requires an existing absolute binary path')
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    if digest != expected_sha256:
        raise ValueError('binary differs from the frozen --binary-sha256')
    return digest


def claim_attempt(out: Path, stem: str) -> None:
    """A failed attempt retains its evidence and cannot be silently retried."""
    for suffix in ('data', 'metrics.jsonl', 'claim'):
        if (out / f'{stem}.{suffix}').exists():
            raise FileExistsError(f'previous attempt exists: {stem}; choose a new output directory')
    if list(out.glob(f'{stem}.turn*')):
        raise FileExistsError(f'previous turn evidence exists: {stem}')
    with (out / f'{stem}.claim').open('x', encoding='utf-8') as stream:
        stream.write('one attempt; no automatic resend\n')
