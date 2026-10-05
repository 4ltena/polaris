#!/usr/bin/env python3
"""Check the TUI parity inventory and Linux ledger against the exact approved ID set."""

from __future__ import annotations

import argparse
from collections import Counter
from pathlib import Path
import re


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_SPEC = ROOT / ".project-notes/specs/2026-09-23-v100-tui-first-parity.md"
DEFAULT_LEDGER = ROOT / ".polaris/notes/checks/2026-09-26-tui-parity-id-ledger.md"
FIXTURES = Path(__file__).resolve().parent / "fixtures" / "tui-parity-ledger"
LEDGER_FIELDS = {
    "ID", "platform_disposition", "linux_status", "all_platform_status", "reason", "evidence",
}
DISPOSITIONS = {
    "linux_required", "linux_shared_core_only", "linux_unavailable_external", "exception",
}
STATUSES = {"pending", "passed", "failed", "blocked_external", "not_applicable"}
LINK = re.compile(r"\[[^\]]+\]\(([^)]+)\)")
ADDENDUM = re.compile(
    r"^D42 addendum: approved by user; \[[^\]]+\]\(([^)]+)\)$", re.MULTILINE,
)


def expected_ids(with_d42: bool) -> set[str]:
    base = {
        f"{prefix}{number:02d}"
        for prefix, end in (("C", 42), ("D", 41), ("T", 21), ("F", 25))
        for number in range(1, end + 1)
    }
    if with_d42:
        base.add("D42")
    return base


def table_cells(line: str) -> list[str]:
    if not line.startswith("|") or not line.endswith("|"):
        raise ValueError(f"malformed table row: {line!r}")
    return [part.strip() for part in line[1:-1].split("|")]


def id_tables(markdown: str) -> list[tuple[list[str], list[dict[str, str]]]]:
    lines = markdown.splitlines()
    tables: list[tuple[list[str], list[dict[str, str]]]] = []
    index = 0
    while index < len(lines):
        line = lines[index].strip()
        if not line.startswith("| ID |"):
            index += 1
            continue
        header = table_cells(line)
        if index + 1 >= len(lines):
            raise ValueError("table separator missing")
        separator = table_cells(lines[index + 1].strip())
        if len(separator) != len(header) or any(
            re.fullmatch(r":?-{3,}:?", part) is None for part in separator
        ):
            raise ValueError("table separator invalid")
        index += 2
        rows: list[dict[str, str]] = []
        while index < len(lines) and lines[index].lstrip().startswith("|"):
            cells = table_cells(lines[index].strip())
            if len(cells) != len(header):
                raise ValueError(f"table row width differs from header: {lines[index]!r}")
            rows.append(dict(zip(header, cells)))
            index += 1
        tables.append((header, rows))
    return tables


def inventory(spec_text: str) -> tuple[list[str], dict[str, str]]:
    ids: list[str] = []
    routes: dict[str, str] = {}
    for header, rows in id_tables(spec_text):
        if len(header) < 3 or header[1] not in {"Operation", "Execution setting"}:
            continue
        if "TUI route" not in header:
            raise ValueError("parent inventory lacks TUI route")
        for row in rows:
            contract = row["ID"]
            if re.fullmatch(r"[A-Z][0-9]{2}", contract) is None:
                raise ValueError(f"malformed inventory ID: {contract!r}")
            ids.append(contract)
            routes[contract] = row["TUI route"]
    if not ids:
        raise ValueError("parent inventory tables missing")
    return ids, routes


def ledger_rows(ledger_text: str, combined_fixture: bool) -> list[dict[str, str]]:
    tables = id_tables(ledger_text)
    if not combined_fixture and len(tables) != 1:
        raise ValueError("ledger must contain exactly one ID table")
    matching = [(header, rows) for header, rows in tables if LEDGER_FIELDS <= set(header)]
    if len(matching) != 1:
        raise ValueError(
            "ledger must have exactly one ID table with platform_disposition, "
            "linux_status, all_platform_status, reason and evidence columns"
        )
    rows = matching[0][1]
    if not rows:
        raise ValueError("ledger table has no rows")
    for row in rows:
        if re.fullmatch(r"[A-Z][0-9]{2}", row["ID"]) is None:
            raise ValueError(f"malformed ledger ID: {row['ID']!r}")
    return rows


def no_duplicates(ids: list[str], source: str) -> None:
    duplicates = sorted(name for name, count in Counter(ids).items() if count > 1)
    if duplicates:
        raise ValueError(f"duplicate_id in {source}: {duplicates}")


def approved_d42(spec_text: str, spec_path: Path) -> bool:
    matches = ADDENDUM.findall(spec_text)
    if not matches:
        return False
    if len(matches) != 1:
        raise ValueError("D42 approval marker must occur exactly once")
    target = matches[0].split("#", 1)[0]
    if not target or Path(target).is_absolute() or "://" in target:
        raise ValueError("D42 addendum needs a local relative link")
    addendum_path = (spec_path.parent / target).resolve()
    if not addendum_path.is_file():
        raise ValueError(f"D42 approved addendum missing: {target}")
    addendum_text = addendum_path.read_text(encoding="utf-8")
    if re.search(r"^Status:\s*(?:\*\*)?approved by user\b", addendum_text, re.MULTILINE | re.IGNORECASE) is None:
        raise ValueError("D42 linked addendum is not marked approved by user")
    if "| D42 |" not in addendum_text:
        raise ValueError("D42 linked addendum has no D42 row")
    return True


