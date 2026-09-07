import json
import os
from decimal import Decimal
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parent))
import benchmark_sadalmelik as sadalmelik


def capabilities():
    return {
        "schema": 1,
        "provider": {
            "name": "codex", "model": "gpt-6-astra", "effort": "medium",
            "max_input_tokens": 32000, "max_output_tokens": 4096,
            "hard_output_cap": True, "input_usage_bounded": True,
            "usage_complete": True, "hosted_action_cap": 2,
        },
        "ledger": {
            "persistent": True, "pre_send_reservation": True,
            "max_physical_attempts": 4500, "max_input_tokens": 2000000,
            "max_output_tokens": 200000, "max_embedding_calls": 1600,
            "max_hosted_actions": 20,
            "max_cost_usd": "40.00", "missing_usage_retains_reservation": True,
            "kinds": ["completion", "summary", "embedding", "web_search", "child"],
        },
    }


class SadalmelikPlanTest(unittest.TestCase):
    def test_approved_strata_are_complete_and_pairwise_covered(self):
        trials = sadalmelik.build_trials()
        self.assertEqual(sadalmelik.validate_trials(trials),
                         {"P1": 3, "P2a": 18, "P2b": 24, "P2c": 30, "P2d": 9})
        self.assertEqual(len(trials), 84)
        self.assertEqual(sum(t["turns"] for t in trials if t["phase"] == "P2b"), 864)
        self.assertEqual(next(t["turns"] for t in trials if t["id"] == "P1-web-canary"), 1)
        self.assertEqual({t["turns"] for t in trials}, {1, 12, 36})
        self.assertTrue(all(t["runtime"]["max_primary_requests_per_turn"] == 12
                            for t in trials if t["phase"] == "P2c"))
        self.assertEqual({t["model"] for t in trials}, {"gpt-6-astra"})
        self.assertEqual({t["effort"] for t in trials}, {"medium"})

    def test_freeze_is_deterministic_and_contains_p0_through_p2d(self):
        first, receipt = sadalmelik.frozen_manifests()
        second, second_receipt = sadalmelik.frozen_manifests()
        self.assertEqual(first, second)
        self.assertEqual(receipt, second_receipt)
        self.assertEqual(set(first), {"P0", "P1", "P2a", "P2b", "P2c", "P2d"})
        self.assertTrue(first["P0"]["offline_only"])
        self.assertFalse(first["P0"]["requires_preflight"])
        self.assertTrue(all(m["requires_preflight"] for phase, m in first.items() if phase != "P0"))
        with tempfile.TemporaryDirectory() as directory:
            saved = sadalmelik.freeze(directory)
            self.assertEqual(saved, receipt)
            persisted = json.loads((Path(directory) / "freeze-receipt.json").read_text())
            self.assertEqual(persisted, receipt)
            self.assertEqual(len(list(Path(directory).glob("manifest-*.json"))), 6)

    def test_fixture_preparation_covers_conversation_repair_and_web_contracts(self):
        with tempfile.TemporaryDirectory() as directory:
            receipt = sadalmelik.prepare_fixtures(Path(directory) / "fixtures")
            root = Path(directory) / "fixtures"
            self.assertEqual(receipt["phase_counts"]["P2c"], 30)
            p1_web = json.loads((root / "P1-web-canary" / "case.json").read_text())
            self.assertEqual(p1_web["contract"]["kind"], "web")
            p2a = json.loads((root / "P2a-01-baseline" / "case.json").read_text())
            self.assertEqual(len(p2a["contract"]["case"]["turns"]), 1)
            p2b = json.loads((root / "P2b-correction-resume-baseline-r1" / "case.json").read_text())
            self.assertEqual(len(p2b["contract"]["case"]["turns"]), 36)
            repair = json.loads((root / "P2c-singlefile_repair-baseline-r1" / "case.json").read_text())
            self.assertEqual(repair["contract"]["task"], "singlefile_repair")

    def test_preflight_fails_closed_for_absent_or_incomplete_evidence(self):
        report = sadalmelik.preflight({})
        self.assertEqual(report["status"], "unsupported")
        self.assertEqual(report["permitted_phases"], ["P0"])
        self.assertEqual(report["blocked_phases"], ["P1", "P2a", "P2b", "P2c", "P2d"])
        self.assertIn("provider.max_output_tokens=4096", report["missing_or_mismatched"])
        report = sadalmelik.preflight(capabilities())
        self.assertEqual(report["status"], "unsupported")
        self.assertIn("provider_bound_proof=schema-1 immutable source hashes", report["missing_or_mismatched"])
        proof = {"schema": 1, "sources": sadalmelik.provider_bound_proof(),
                 "normal_request_builder_bound": True, "durable_ledger_bound": True}
        report = sadalmelik.preflight(capabilities(), proof)
        self.assertNotIn("P2a-01-baseline", report["blocked_trials"])
        self.assertIn("P1-web-canary", report["blocked_trials"])
        self.assertIn("P1-strict10", report["blocked_trials"])
        incomplete = capabilities()
        incomplete["ledger"]["missing_usage_retains_reservation"] = False
        self.assertEqual(sadalmelik.preflight(incomplete)["status"], "unsupported")

    def test_reservation_prevents_a_send_that_can_exceed_forty_dollars(self):
        self.assertEqual(sadalmelik.maximum_request_reservation(), Decimal("0.604800"))
        ledger = sadalmelik.ReservationLedger()
        for number in range(66):
            ledger.reserve_maximum("r%d" % number)
        with self.assertRaises(ValueError):
            ledger.reserve_maximum("overflow")
        ledger.retain_missing_usage("r0")
        self.assertEqual(ledger.settled, Decimal("0.604800"))
        self.assertEqual(ledger.reserved, Decimal("39.312000"))
        with self.assertRaises(ValueError):
            ledger.reserve_maximum("still-overflow")

    def test_settlement_releases_only_the_known_unused_remainder(self):
        ledger = sadalmelik.ReservationLedger()
        reserved = ledger.reserve_maximum("known")
        self.assertEqual(ledger.settle("known", Decimal("0.100000")), reserved - Decimal("0.100000"))
        self.assertEqual(ledger.reserved, Decimal("0"))
        self.assertEqual(ledger.settled, Decimal("0.100000"))

    def test_offline_fake_binary_exercises_fixture_and_quality_contracts_end_to_end(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixtures = root / "fixtures"
            sadalmelik.prepare_fixtures(fixtures)
            fake = root / "fake-polaris"
            fake.write_text("#!%s\nimport sys\nsys.path.insert(0, %r)\nfrom benchmark_sadalmelik import main\nraise SystemExit(main(['fake-binary', *sys.argv[1:]]))\n" %
                            (sys.executable, str(Path(sadalmelik.__file__).parent)), encoding="utf-8")
            fake.chmod(0o700)
            for trial_id in ("P1-baseline", "P2b-correction-resume-baseline-r1",
                             "P2c-singlefile_repair-baseline-r1", "P1-web-canary"):
                trial = fixtures / trial_id
                result = sadalmelik.run_fixture(trial / "case.json", trial / "fixture", fake,
                                                 trial / "rows.jsonl")
                self.assertEqual(result["status"], "offline_fixture_passed")


if __name__ == "__main__":
    unittest.main()
