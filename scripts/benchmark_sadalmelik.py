#!/usr/bin/env python3
"""Freeze and fail-closed preflight for the Sadalmelik M0 benchmark.

This driver deliberately has no provider client.  It writes a frozen trial plan
and accepts a capability receipt produced by the integrated provider.  A run is
eligible only when that receipt proves that every physical send is bounded and
reserved before it is sent.  In particular, an absent receipt is evidence of
unsupported execution, never a successful measurement.
"""

import argparse
import copy
import hashlib
import importlib
import json
from decimal import Decimal
from pathlib import Path
import subprocess
import sys


SCHEMA_VERSION = 1
BASELINE_REVISION = "4988677"
MODEL = "gpt-6-astra"
EFFORT = "medium"
INPUT_CAP = 32_000
OUTPUT_CAP = 4_096
LIMITS = {
    "input_tokens": 2_000_000,
    "output_tokens": 200_000,
    "physical_attempts": 4_500,
    "embedding_calls": 1_600,
    "hosted_actions": 20,
    "cost_usd": "40.00",
}
PRICES = {"input_per_million": "10.00", "cache_write_per_million": "12.50",
          "output_per_million": "50.00"}
MATRIX = (
    (0, 0, 0), (0, 3, 1), (0, 6, 3),
    (3, 0, 1), (3, 3, 3), (3, 6, 0),
    (30, 0, 3), (30, 3, 0), (30, 6, 1),
)


def _fixture_module(name):
    """Load only repository fixture modules when Python isolated mode omits it."""
    directory = str(Path(__file__).resolve().parent)
    if directory not in sys.path:
        sys.path.insert(0, directory)
    return importlib.import_module(name)


def _canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def _sha256(value):
    return hashlib.sha256(_canonical(value)).hexdigest()


def maximum_request_reservation():
    """Return the USD reservation for one request at both approved token caps."""
    # Before usage identifies cache treatment, reserve as a cache write.  A
    # reported cached-read value can refine a result but cannot lower a
    # pre-send hard bound.
    return ((Decimal(INPUT_CAP) * Decimal(PRICES["cache_write_per_million"]) +
             Decimal(OUTPUT_CAP) * Decimal(PRICES["output_per_million"])) /
            Decimal(1_000_000)).quantize(Decimal("0.000001"))


class ReservationLedger:
    """Small offline model of the required pre-send reservation behaviour."""

    def __init__(self, limit=Decimal(LIMITS["cost_usd"])):
        self.limit = Decimal(str(limit))
        self.reserved = Decimal("0")
        self.settled = Decimal("0")
        self._active = {}

    def reserve_maximum(self, receipt_id):
        if not isinstance(receipt_id, str) or not receipt_id or receipt_id in self._active:
            raise ValueError("reservation receipt id must be new and non-empty")
        amount = maximum_request_reservation()
        if self.reserved + self.settled + amount > self.limit:
            raise ValueError("cost reservation exhausted before send")
        self.reserved += amount
        self._active[receipt_id] = amount
        return amount

    def settle(self, receipt_id, actual):
        actual = Decimal(str(actual))
        reserved = self._active.pop(receipt_id, None)
        if reserved is None or actual < 0 or actual > reserved:
            raise ValueError("invalid settled amount")
        self.reserved -= reserved
        self.settled += actual
        return reserved - actual

    def retain_missing_usage(self, receipt_id):
        """Leave the cache-write worst-case reservation consumed on missing usage."""
        reserved = self._active.pop(receipt_id, None)
        if reserved is None:
            raise ValueError("unknown reservation receipt id")
        self.reserved -= reserved
        self.settled += reserved


def _trial(phase, identifier, *, turns, mode, skills=None, tools=None, shots=None, repeat=None, task=None):
    trial = {
        "id": identifier,
        "phase": phase,
        "model": MODEL,
        "effort": EFFORT,
        "turns": turns,
        "mode": mode,
        "quality": ["answer", "required_evidence", "scope"],
        "runtime": {"max_seconds": 600, "max_primary_requests_per_turn": 4,
                    "stop_tracker_limit": 5, "physical_attempt_ledger_required": True},
    }
    if skills is not None:
        trial["skills"] = skills
    if tools is not None:
        trial["tools"] = tools
    if shots is not None:
        trial["shots"] = shots
    if repeat is not None:
        trial["repeat"] = repeat
    if task is not None:
        trial["task"] = task
    return trial


