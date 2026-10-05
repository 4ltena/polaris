#!/usr/bin/env python3
"""Validate the fixed Windows TUI stage evidence inventory."""

from __future__ import annotations

import argparse
from pathlib import Path


CONTRACTS = (
    "T01", "T02", "T03", "T04", "F15", "T05", "T06", "T07",
    "T11", "T12", "T13", "T15", "F21", "F22", "T18", "T21",
    "C32", "C35", "rewind_admission", "secret_input",
)
HEADERS = (
    "Contract", "Linux reference", "Windows synthetic",
    "Windows TUI operation", "Windows terminal", "Open limit",
)
RESULTS = {"not_run", "passed", "failed", "blocked_external", "not_applicable"}
RECORD = (
    Path(__file__).resolve().parents[1]
    / ".polaris/notes/checks/2026-09-27-windows-tui-stage.md"
)


def cells(line: str) -> tuple[str, ...]:
    if not line.startswith("|") or not line.endswith("|"):
        raise ValueError(f"invalid table row: {line!r}")
    return tuple(part.strip() for part in line[1:-1].split("|"))


def check_result(contract: str, field: str, value: str) -> None:
    parts = [part.strip() for part in value.split(";")]
    status = parts[0]
    if status not in RESULTS:
        raise ValueError(f"{contract} {field}: unknown result {status!r}")
    if status != "passed":
        if len(parts) != 1:
            raise ValueError(f"{contract} {field}: metadata requires passed")
        return
    details = {}
    for part in parts[1:]:
        key, separator, detail = part.partition("=")
        if not separator or not key or not detail:
            raise ValueError(f"{contract} {field}: malformed passed evidence")
        if key in details:
            raise ValueError(f"{contract} {field}: duplicate {key}")
        details[key] = detail
    if set(details) != {"command", "result", "evidence"}:
        raise ValueError(f"{contract} {field}: passed requires command, result, evidence")


def validate(text: str) -> int:
    lines = text.splitlines()
    try:
        start = next(i for i, line in enumerate(lines) if line.strip() == "| " + " | ".join(HEADERS) + " |")
    except StopIteration as exc:
        raise ValueError("Windows evidence table header missing") from exc
    if start + 1 >= len(lines) or len(cells(lines[start + 1])) != len(HEADERS):
        raise ValueError("Windows evidence table separator missing")
    found = []
    for line in lines[start + 2:]:
        if not line.startswith("|"):
            break
        row = cells(line)
        if len(row) != len(HEADERS):
            raise ValueError(f"expected {len(HEADERS)} columns, got {len(row)}")
        contract, linux_reference, synthetic, operation, terminal, open_limit = row
        if not linux_reference or not open_limit:
            raise ValueError(f"{contract}: reference and open limit are required")
        if contract in found:
            raise ValueError(f"duplicate contract: {contract}")
        found.append(contract)
        check_result(contract, "synthetic", synthetic)
        check_result(contract, "operation", operation)
        check_result(contract, "terminal", terminal)
    missing = set(CONTRACTS) - set(found)
    extra = set(found) - set(CONTRACTS)
    if missing or extra:
        raise ValueError(f"contract set mismatch: missing={sorted(missing)}, extra={sorted(extra)}")
    return len(found)


def self_test() -> None:
    header = "| " + " | ".join(HEADERS) + " |\n"
    separator = "| " + " | ".join("---" for _ in HEADERS) + " |\n"
    rows = [
        f"| {name} | fixed Linux snapshot | not_run | not_run | not_run | open |\n"
        for name in CONTRACTS
    ]
    baseline = header + separator + "".join(rows)
    assert validate(baseline) == len(CONTRACTS)
    broken = (
        baseline.replace(rows[0], ""),
        baseline + rows[0],
        baseline + "| EXTRA | ref | not_run | not_run | not_run | open |\n",
        baseline.replace("not_run", "passed", 1),
    )
    for fixture in broken:
        try:
            validate(fixture)
        except ValueError:
            continue
        raise AssertionError("invalid exact-ID fixture was accepted")
    valid_pass = baseline.replace(
        "not_run",
        "passed; command=cargo test fixed; result=exit 0; evidence=checks/run.txt",
        1,
    )
    assert validate(valid_pass) == len(CONTRACTS)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--file", type=Path, default=RECORD)
    args = parser.parse_args()
    try:
        if args.self_test:
            self_test()
            print("self-test passed: exact, missing, duplicate, extra, false passed")
        count = validate(args.file.read_text(encoding="utf-8"))
        print(f"valid Windows TUI stage record: {count} contracts")
    except (OSError, ValueError, AssertionError) as exc:
        parser.exit(1, f"validation failed: {exc}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
