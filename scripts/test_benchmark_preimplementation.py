import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parent))
import benchmark_preimplementation as fixture


def valid_answer(case, turn):
    expected = turn["expectation"]
    confirmed = expected.get("confirmed", {})
    return {
        "stage": case["stage"],
        "fact_ids": expected.get("facts", []),
        "constraint_ids": expected.get("constraints", []),
        "updated_conditions": {item["id"]: {"status": item["status"], "value": item["value"]}
                               for item in turn["updated_conditions"]},
        "unknowns": [{"id": item["id"], "status": "open"}
                     for item in turn["updated_conditions"] if item["status"] == "unknown"],
        "assumptions": [],
        "dependencies": ([{"from": "backup", "to": "migrate"}]
                         if {item["id"] for item in case["task_definitions"]} == {"backup", "migrate"}
                         else [{"from": "discover", "to": "design"}]),
        "requirements": [{"id": identifier, "text": "確認可能な要件として記録する。"}
                         for identifier in expected.get("requirements", case["required_requirements"])],
        "acceptance_tests": [
            {"id": "T-%s-%s" % (identifier, kind), "requirement_ids": [identifier], "kind": kind,
             "text": "期待結果を観測して判定する。"}
            for identifier in expected.get("requirements", case["required_requirements"])
            for kind in (expected.get("test_kinds", case["required_test_kinds"]) or ["positive"])
        ],
        "tasks": [dict(item) for item in case["task_definitions"]],
        "work_status": dict(fixture.WORK_STATUS),
        "review_findings": ([{"id": identifier, "status": "resolved"} for identifier in expected.get("review", {}).get("resolved", [])]
                            + [{"id": identifier, "status": "remaining"} for identifier in expected.get("review", {}).get("remaining", [])]),
        "prose": "資料の事実と制約を保ち、未確定事項は確認が終わるまで保留として整理します。",
    }