def build_trials():
    """Return the approved 84-process upper-bound strata without executing it."""
    trials = []
    trials.extend([
        _trial("P1", "P1-baseline", turns=12, mode="baseline"),
        _trial("P1", "P1-strict10", turns=12, mode="strict10"),
        _trial("P1", "P1-web-canary", turns=1, mode="web", tools=3),
    ])
    for index, (skills, tools, shots) in enumerate(MATRIX, 1):
        for mode in ("baseline", "workflow"):
            trials.append(_trial("P2a", "P2a-%02d-%s" % (index, mode), turns=1, mode=mode,
                                 skills=skills, tools=tools, shots=shots))
    for task in ("correction-resume", "fork-phase"):
        for mode in ("baseline", "workflow", "strict10", "workflow-strict10"):
            for repeat in range(1, 4):
                trials.append(_trial("P2b", "P2b-%s-%s-r%d" % (task, mode, repeat),
                                     turns=36, mode=mode, repeat=repeat, task=task))
    for task in ("singlefile_repair", "multifile_repair", "long_log_rootcause",
                 "superseded_decision", "changed_source_evidence"):
        for mode in ("baseline", "adopted-candidate"):
            for repeat in range(1, 4):
                trial = _trial("P2c", "P2c-%s-%s-r%d" % (task, mode, repeat),
                               turns=1, mode=mode, repeat=repeat, task=task)
                trial["runtime"]["max_primary_requests_per_turn"] = 12
                # Existing repair CLI semantics allow 11 sends when this is
                # 12.  The durable ledger, not StopTracker, remains the M0
                # hard twelve-attempt ceiling.
                trial["runtime"]["stop_tracker_limit"] = 12
                trials.append(trial)
    for task in ("web-1", "web-2", "web-3"):
        for repeat in range(1, 4):
            trials.append(_trial("P2d", "P2d-%s-r%d" % (task, repeat), turns=1,
                                 mode="web", tools=3, repeat=repeat, task=task))
    return trials


def _phase_counts(trials):
    return {phase: sum(trial["phase"] == phase for trial in trials)
            for phase in ("P1", "P2a", "P2b", "P2c", "P2d")}


def validate_trials(trials):
    counts = _phase_counts(trials)
    expected = {"P1": 3, "P2a": 18, "P2b": 24, "P2c": 30, "P2d": 9}
    if counts != expected or len(trials) != 84:
        raise ValueError("M0 trial upper bound is not 84")
    if len({trial["id"] for trial in trials}) != 84:
        raise ValueError("trial ids must be unique")
    p2a = [trial for trial in trials if trial["phase"] == "P2a"]
    if {(t["skills"], t["tools"]) for t in p2a} != {(a, b) for a in (0, 3, 30) for b in (0, 3, 6)}:
        raise ValueError("P2a does not cover skill/tool pairs")
    if {(t["skills"], t["shots"]) for t in p2a} != {(a, b) for a in (0, 3, 30) for b in (0, 1, 3)}:
        raise ValueError("P2a does not cover skill/shot pairs")
    if {(t["tools"], t["shots"]) for t in p2a} != {(a, b) for a in (0, 3, 6) for b in (0, 1, 3)}:
        raise ValueError("P2a does not cover tool/shot pairs")
    if sum(t["turns"] for t in trials if t["phase"] == "P2b") != 864:
        raise ValueError("P2b must contain 864 user turns")
    if any(t["runtime"]["max_primary_requests_per_turn"] != 12 for t in trials if t["phase"] == "P2c"):
        raise ValueError("P2c must retain its one-instruction, twelve-request cap")
    if any(t["model"] != MODEL or t["effort"] != EFFORT for t in trials):
        raise ValueError("model strata drifted")
    return counts