def require_exact(actual: list[str], expected: set[str], source: str) -> None:
    missing = sorted(expected - set(actual))
    unexpected = sorted(set(actual) - expected)
    if missing or unexpected:
        raise ValueError(
            f"wrong_set in {source}: missing={missing}, unexpected={unexpected}"
        )


def expected_disposition(contract: str, route: str) -> str:
    if contract in {"D19", "D42"}:
        return "linux_shared_core_only"
    if contract == "D41":
        return "linux_unavailable_external"
    if route.startswith("X "):
        return "exception"
    return "linux_required"


def check_evidence(value: str, ledger_path: Path, contract: str) -> None:
    links = LINK.findall(value)
    if not links:
        raise ValueError(f"{contract}: evidence must contain a Markdown file link")
    for raw_target in links:
        target = raw_target.split("#", 1)[0]
        if not target or Path(target).is_absolute() or "://" in target:
            raise ValueError(f"{contract}: evidence must use a local relative file link")
        if not (ledger_path.parent / target).resolve().is_file():
            raise ValueError(f"{contract}: evidence file missing: {target}")


def validate(spec_path: Path, ledger_path: Path) -> int:
    spec_text = spec_path.read_text(encoding="utf-8")
    ledger_text = ledger_path.read_text(encoding="utf-8")
    spec_ids, routes = inventory(spec_text)
    rows = ledger_rows(ledger_text, spec_path.resolve() == ledger_path.resolve())
    ledger_ids = [row["ID"] for row in rows]
    no_duplicates(spec_ids, "parent spec")
    no_duplicates(ledger_ids, "ledger")
    d42_approved = approved_d42(spec_text, spec_path)
    if "D42" in spec_ids or "D42" in ledger_ids:
        if not d42_approved:
            raise ValueError("unapproved_d42: D42 row appears before approved addendum")
    elif d42_approved:
        raise ValueError("approved_d42_missing_row: addendum marker without inventory rows")
    expected = expected_ids(d42_approved)
    require_exact(spec_ids, expected, "parent spec")
    require_exact(ledger_ids, expected, "ledger")
    for row in rows:
        contract = row["ID"]
        disposition = row["platform_disposition"]
        linux = row["linux_status"]
        all_platform = row["all_platform_status"]
        if disposition not in DISPOSITIONS:
            raise ValueError(f"{contract}: unknown platform_disposition {disposition!r}")
        required_disposition = expected_disposition(contract, routes[contract])
        if disposition != required_disposition:
            raise ValueError(
                f"{contract}: disposition {disposition!r} conflicts with parent route; "
                f"expected {required_disposition!r}"
            )
        if linux not in STATUSES or all_platform not in STATUSES:
            raise ValueError(f"{contract}: unknown status ({linux!r}, {all_platform!r})")
        if disposition == "linux_required" and linux != "passed":
            raise ValueError(f"{contract}: incomplete linux_required status: {linux!r}")
        if not row["reason"] or row["reason"] == "未確認":
            raise ValueError(f"{contract}: reason is missing")
        check_evidence(row["evidence"], ledger_path, contract)
    return len(rows)


def self_test() -> None:
    for label, filename, expected in (
        ("valid_129", "valid-129.md", None),
        ("duplicate_id", "duplicate-id.md", "duplicate_id"),
        ("wrong_set_129", "wrong-set-129.md", "wrong_set"),
        ("unapproved_d42", "d42-without-addendum.md", "unapproved_d42"),
        ("valid_130_with_addendum", "valid-130-with-addendum.md", None),
    ):
        fixture = FIXTURES / filename
        try:
            count = validate(fixture, fixture)
        except ValueError as exc:
            if expected is None or expected not in str(exc):
                raise AssertionError(f"{label}: unexpected rejection: {exc}") from exc
            print(f"{label}: rejected ({expected})")
        else:
            if expected is not None:
                raise AssertionError(f"{label}: invalid fixture accepted")
            expected_count = 130 if "130" in label else 129
            if count != expected_count:
                raise AssertionError(f"{label}: wrong count {count}")
            print(f"{label}: accepted {count} exact IDs")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--spec", type=Path, default=DEFAULT_SPEC)
    parser.add_argument("--ledger", type=Path, default=DEFAULT_LEDGER)
    args = parser.parse_args()
    try:
        if args.self_test:
            self_test()
            return 0
        count = validate(args.spec, args.ledger)
        print(f"valid TUI parity ledger: {count} exact IDs")
        return 0
    except (OSError, ValueError, AssertionError) as exc:
        parser.exit(1, f"validation failed: {exc}\n")


if __name__ == "__main__":
    raise SystemExit(main())