class PreimplementationFixtureTest(unittest.TestCase):
    def test_eight_two_turn_cases_have_required_stage_distribution(self):
        cases = fixture.build_cases()
        self.assertEqual(len(cases), 8)
        self.assertEqual([case["stage"] for case in cases],
                         ["general", "brainstorm", "brainstorm", "specify", "specify", "specify", "specify", "review"])
        self.assertTrue(all(len(case["turns"]) == 2 for case in cases))
        self.assertTrue(all(case["workflow"]["tool_count"] == 0 for case in cases))
        self.assertTrue(all("段階: " + case["stage"] in turn["prompt"] for case in cases for turn in case["turns"]))
        self.assertTrue(all(fixture.COMMON_EQUIPMENT in turn["prompt"] for case in cases for turn in case["turns"]))
        self.assertTrue(all(fixture.OUTPUT_SCHEMA in turn["prompt"] for case in cases for turn in case["turns"]))
        self.assertTrue(all("expectation" not in turn["prompt"] for case in cases for turn in case["turns"]))
        self.assertEqual(cases[0]["workflow"]["stage_skills"], [])
        self.assertIn("R04-1", cases[3]["turns"][0]["prompt"])
        self.assertIn("I08-1", cases[7]["turns"][0]["prompt"])
        self.assertIn("管理者に失敗を表示", cases[7]["turns"][1]["prompt"])
        self.assertIn("true", cases[3]["turns"][1]["prompt"])
        self.assertNotIn("True", cases[3]["turns"][1]["prompt"])

    def test_positive_objective_contracts_still_require_semantic_review(self):
        for case in fixture.build_cases():
            for index, turn in enumerate(case["turns"], 1):
                result = fixture.validate_answer(case["id"], index, json.dumps(valid_answer(case, turn), ensure_ascii=False))
                self.assertTrue(result["objective_pass"], (case["id"], index, result))
                self.assertEqual(result["errors"], [])
                self.assertTrue(result["semantic_review_required"])

    def test_bad_json_unknown_fields_and_false_completion_fail_closed(self):
        self.assertFalse(fixture.validate_answer("PRE01", 1, "{")["objective_pass"])
        case = fixture.build_cases()[0]
        answer = valid_answer(case, case["turns"][0])
        answer["extra"] = True
        self.assertIn("unknown_fields:extra", fixture.validate_answer("PRE01", 1, json.dumps(answer))["errors"])
        del answer["extra"]
        answer["work_status"]["implementation"] = "complete"
        self.assertIn("false_completion_or_invalid_work_status", fixture.validate_answer("PRE01", 1, json.dumps(answer))["errors"])

    def test_corrections_unknown_assumptions_and_cycles_are_rejected(self):
        case = fixture.build_cases()[4]
        answer = valid_answer(case, case["turns"][1])
        answer["updated_conditions"]["U06"]["value"] = 3
        answer["dependencies"] = [{"from": "backup", "to": "migrate"}, {"from": "migrate", "to": "backup"}]
        result = fixture.validate_answer("PRE05", 2, json.dumps(answer, ensure_ascii=False))
        self.assertIn("stale_or_incorrect_condition:U06", result["errors"])
        self.assertIn("cyclic_or_invalid_dependencies", result["errors"])
        case = fixture.build_cases()[1]
        answer = valid_answer(case, case["turns"][1])
        answer["assumptions"] = [{"id": "U03", "status": "confirmed", "text": "未確認値を確定扱いする"}]
        self.assertIn("unknown_claimed_as_confirmed:U03", fixture.validate_answer("PRE02", 2, json.dumps(answer))["errors"])

    def test_reproduced_parent_regressions_and_malformed_nested_json_fail_safely(self):
        case = fixture.build_cases()[3]
        self.assertIn("R04-1", case["turns"][0]["prompt"])
        case = fixture.build_cases()[1]
        answer = valid_answer(case, case["turns"][1])
        answer["assumptions"] = [{"id": [], "status": "proposed", "text": "不正"}]
        result = fixture.validate_answer("PRE02", 2, json.dumps(answer, ensure_ascii=False))
        self.assertFalse(result["objective_pass"])
        self.assertIn("assumptions_shape", result["errors"])
        case = fixture.build_cases()[6]
        answer = valid_answer(case, case["turns"][1])
        answer["dependencies"] = []
        self.assertIn("missing_required_dependency:backup->migrate",
                      fixture.validate_answer("PRE07", 2, json.dumps(answer, ensure_ascii=False))["errors"])
        case = fixture.build_cases()[0]
        answer = valid_answer(case, case["turns"][0])
        answer["updated_conditions"]["U99"] = {"status": "confirmed", "value": True}
        answer["fact_ids"].append("F99")
        self.assertIn("unknown_or_missing_updated_condition", fixture.validate_answer("PRE01", 1, json.dumps(answer))["errors"])
        self.assertIn("invented_fact_id", fixture.validate_answer("PRE01", 1, json.dumps(answer))["errors"])
        duplicate = '{"stage":"general","stage":"general"}'
        self.assertIn("invalid_json", fixture.validate_answer("PRE01", 1, duplicate)["errors"])
        self.assertIn("invalid_json", fixture.validate_answer("PRE01", 1, '{"n":NaN}')["errors"])

    def test_duplicate_and_dangling_nested_references_fail_closed(self):
        case = fixture.build_cases()[4]
        answer = valid_answer(case, case["turns"][1])
        answer["tasks"].append(dict(answer["tasks"][0]))
        answer["acceptance_tests"][0]["requirement_ids"].append("R05-1")
        answer["review_findings"] = [{"id": ["I08-1"], "status": "resolved"}]
        result = fixture.validate_answer("PRE05", 2, json.dumps(answer, ensure_ascii=False))
        self.assertFalse(result["objective_pass"])
        self.assertIn("duplicate_task_id:discover", result["errors"])
        self.assertIn("acceptance_test_unknown_or_duplicate_requirement_reference", result["errors"])
        self.assertIn("review_findings_shape", result["errors"])

    def test_confirmed_boolean_is_not_replaced_with_integer(self):
        case = fixture.build_cases()[3]
        answer = valid_answer(case, case["turns"][1])
        answer["updated_conditions"]["U05"]["value"] = 1
        self.assertIn("stale_or_incorrect_condition:U05",
                      fixture.validate_answer("PRE04", 2, json.dumps(answer))["errors"])

    def test_nested_status_arrays_fail_safely(self):
        case = fixture.build_cases()[7]
        answer = valid_answer(case, case["turns"][0])
        answer["unknowns"][0]["status"] = ["open"]
        answer["review_findings"][0]["status"] = ["resolved"]
        result = fixture.validate_answer("PRE08", 1, json.dumps(answer, ensure_ascii=False))
        self.assertFalse(result["objective_pass"])
        self.assertIn("unknown_shape", result["errors"])
        self.assertIn("review_findings_shape", result["errors"])

    def test_requirement_test_coverage_and_review_status_are_checked(self):
        case = fixture.build_cases()[3]
        answer = valid_answer(case, case["turns"][1])
        answer["acceptance_tests"] = []
        self.assertIn("missing_acceptance_test:R04-1", fixture.validate_answer("PRE04", 2, json.dumps(answer, ensure_ascii=False))["errors"])
        case = fixture.build_cases()[4]
        answer = valid_answer(case, case["turns"][1])
        answer["acceptance_tests"] = answer["acceptance_tests"][:2]
        self.assertIn("missing_test_coverage:R05-1:boundary", fixture.validate_answer("PRE05", 2, json.dumps(answer, ensure_ascii=False))["errors"])
        case = fixture.build_cases()[7]
        answer = valid_answer(case, case["turns"][1])
        answer["review_findings"] = [{"id": "I08-2", "status": "remaining"}]
        result = fixture.validate_answer("PRE08", 2, json.dumps(answer, ensure_ascii=False))
        self.assertIn("review_status_not_resolved:I08-1", result["errors"])
        self.assertIn("review_status_not_resolved:I08-2", result["errors"])

    def test_freeze_cli_writes_deterministic_source_and_receipt_without_provider(self):
        runner = Path(fixture.__file__)
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "frozen"
            result = subprocess.run([sys.executable, str(runner), "--output", str(output)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            source = json.loads((output / "preimplementation-cases.json").read_text())
            receipt = json.loads((output / "preimplementation-receipt.json").read_text())
            self.assertTrue(source["offline_only"])
            self.assertEqual(receipt["case_count"], 8)
            self.assertEqual(receipt["physical_requests_authorized"], 0)
            self.assertEqual(receipt["sha256"], fixture._digest(source))
            second = subprocess.run([sys.executable, str(runner), "--output", str(output)], capture_output=True, text=True)
            self.assertNotEqual(second.returncode, 0)


if __name__ == "__main__":
    unittest.main()