def _cache_case_for_trial(trial):
    """Reuse the existing production-Session fixture shape without mutating it."""
    cache_quality = _fixture_module("benchmark_cache_quality")

    if trial["phase"] == "P2a":
        index = int(trial["id"].split("-")[1]) - 1
        case = copy.deepcopy(cache_quality.cases()[index])
    elif trial["phase"] == "P2b":
        case = copy.deepcopy(cache_quality.cases()[9 if trial["task"] == "correction-resume" else 10])
    else:
        # P1 has a real 12-turn Session fixture.  It deliberately repeats a
        # simple answer contract rather than inventing a model-side feature.
        base = copy.deepcopy(cache_quality.cases()[0])
        case = {"id": trial["id"], "skill_count": 0, "tool_count": 0,
                "examples": [], "turns": []}
        for turn in range(1, 13):
            item = copy.deepcopy(base["turns"][0])
            item["prompt"] = item["prompt"].replace("Q01", "P1-%02d" % turn)
            item["expected"]["value"] = "USER-P1-%02d-4C8E" % turn
            case["turns"].append(item)
    case["id"] = trial["id"]
    return case


def concrete_case(trial):
    """Return a concrete, JSON-only fixture contract for one frozen trial."""
    if trial["mode"] == "web":
        return {"kind": "web", "prompt": "検索結果のURLと根拠をJSONだけで返す。",
                "expected": {"url": "https://example.invalid/sadalmelik", "source": "hosted_web_search"},
                "required_tools": [{"name": "web_search", "arguments": {"q": "sadalmelik"}, "min_calls": 1}]}
    if trial["phase"] in ("P1", "P2a", "P2b"):
        return {"kind": "conversation", "case": _cache_case_for_trial(trial)}
    if trial["phase"] == "P2c":
        algedi = _fixture_module("benchmark_algedi")
        return {"kind": "repair", "task": trial["task"], "prompt": algedi.PROMPTS[trial["task"]]}


def prepare_fixtures(output, baseline_revision=BASELINE_REVISION):
    """Materialize every frozen fixture.  This performs no model request."""
    algedi = _fixture_module("benchmark_algedi")
    cache_quality = _fixture_module("benchmark_cache_quality")

    root = Path(output)
    if root.exists():
        raise ValueError("fixture output must be new")
    root.mkdir(parents=True)
    manifests, receipt = frozen_manifests(baseline_revision)
    for manifest in manifests.values():
        for trial in manifest["trials"]:
            trial_root = root / trial["id"]
            fixture = trial_root / "fixture"
            trial_root.mkdir()
            contract = concrete_case(trial)
            if contract["kind"] == "conversation":
                cache_quality.prepare_case(contract["case"], fixture)
            elif contract["kind"] == "repair":
                fixture.mkdir()
                for name, contents in algedi.task_files(contract["task"]).items():
                    (fixture / name).write_text(contents, encoding="utf-8")
            else:
                fixture.mkdir()
                (fixture / "query.txt").write_text(contract["prompt"], encoding="utf-8")
            run = dict(trial)
            run["fixture_sha256"] = _sha256(contract)
            run["contract"] = contract
            run["command"] = ["benchmark_sadalmelik.py", "run-fixture", "--case", "case.json",
                              "--fixture", "fixture", "--binary", "PROVIDER_BOUND_BINARY", "--output", "rows.jsonl"]
            (trial_root / "case.json").write_bytes(_canonical(run) + b"\n")
    (root / "freeze-receipt.json").write_bytes(_canonical(receipt) + b"\n")
    return receipt


def _same(left, right):
    if type(left) is not type(right):
        return False
    if isinstance(left, dict):
        return left.keys() == right.keys() and all(_same(left[key], right[key]) for key in left)
    if isinstance(left, list):
        return len(left) == len(right) and all(_same(a, b) for a, b in zip(left, right))
    return left == right


