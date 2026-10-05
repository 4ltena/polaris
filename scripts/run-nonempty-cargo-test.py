#!/usr/bin/env python3
"""Run a focused Cargo test only when enumeration proves a nonempty exact set."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys


FIXTURES = Path(__file__).resolve().parent / "fixtures" / "run-nonempty-cargo-test"
TEST_RECORD = re.compile(r"^(.+): test$")
RESULT_RECORD = re.compile(
    r"^test result: (?:ok|FAILED)\. "
    r"([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored;",
    re.MULTILINE,
)

TEST_ONLY_FLAGS = (
    "--nocapture", "--list", "--format", "--exact", "--ignored",
    "--include-ignored", "--show-output", "--skip", "--test-threads",
    "--ensure-time", "--exclude-should-panic", "--logfile", "--time",
)


def has_flag(value: str, flag: str) -> bool:
    return value == flag or value.startswith(flag + "=")



def listed_names(output: str) -> list[str]:
    names: list[str] = []
    seen: set[str] = set()
    for line in output.splitlines():
        match = TEST_RECORD.fullmatch(line)
        if match is None:
            continue
        name = match.group(1)
        if name.strip() != name:
            continue
        if name in seen:
            raise ValueError(f"duplicate_name: {name}")
        seen.add(name)
        names.append(name)
    if not names:
        raise ValueError("zero_match: no exact '<full-test-name>: test' records")
    return names


def executed_totals(output: str) -> dict[str, int]:
    records = RESULT_RECORD.findall(output)
    if not records:
        raise ValueError("no_test_result: execution emitted no harness summary")
    return {
        key: sum(int(row[index]) for row in records)
        for index, key in enumerate(("passed", "failed", "ignored"))
    }


def commands(
    cargo_bin: str,
    package: str,
    selector: str,
    cargo_args: list[str],
    test_args: list[str],
) -> tuple[list[str], list[str]]:
    if not package or not selector or not cargo_bin:
        raise ValueError("cargo_bin, package and filter must be nonempty")
    if any(value in {"--", "--package", "-p"} or value.startswith("--package=") for value in cargo_args):
        raise ValueError("package/separator must be supplied by the wrapper")
    if any(has_flag(value, flag) for value in cargo_args for flag in TEST_ONLY_FLAGS):
        raise ValueError("test-only arguments belong after the separator")
    if any(value == "--" or has_flag(value, "--list") or has_flag(value, "--format") for value in test_args):
        raise ValueError("test arguments cannot override enumeration")
    if any(value.startswith(("--locked", "--offline", "--lib", "--package", "--bin", "--test=")) for value in test_args):
        raise ValueError("Cargo arguments belong before the separator")
    prefix = [cargo_bin, "test", *cargo_args, "--package", package, selector, "--"]
    return (
        [*prefix, "--list", "--format=terse"],
        [*prefix, *test_args],
    )


def save_evidence(path: Path, manifest: dict[str, object]) -> None:
    payload = (json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode("utf-8")
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as target:
        target.write(payload)
        target.flush()
        os.fsync(target.fileno())


def self_test() -> None:
    parsed = argument_parser().parse_args([
        "--cargo-bin", "/home/example/.cargo/bin/cargo",
        "--package", "polaris-core", "--filter", "workspace_apply::tests::",
        "--cargo-arg=--locked", "--cargo-arg=--offline", "--cargo-arg=--lib",
        "--test-arg=--nocapture", "--evidence-json", "/tmp/fixed-evidence.json",
    ])
    assert parsed.cargo_arg == ["--locked", "--offline", "--lib"]
    assert parsed.test_arg == ["--nocapture"]
    enumerate_argv, execute_argv = commands(
        parsed.cargo_bin, parsed.package, parsed.filter, parsed.cargo_arg, parsed.test_arg,
    )
    assert enumerate_argv == [
        "/home/example/.cargo/bin/cargo", "test", "--locked", "--offline", "--lib",
        "--package", "polaris-core", "workspace_apply::tests::", "--",
        "--list", "--format=terse",
    ]
    assert execute_argv == [
        "/home/example/.cargo/bin/cargo", "test", "--locked", "--offline", "--lib",
        "--package", "polaris-core", "workspace_apply::tests::", "--", "--nocapture",
    ]
    names = listed_names((FIXTURES / "happy.list").read_text(encoding="utf-8"))
    assert names == [
        "workspace_apply::tests::apply_restores_file",
        "workspace_apply::tests::rejects_escape",
    ]
    print(f"happy: {len(names)} exact tests; argument arrays separated")
    for label, filename, expected in (
        ("zero_match", "zero.list", "zero_match"),
        ("duplicate_name", "duplicate.list", "duplicate_name"),
        ("false_positive_text", "false-positive.list", "zero_match"),
    ):
        try:
            listed_names((FIXTURES / filename).read_text(encoding="utf-8"))
        except ValueError as exc:
            if not str(exc).startswith(expected + ":"):
                raise AssertionError(f"{label}: wrong rejection: {exc}") from exc
            print(f"{label}: rejected ({expected})")
        else:
            raise AssertionError(f"{label}: invalid fixture accepted")
    assert executed_totals("test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured;\n") == {
        "passed": 2, "failed": 0, "ignored": 0,
    }
    misplaced = (
        (["--nocapture"], [], "test-only arguments"),
        (["--list"], [], "test-only arguments"),
        (["--format=terse"], [], "test-only arguments"),
        (["--exact"], [], "test-only arguments"),
        ([], ["--locked"], "Cargo arguments"),
        ([], ["--list"], "test arguments"),
    )
    for cargo_args, test_args, expected in misplaced:
        try:
            commands("cargo", "polaris-core", "fixed::selector", cargo_args, test_args)
        except ValueError as exc:
            if expected not in str(exc):
                raise AssertionError(f"misplaced argument: wrong rejection: {exc}") from exc
        else:
            raise AssertionError("misplaced argument accepted")
    print(f"misplaced_args: rejected {len(misplaced)} cases")


def run(args: argparse.Namespace) -> int:
    enumerate_argv, execute_argv = commands(
        args.cargo_bin, args.package, args.filter, args.cargo_arg, args.test_arg,
    )
    evidence = args.evidence_json
    if args.timeout_seconds <= 0:
        raise ValueError("timeout-seconds must be positive")
    if evidence.exists():
        raise ValueError(f"evidence path already exists: {evidence}")
    manifest: dict[str, object] = {
        "selector": args.filter,
        "package": args.package,
        "cargo_arguments": args.cargo_arg,
        "test_arguments": args.test_arg,
        "enumeration_argv": enumerate_argv,
        "execution_argv": execute_argv,
        "selected_names": [],
        "enumerated_count": 0,
        "executed_totals": None,
        "enumeration_exit": None,
        "execution_exit": None,
        "timeout_seconds": args.timeout_seconds,
        "timeout_phase": None,
        "status": "incomplete",
    }
    phase = "enumeration"
    try:
        print(shlex.join(enumerate_argv), flush=True)
        listing = subprocess.run(
            enumerate_argv, text=True, capture_output=True, check=False,
            stdin=subprocess.DEVNULL, timeout=args.timeout_seconds,
        )
        manifest["enumeration_exit"] = listing.returncode
        sys.stdout.write(listing.stdout)
        sys.stderr.write(listing.stderr)
        if listing.returncode:
            raise ValueError(f"Cargo enumeration failed: exit {listing.returncode}")
        names = listed_names(listing.stdout)
        manifest["selected_names"] = names
        manifest["enumerated_count"] = len(names)
        print(shlex.join(execute_argv), flush=True)
        phase = "execution"
        execution = subprocess.run(
            execute_argv, text=True, capture_output=True, check=False,
            stdin=subprocess.DEVNULL, timeout=args.timeout_seconds,
        )
        manifest["execution_exit"] = execution.returncode
        sys.stdout.write(execution.stdout)
        sys.stderr.write(execution.stderr)
        totals = executed_totals(execution.stdout)
        manifest["executed_totals"] = totals
        if sum(totals.values()) != len(names):
            raise ValueError(
                f"count_mismatch: enumerated {len(names)}, executed totals {sum(totals.values())}"
            )
        if execution.returncode or totals["failed"] or totals["ignored"]:
            raise ValueError(f"focused test did not pass: exit {execution.returncode}, totals {totals}")
        manifest["status"] = "passed"
        return 0
    except subprocess.TimeoutExpired:
        manifest["status"] = "failed"
        manifest["timeout_phase"] = phase
        manifest["error"] = f"Cargo {phase} timed out after {args.timeout_seconds} seconds"
        print(f"validation failed: {manifest['error']}", file=sys.stderr)
        return 1
    except (OSError, ValueError) as exc:
        manifest["status"] = "failed"
        manifest["error"] = str(exc)
        print(f"validation failed: {exc}", file=sys.stderr)
        return 1
    finally:
        save_evidence(evidence, manifest)
        print(f"evidence: {evidence}", flush=True)


def argument_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--cargo-bin", default="cargo")
    parser.add_argument("--package")
    parser.add_argument("--filter")
    parser.add_argument("--cargo-arg", action="append", default=[])
    parser.add_argument("--test-arg", action="append", default=[])
    parser.add_argument("--evidence-json", type=Path)
    parser.add_argument("--timeout-seconds", type=int, default=900)
    return parser


def main() -> int:
    parser = argument_parser()
    args = parser.parse_args()
    try:
        if args.self_test:
            self_test()
            return 0
        if not args.package or not args.filter or args.evidence_json is None:
            parser.error("--package, --filter and --evidence-json are required")
        return run(args)
    except (OSError, ValueError, AssertionError) as exc:
        parser.exit(1, f"validation failed: {exc}\n")


if __name__ == "__main__":
    raise SystemExit(main())