def _quality_rows(trial, contract, rows, fixture):
    if contract["kind"] == "conversation":
        turns = contract["case"]["turns"]
        if len(rows) != len(turns):
            raise ValueError("incomplete conversation rows")
        for number, (turn, row) in enumerate(zip(turns, rows), 1):
            if (row.get("id") != trial["id"] or row.get("turn") != number
                    or row.get("model") != MODEL or row.get("effort") != EFFORT
                    or not _same(row.get("answer"), turn["expected"])
                    or row.get("scope_pass") is not True):
                raise ValueError("conversation answer, scope, or model contract failed")
            if not isinstance(row.get("requests"), int) or not 1 <= row["requests"] <= trial["runtime"]["max_primary_requests_per_turn"]:
                raise ValueError("conversation request cap failed")
            if row.get("offline_fake") is not True and not row.get("physical_attempt_ids"):
                raise ValueError("real execution lacks physical attempt ledger evidence")
        return
    if contract["kind"] == "repair":
        algedi = _fixture_module("benchmark_algedi")
        if len(rows) != 1 or rows[0].get("scope_pass") is not True:
            raise ValueError("repair scope contract failed")
        if rows[0].get("offline_fake") is not True and not rows[0].get("physical_attempt_ids"):
            raise ValueError("real repair execution lacks physical attempt ledger evidence")
        state, reason = algedi.quality_result(contract["task"], rows[0].get("stdout", ""), Path(fixture))
        if state != "pass":
            raise ValueError("repair quality failed: %s" % (reason or state))
        return
    if len(rows) != 1 or not _same(rows[0].get("answer"), contract["expected"]):
        raise ValueError("web answer contract failed")
    if rows[0].get("offline_fake") is not True and not rows[0].get("physical_attempt_ids"):
        raise ValueError("real web execution lacks physical attempt ledger evidence")
    calls = rows[0].get("tool_calls")
    if calls != contract["required_tools"]:
        raise ValueError("web evidence contract failed")


def run_fixture(case_path, fixture, binary, output):
    """Run a supplied provider-bound binary then judge immutable fixture facts."""
    case = _read_json(case_path)
    trial = {key: case[key] for key in ("id", "phase", "model", "effort", "runtime")}
    contract = case["contract"]
    command = [str(Path(binary).resolve()), "--case", str(Path(case_path).resolve()),
               "--fixture", str(Path(fixture).resolve()), "--output", str(Path(output).resolve())]
    completed = subprocess.run(command, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=600)
    if completed.returncode != 0:
        raise ValueError("fixture binary failed")
    rows = [json.loads(line) for line in Path(output).read_text(encoding="utf-8").splitlines()]
    _quality_rows(trial, contract, rows, fixture)
    return {"status": "offline_fixture_passed", "rows": len(rows)}


def fake_binary(case_path, fixture, output):
    """Offline fake binary for end-to-end fixture/quality tests only."""
    case = _read_json(case_path)
    trial, contract = case, case["contract"]
    rows = []
    if contract["kind"] == "conversation":
        for number, turn in enumerate(contract["case"]["turns"], 1):
            rows.append({"id": trial["id"], "turn": number, "model": MODEL, "effort": EFFORT,
                         "answer": turn["expected"], "scope_pass": True, "requests": 1,
                         "offline_fake": True})
    elif contract["kind"] == "repair":
        task = contract["task"]
        root = Path(fixture)
        if task == "singlefile_repair":
            (root / "rates.py").write_text("def shipping_fee(weight_g):\n    if weight_g < 0:\n        raise ValueError('negative')\n    return 500 + max(0, (weight_g - 1000 + 999) // 1000) * 200\n", encoding="utf-8")
        elif task == "multifile_repair":
            (root / "pricing.py").write_text("def total(subtotal):\n    return subtotal * 110 // 100\n", encoding="utf-8")
            (root / "receipt.py").write_text("from pricing import total\ndef render(subtotal):\n    return f'total={total(subtotal)}'\n", encoding="utf-8")
        else:
            answers = {
                "long_log_rootcause": {"root_cause": "TLS_CERT_EXPIRED", "serial": "AL-42"},
                "superseded_decision": {"decision_id": "D011", "retention_days": 30},
                "changed_source_evidence": {"evidence_id": "EV-7", "saved_retention_days": 30, "current_retention_days": 45, "changed": True},
            }
            if task == "changed_source_evidence":
                (root / "current_source.md").write_text("現行時点: policy_version=4; retention_days=45\n", encoding="utf-8")
            stdout = json.dumps(answers[task], ensure_ascii=False)
        rows.append({"scope_pass": True, "stdout": locals().get("stdout", ""), "offline_fake": True})
    else:
        rows.append({"answer": contract["expected"], "tool_calls": contract["required_tools"], "offline_fake": True})
    Path(output).write_text("".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows), encoding="utf-8")


def frozen_manifests(baseline_revision=BASELINE_REVISION):
    trials = build_trials()
    counts = validate_trials(trials)
    manifests = {}
    for phase in ("P0", "P1", "P2a", "P2b", "P2c", "P2d"):
        phase_trials = [trial for trial in trials if trial["phase"] == phase]
        manifests[phase] = {
            "schema": SCHEMA_VERSION,
            "phase": phase,
            "baseline_revision": baseline_revision,
            "model": {"name": MODEL, "effort": EFFORT},
            "limits": LIMITS,
            "prices": PRICES,
            "trials": phase_trials,
            "offline_only": phase == "P0",
            "requires_preflight": phase != "P0",
        }
    receipt = {
        "schema": SCHEMA_VERSION,
        "baseline_revision": baseline_revision,
        "phase_counts": {"P0": 0, **counts},
        "manifest_sha256": {phase: _sha256(manifest) for phase, manifest in manifests.items()},
        "max_request_reservation_usd": str(maximum_request_reservation()),
        "limits": LIMITS,
    }
    return manifests, receipt


def freeze(output, baseline_revision=BASELINE_REVISION):
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    manifests, receipt = frozen_manifests(baseline_revision)
    for phase, manifest in manifests.items():
        (output / ("manifest-%s.json" % phase.lower())).write_bytes(_canonical(manifest) + b"\n")
    (output / "freeze-receipt.json").write_bytes(_canonical(receipt) + b"\n")
    return receipt


def _require(capabilities, path, expected, missing):
    current = capabilities
    for key in path:
        if not isinstance(current, dict) or key not in current:
            missing.append("%s=%r" % (".".join(path), expected))
            return
        current = current[key]
    if current != expected:
        missing.append("%s=%r" % (".".join(path), expected))


def provider_bound_proof():
    """Hashes that a parent-owned normal-request binding must attest to."""
    root = Path(__file__).resolve().parents[1]
    paths = ("crates/polaris-provider/src/codex.rs", "crates/polaris-provider/src/attempts.rs")
    return {path: hashlib.sha256((root / path).read_bytes()).hexdigest() for path in paths}


def preflight(capabilities, proof=None):
    """Validate caps plus an immutable normal-request/ledger binding proof."""
    missing = []
    checks = (
        (("schema",), SCHEMA_VERSION),
        (("provider", "name"), "codex"),
        (("provider", "model"), MODEL),
        (("provider", "effort"), EFFORT),
        (("provider", "max_input_tokens"), INPUT_CAP),
        (("provider", "max_output_tokens"), OUTPUT_CAP),
        (("provider", "hard_output_cap"), True),
        (("provider", "input_usage_bounded"), True),
        (("provider", "usage_complete"), True),
        (("provider", "hosted_action_cap"), 2),
        (("ledger", "persistent"), True),
        (("ledger", "pre_send_reservation"), True),
        (("ledger", "max_physical_attempts"), LIMITS["physical_attempts"]),
        (("ledger", "max_input_tokens"), LIMITS["input_tokens"]),
        (("ledger", "max_output_tokens"), LIMITS["output_tokens"]),
        (("ledger", "max_embedding_calls"), LIMITS["embedding_calls"]),
        (("ledger", "max_hosted_actions"), LIMITS["hosted_actions"]),
        (("ledger", "max_cost_usd"), LIMITS["cost_usd"]),
        (("ledger", "missing_usage_retains_reservation"), True),
    )
    for path, expected in checks:
        _require(capabilities, path, expected, missing)
    expected_kinds = {"completion", "summary", "embedding", "web_search", "child"}
    kinds = capabilities.get("ledger", {}).get("kinds") if isinstance(capabilities, dict) else None
    if not isinstance(kinds, list) or set(kinds) != expected_kinds:
        missing.append("ledger.kinds=%r" % sorted(expected_kinds))
    required_proof = provider_bound_proof()
    if not isinstance(proof, dict) or proof.get("schema") != SCHEMA_VERSION:
        missing.append("provider_bound_proof=schema-1 immutable source hashes")
    elif proof.get("sources") != required_proof:
        missing.append("provider_bound_proof.sources=current normal request and ledger hashes")
    elif proof.get("normal_request_builder_bound") is not True or proof.get("durable_ledger_bound") is not True:
        missing.append("provider_bound_proof=normal request builder and durable ledger binding")
    trial_blockers = {}
    generic = bool(missing)
    for trial in build_trials():
        blockers = []
        if generic:
            blockers.append("provider-bound hard-cap proof")
        if trial["mode"] == "web" and (not isinstance(capabilities, dict) or capabilities.get("web_capability_verified") is not True):
            blockers.append("Codex hosted-web endpoint capability")
        if "strict10" in trial["mode"] and (not isinstance(capabilities, dict) or capabilities.get("strict10_runtime_verified") is not True):
            blockers.append("strict10 local embedding runtime")
        if blockers:
            trial_blockers[trial["id"]] = blockers
    report = {
        "schema": SCHEMA_VERSION,
        "status": "ready" if not trial_blockers else "unsupported",
        "reason": None if not trial_blockers else "provider capability receipt does not prove M0 hard bounds",
        "missing_or_mismatched": missing,
        "blocked_trials": trial_blockers,
        "blocked_phases": sorted({trial_id.split("-")[0] for trial_id in trial_blockers}),
        "permitted_phases": ["P0"],
        "required": {"model": MODEL, "effort": EFFORT, "input_cap": INPUT_CAP,
                     "output_cap": OUTPUT_CAP, "limits": LIMITS},
        "max_request_reservation_usd": str(maximum_request_reservation()),
    }
    return report


def _read_json(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))


def main(argv=None):
    parser = argparse.ArgumentParser(description="Sadalmelik benchmark freeze and fail-closed preflight")
    commands = parser.add_subparsers(dest="command", required=True)
    freeze_parser = commands.add_parser("freeze")
    freeze_parser.add_argument("--output", required=True)
    freeze_parser.add_argument("--baseline-revision", default=BASELINE_REVISION)
    preflight_parser = commands.add_parser("preflight")
    preflight_parser.add_argument("--capabilities", required=True)
    preflight_parser.add_argument("--provider-bound-proof")
    preflight_parser.add_argument("--output", required=True)
    fixtures_parser = commands.add_parser("prepare-fixtures")
    fixtures_parser.add_argument("--output", required=True)
    fixtures_parser.add_argument("--baseline-revision", default=BASELINE_REVISION)
    run_parser = commands.add_parser("run-fixture")
    run_parser.add_argument("--case", required=True)
    run_parser.add_argument("--fixture", required=True)
    run_parser.add_argument("--binary", required=True)
    run_parser.add_argument("--output", required=True)
    fake_parser = commands.add_parser("fake-binary")
    fake_parser.add_argument("--case", required=True)
    fake_parser.add_argument("--fixture", required=True)
    fake_parser.add_argument("--output", required=True)
    args = parser.parse_args(argv)
    if args.command == "freeze":
        print(json.dumps(freeze(args.output, args.baseline_revision), ensure_ascii=False, sort_keys=True))
        return 0
    if args.command == "prepare-fixtures":
        print(json.dumps(prepare_fixtures(args.output, args.baseline_revision), ensure_ascii=False, sort_keys=True))
        return 0
    if args.command == "fake-binary":
        fake_binary(args.case, args.fixture, args.output)
        return 0
    if args.command == "run-fixture":
        print(json.dumps(run_fixture(args.case, args.fixture, args.binary, args.output), ensure_ascii=False, sort_keys=True))
        return 0
    proof = _read_json(args.provider_bound_proof) if args.provider_bound_proof else None
    report = preflight(_read_json(args.capabilities), proof)
    Path(args.output).write_bytes(_canonical(report) + b"\n")
    print(json.dumps(report, ensure_ascii=False, sort_keys=True))
    return 0 if report["status"] == "ready" else 2


if __name__ == "__main__":
    raise SystemExit(main())
