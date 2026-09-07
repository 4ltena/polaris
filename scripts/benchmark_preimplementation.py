#!/usr/bin/env python3
"""Frozen fixtures and offline checks for pre-implementation planning answers.

This module deliberately has no model, provider, filesystem mutation (apart
from ``freeze``), or network client.  The objective checker verifies a small
answer contract; it never declares that a plan is semantically good.
"""

import argparse
import hashlib
import json
from pathlib import Path


SCHEMA_VERSION = 1
OUTPUT_FIELDS = frozenset({
    "stage", "fact_ids", "constraint_ids", "updated_conditions", "unknowns",
    "assumptions", "dependencies", "requirements", "acceptance_tests",
    "tasks", "work_status", "review_findings", "prose",
})
WORK_STATUS = {
    "implementation": "not_started", "tests": "not_run", "external_actions": "not_started",
}
COMMON_EQUIPMENT = (
    "共通資料: 社内機材貸出サービス。カメラ12台、三脚18台、貸出は1日単位。"
    "利用者は社員で、返却確認は管理者が行う。"
)
OUTPUT_SCHEMA = (
    "以下の全キーを持つ単一JSON objectだけを返す。コードフェンス・追加キーは使わない。"
    "fact_ids・constraint_ids・tasksは表示された全IDを含める。"
    "条件は表示された全IDと状態・値を転記し、unknownの条件だけをunknownsに含める。"
    "要件候補のうち現時点の確認済み条件で必要なものをrequirementsに含め、各要件に最低1件の受入試験を対応させる。"
    "未確定条件に依存する候補は確定済み要件とせずproseに記す。"
    "dependenciesのfromは先行、toは後続タスクを示す。全キーは必要だが該当項目がなければ空配列とする。"
    "stage: string（このターンの段階と同値）、fact_ids/constraint_ids: 重複のないstring配列、"
    "updated_conditions: 表示済み条件IDだけをキーにするobject。各値は {status: 'unknown'|'confirmed', value: JSON値}、"
    "unknowns: {id: 表示済みunknown条件ID, status: 'open'} の重複なし配列、"
    "assumptions: {id: 'A-'で始まるstring, status: 'proposed', text: 非空string} の重複なし配列、"
    "tasks: {id: 表示済みtask ID, text: 非空string} の重複なし配列、"
    "dependencies: {from: task ID, to: task ID} の重複なし配列、"
    "requirements: {id: 表示済みrequirement ID, text: 非空string} の重複なし配列、"
    "acceptance_tests: {id: 非空string, requirement_ids: requirement IDの重複なし配列, "
    "kind: 'positive'|'negative'|'boundary', text: 非空string} の重複なし配列、"
    "work_status: {implementation: 'not_started', tests: 'not_run', external_actions: 'not_started'}、"
    "review_findings: {id: 表示済みissue ID, status: 'resolved'|'remaining'} の重複なし配列、"
    "prose: 20〜2000文字のstring"
)
REQUIREMENT_CATALOG = {
    "R04-1": "社員は機材、貸出日、返却予定日を指定して申請できる。",
    "R04-2": "申請は管理者が承認するまで貸出確定にならない。",
    "R05-1": "同一利用者の同時予約件数は設定上限を超えない。",
    "R06-1": "貸出記録は機材、日付、申請者、状態を保持する。",
    "R06-2": "同一機材・同一日の競合申請は確定させない。",
    "R07-1": "移行作業は業務停止1時間以内で完了できる順序を持つ。",
    "R07-2": "移行前に既存台帳の復元可能なバックアップを作成する。",
}
ISSUE_CATALOG = {
    "I08-1": "申請者への通知が元計画に無かった問題。",
    "I08-2": "通知失敗時の運用が元計画に無かった問題。",
}
DEFAULT_TASKS = [{"id": "discover", "text": "事実と未確定事項を確認する。"},
                 {"id": "design", "text": "確認済み条件から計画を作る。"}]
MIGRATION_TASKS = [{"id": "backup", "text": "既存台帳の復元可能なバックアップを作成する。"},
                   {"id": "migrate", "text": "バックアップ後に新しい台帳へ移行する。"}]


def _canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def _digest(value):
    return hashlib.sha256(_canonical(value)).hexdigest()


def _turn(prompt, facts, constraints, conditions, expectation):
    """Keep evaluator expectations separate from model-visible prompt material."""
    return {
        "prompt": prompt,
        "facts": facts,
        "constraints": constraints,
        "updated_conditions": conditions,
        "expectation": expectation,
    }


def _complete_prompt(stage, task, facts, constraints, conditions, requirements, issues, tasks):
    """Build the exact self-contained model input from visible fixture data."""
    visible_facts = "\n".join("- %s: %s" % (item["id"], item["text"]) for item in facts)
    visible_constraints = "\n".join("- %s: %s" % (item["id"], item["text"]) for item in constraints)
    visible_conditions = "\n".join("- %s: %s = %s（%s）" % (item["id"], item["status"],
                                   json.dumps(item["value"], ensure_ascii=False), item["text"])
                                   for item in conditions)
    visible_requirements = "\n".join("- %s: %s" % (item["id"], item["text"]) for item in requirements) or "- なし"
    visible_issues = "\n".join("- %s: %s" % (item["id"], item["text"]) for item in issues) or "- なし"
    visible_tasks = "\n".join("- %s: %s" % (item["id"], item["text"]) for item in tasks)
    return ("段階: %s\n%s\n\n課題:\n%s\n\n事実:\n%s\n\n制約:\n%s\n\n更新条件:\n%s\n\n"
            "要件候補:\n%s\n\nレビューissue:\n%s\n\n計画タスク:\n%s\n\n"
            "実装、ファイル変更、外部送信は行わない。出力JSONの契約: %s"
            % (stage, COMMON_EQUIPMENT, task, visible_facts, visible_constraints, visible_conditions,
               visible_requirements, visible_issues, visible_tasks, OUTPUT_SCHEMA))


def _case(case_id, title, stage, turns, *, required_requirements=(), required_test_kinds=(), review=None, tasks=None):
    task_definitions = tasks or DEFAULT_TASKS
    visible_requirements = [{"id": identifier, "text": REQUIREMENT_CATALOG[identifier]}
                            for identifier in required_requirements]
    visible_issues = ([{"id": identifier, "text": ISSUE_CATALOG[identifier]} for identifier in ISSUE_CATALOG]
                      if stage == "review" else [])
    for turn in turns:
        # These are model-visible declarations in every turn.  They are not
        # copied from the turn's evaluator expectation.
        turn["requirement_declarations"] = [dict(item) for item in visible_requirements]
        turn["issue_declarations"] = [dict(item) for item in visible_issues]
        turn["task_definitions"] = [dict(item) for item in task_definitions]
        turn["prompt"] = _complete_prompt(stage, turn["prompt"], turn["facts"], turn["constraints"],
                                            turn["updated_conditions"], turn["requirement_declarations"],
                                            turn["issue_declarations"], turn["task_definitions"])
    return {
        "id": case_id,
        "title": title,
        "stage": stage,
        "equipment_scenario": COMMON_EQUIPMENT,
        "workflow": {"common_skills": ["workflow-core"], "stage_skills": [] if stage == "general" else [stage],
                     "tool_count": 0, "shot_count": 0, "classifier_count": 0},
        "output_contract": sorted(OUTPUT_FIELDS),
        "required_requirements": list(required_requirements),
        "required_test_kinds": list(required_test_kinds),
        "requirement_declarations": visible_requirements,
        "issue_declarations": visible_issues,
        "task_definitions": task_definitions,
        "review_contract": review or {},
        "turns": turns,
    }


def build_cases():
    """Return eight deterministic, two-turn source fixtures without execution."""
    cases = [
        _case("PRE01", "課題と未確定事項", "general", [
            _turn("貸出台帳を整える前に、課題・既知事項・未確定事項を整理してください。",
                  [{"id": "F01", "text": "カメラは12台"}, {"id": "F02", "text": "返却確認は管理者"}],
                  [{"id": "C01", "text": "実装や外部送信はしない"}],
                  [{"id": "U01", "status": "unknown", "value": None, "text": "利用予定人数"}],
                  {"unknown_open": ["U01"], "facts": ["F01", "F02"], "constraints": ["C01"]}),
            _turn("利用予定人数は24人と確認できました。残る未確定事項を保ったまま整理を更新してください。",
                  [{"id": "F01", "text": "カメラは12台"}, {"id": "F02", "text": "返却確認は管理者"}],
                  [{"id": "C01", "text": "実装や外部送信はしない"}],
                  [{"id": "U01", "status": "confirmed", "value": 24, "text": "利用予定人数"},
                   {"id": "U02", "status": "unknown", "value": None, "text": "繁忙日の予約数"}],
                  {"confirmed": {"U01": 24}, "unknown_open": ["U02"], "facts": ["F01", "F02"], "constraints": ["C01"]}),
        ]),
        _case("PRE02", "企画と代替案", "brainstorm", [
            _turn("貸出申請を減らす案を複数出し、比較の観点も示してください。",
                  [{"id": "F03", "text": "申請は現在メールで届く"}],
                  [{"id": "C02", "text": "個人の予定は収集しない"}],
                  [{"id": "U03", "status": "unknown", "value": None, "text": "既存社内ポータルの連携可否"}],
                  {"unknown_open": ["U03"], "facts": ["F03"], "constraints": ["C02"]}),
            _turn("外部サービスは利用できないと決まりました。案を修正し、未確認の連携可否は仮定しないでください。",
                  [{"id": "F03", "text": "申請は現在メールで届く"}],
                  [{"id": "C02", "text": "個人の予定は収集しない"}, {"id": "C03", "text": "外部サービスは禁止"}],
                  [{"id": "U03", "status": "unknown", "value": None, "text": "既存社内ポータルの連携可否"}],
                  {"unknown_open": ["U03"], "facts": ["F03"], "constraints": ["C02", "C03"]}),
        ]),
        _case("PRE03", "方式比較と選択根拠", "brainstorm", [
            _turn("予約の記録方式を比較し、選択に必要な根拠を整理してください。",
                  [{"id": "F04", "text": "貸出は1日単位"}],
                  [{"id": "C04", "text": "事務室の端末は共有端末"}],
                  [{"id": "U04", "status": "unknown", "value": None, "text": "端末の常時接続可否"}],
                  {"unknown_open": ["U04"], "facts": ["F04"], "constraints": ["C04"]}),
            _turn("オフライン利用が必須になりました。比較を更新し、接続前提の方式を選定済みと扱わないでください。",
                  [{"id": "F04", "text": "貸出は1日単位"}],
                  [{"id": "C04", "text": "事務室の端末は共有端末"}, {"id": "C05", "text": "オフライン利用が必須"}],
                  [{"id": "U04", "status": "unknown", "value": None, "text": "端末の常時接続可否"}],
                  {"unknown_open": ["U04"], "facts": ["F04"], "constraints": ["C04", "C05"]}),
        ]),
        _case("PRE04", "要件定義と対象外", "specify", [
            _turn("貸出申請の機能要件と対象外を定義してください。",
                  [{"id": "F05", "text": "社員が申請する"}],
                  [{"id": "C06", "text": "在庫の購入管理は対象外"}],
                  [{"id": "U05", "status": "unknown", "value": None, "text": "承認の要否"}],
                  {"requirements": ["R04-1"], "unknown_open": ["U05"], "facts": ["F05"], "constraints": ["C06"]}),
            _turn("管理者承認が必須になりました。要件と対象外を更新してください。",
                  [{"id": "F05", "text": "社員が申請する"}],
                  [{"id": "C06", "text": "在庫の購入管理は対象外"}, {"id": "C07", "text": "管理者承認が必須"}],
                  [{"id": "U05", "status": "confirmed", "value": True, "text": "承認の要否"}],
                  {"requirements": ["R04-1", "R04-2"], "confirmed": {"U05": True}, "facts": ["F05"], "constraints": ["C06", "C07"]}),
        ], required_requirements=("R04-1", "R04-2")),
        _case("PRE05", "受入条件と境界値", "specify", [
            _turn("予約受付の受入条件と正常・異常・境界値の3種類の試験観点を定義してください。",
                  [{"id": "F06", "text": "同じ機材は日ごとに貸し出す"}],
                  [{"id": "C08", "text": "同時予約上限は3件"}],
                  [{"id": "U06", "status": "confirmed", "value": 3, "text": "同時予約上限"}],
                  {"requirements": ["R05-1"], "test_kinds": ["positive", "negative", "boundary"], "facts": ["F06"], "constraints": ["C08"]}),
            _turn("同時予約上限は2件へ変わりました。正・負・境界試験を更新してください。",
                  [{"id": "F06", "text": "同じ機材は日ごとに貸し出す"}],
                  [{"id": "C08", "text": "同時予約上限は2件"}],
                  [{"id": "U06", "status": "confirmed", "value": 2, "text": "同時予約上限"}],
                  {"requirements": ["R05-1"], "test_kinds": ["positive", "negative", "boundary"], "confirmed": {"U06": 2}, "facts": ["F06"], "constraints": ["C08"]}),
        ], required_requirements=("R05-1",), required_test_kinds=("positive", "negative", "boundary")),
        _case("PRE06", "データ設計と整合性", "specify", [
            _turn("貸出記録のデータと整合性ルールを定義してください。",
                  [{"id": "F07", "text": "カメラと三脚は別の機材種別"}],
                  [{"id": "C09", "text": "履歴は削除しない"}],
                  [{"id": "U07", "status": "unknown", "value": None, "text": "重複申請の扱い"}],
                  {"requirements": ["R06-1"], "unknown_open": ["U07"], "facts": ["F07"], "constraints": ["C09"]}),
            _turn("同一機材の同日二重貸出を防ぐ必要があります。整合性ルールと依存関係を更新してください。",
                  [{"id": "F07", "text": "カメラと三脚は別の機材種別"}],
                  [{"id": "C09", "text": "履歴は削除しない"}, {"id": "C10", "text": "同一機材の同日二重貸出を禁止"}],
                  [{"id": "U07", "status": "confirmed", "value": "reject_conflict", "text": "重複申請の扱い"}],
                  {"requirements": ["R06-1", "R06-2"], "confirmed": {"U07": "reject_conflict"}, "facts": ["F07"], "constraints": ["C09", "C10"]}),
        ], required_requirements=("R06-1", "R06-2")),
        _case("PRE07", "実装計画と依存関係", "specify", [
            _turn("貸出台帳の実装計画を、依存関係と確認作業を含めて作ってください。",
                  [{"id": "F08", "text": "既存の表計算台帳がある"}],
                  [{"id": "C11", "text": "業務停止時間は1時間以内"}],
                  [{"id": "U08", "status": "unknown", "value": None, "text": "移行前バックアップの要否"}],
                  {"requirements": ["R07-1"], "unknown_open": ["U08"], "facts": ["F08"], "constraints": ["C11"]}),
            _turn("移行前バックアップが必須になりました。順序を更新し、循環依存を作らないでください。",
                  [{"id": "F08", "text": "既存の表計算台帳がある"}],
                  [{"id": "C11", "text": "業務停止時間は1時間以内"}, {"id": "C12", "text": "移行前バックアップが必須"}],
                  [{"id": "U08", "status": "confirmed", "value": True, "text": "移行前バックアップの要否"}],
                  {"requirements": ["R07-1", "R07-2"], "confirmed": {"U08": True}, "facts": ["F08"], "constraints": ["C11", "C12"]}),
        ], required_requirements=("R07-1", "R07-2"), tasks=MIGRATION_TASKS),
        _case("PRE08", "実装計画のレビュー", "review", [
            _turn("元計画は P08-1『申請フォームを作る』、P08-2『申請後に申請者へ通知する』だけで、通知失敗時の扱いは無い。"
                  "修正版は P08-2 に通知表示を追加したが、失敗時の表示や運用はまだ無い。元計画と修正版を比較してレビューしてください。",
                  [{"id": "F09", "text": "修正版には申請者通知が含まれる"}],
                  [{"id": "C13", "text": "未実装の確認を完了と書かない"}],
                  [{"id": "U09", "status": "unknown", "value": None, "text": "通知失敗時の再送方針"}],
                  {"unknown_open": ["U09"], "facts": ["F09"], "constraints": ["C13"], "review": {"resolved": ["I08-1"], "remaining": ["I08-2"]}}),
            _turn("元計画と前回の修正版に加え、再修正版 P08-3『通知失敗時は管理者に失敗を表示する』が承認された。"
                  "ただし P08-3 は実装・試験とも未着手である。計画内容からレビュー状態を更新してください。",
                  [{"id": "F09", "text": "修正版には申請者通知が含まれる"}],
                  [{"id": "C13", "text": "未実装の確認を完了と書かない"}],
                  [{"id": "U09", "status": "confirmed", "value": "show_admin", "text": "通知失敗時の再送方針"}],
                  {"confirmed": {"U09": "show_admin"}, "facts": ["F09"], "constraints": ["C13"], "review": {"resolved": ["I08-1", "I08-2"], "remaining": []}}),
        ], review={"uses_status": True}),
    ]
    validate_cases(cases)
    return cases


def validate_cases(cases):
    if not isinstance(cases, list) or len(cases) != 8:
        raise ValueError("exactly eight cases are required")
    if [case["id"] for case in cases] != ["PRE%02d" % number for number in range(1, 9)]:
        raise ValueError("case ids must be PRE01 through PRE08")
    expected_stages = ["general", "brainstorm", "brainstorm", "specify", "specify", "specify", "specify", "review"]
    if [case["stage"] for case in cases] != expected_stages:
        raise ValueError("stage coverage drifted")
    for case in cases:
        if len(case["turns"]) != 2 or any("expectation" not in turn for turn in case["turns"]):
            raise ValueError("every case needs exactly two separated expectations")
        if case["workflow"]["tool_count"] or case["workflow"]["shot_count"] or case["workflow"]["classifier_count"]:
            raise ValueError("pre-implementation cases do not authorize tools, shots, or classification")
    return True


def _error(errors, code):
    errors.append(code)


def _as_json_object(answer, errors):
    if not isinstance(answer, (str, bytes, bytearray)):
        _error(errors, "answer_must_be_json_text")
        return None
    def reject_duplicate_keys(pairs):
        value = {}
        for key, item in pairs:
            if key in value:
                raise ValueError("duplicate key")
            value[key] = item
        return value
    try:
        decoded = json.loads(answer, object_pairs_hook=reject_duplicate_keys,
                             parse_constant=lambda _value: (_ for _ in ()).throw(ValueError("non-finite")))
    except (TypeError, ValueError, UnicodeDecodeError):
        _error(errors, "invalid_json")
        return None
    if not isinstance(decoded, dict):
        _error(errors, "answer_must_be_json_object")
        return None
    unknown = sorted(set(decoded) - OUTPUT_FIELDS)
    missing = sorted(OUTPUT_FIELDS - set(decoded))
    if unknown:
        _error(errors, "unknown_fields:" + ",".join(unknown))
    if missing:
        _error(errors, "missing_fields:" + ",".join(missing))
    return decoded


def _id_list(value, label, errors):
    if not isinstance(value, list) or any(not isinstance(item, str) or not item for item in value):
        _error(errors, label + "_must_be_unique_string_list")
        return set()
    if len(value) != len(set(value)):
        _error(errors, label + "_must_be_unique_string_list")
        return set()
    return set(value)


def _unique_objects(items, label, fields, errors):
    if not isinstance(items, list):
        _error(errors, label + "_must_be_list")
        return None
    checked = []
    for item in items:
        if not isinstance(item, dict) or set(item) != set(fields):
            _error(errors, label + "_shape")
            return None
        checked.append(item)
    return checked


def _acyclic(edges, task_ids):
    graph = {}
    seen = set()
    for edge in edges:
        if not isinstance(edge, dict) or set(edge) != {"from", "to"} or not all(isinstance(edge[key], str) and edge[key] for key in edge):
            return False
        if edge["from"] not in task_ids or edge["to"] not in task_ids:
            return False
        signature = (edge["from"], edge["to"])
        if signature in seen:
            return False
        seen.add(signature)
        graph.setdefault(edge["from"], set()).add(edge["to"])
        graph.setdefault(edge["to"], set())
    active, done = set(), set()
    def visit(node):
        if node in active:
            return False
        if node in done:
            return True
        active.add(node)
        valid = all(visit(child) for child in graph[node])
        active.remove(node)
        done.add(node)
        return valid
    return all(visit(node) for node in graph)


def _conditions(value, declared, errors):
    if not isinstance(value, dict):
        _error(errors, "updated_conditions_must_be_object")
        return
    declared_by_id = {item["id"]: item for item in declared}
    if set(value) != set(declared_by_id):
        _error(errors, "unknown_or_missing_updated_condition")
    for condition_id, source in declared_by_id.items():
        item = value.get(condition_id)
        if not isinstance(item, dict) or set(item) != {"status", "value"}:
            _error(errors, "condition_shape:" + condition_id)
        elif not isinstance(item["status"], str) or item["status"] not in {"unknown", "confirmed"}:
            _error(errors, "condition_status:" + condition_id)
        elif item["status"] != source["status"] or _canonical(item["value"]) != _canonical(source["value"]):
            _error(errors, "stale_or_incorrect_condition:" + condition_id)


def _unknowns(value, declared, errors):
    items = _unique_objects(value, "unknowns", ("id", "status"), errors)
    if items is None:
        return
    expected = {item["id"] for item in declared if item["status"] == "unknown"}
    actual = set()
    for item in items:
        if not isinstance(item["id"], str) or item["status"] != "open":
            _error(errors, "unknown_shape")
            continue
        if item["id"] in actual:
            _error(errors, "duplicate_unknown_id:" + item["id"])
        actual.add(item["id"])
    if actual != expected:
        _error(errors, "unknowns_do_not_match_visible_conditions")


def _assumptions(value, condition_ids, errors):
    items = _unique_objects(value, "assumptions", ("id", "status", "text"), errors)
    if items is None:
        return
    seen = set()
    for item in items:
        identifier = item["id"]
        if not isinstance(identifier, str):
            _error(errors, "assumptions_shape")
            continue
        if identifier in condition_ids and item["status"] == "confirmed":
            _error(errors, "unknown_claimed_as_confirmed:" + identifier)
        if not identifier.startswith("A-") or item["status"] != "proposed" or not isinstance(item["text"], str) or not item["text"].strip():
            _error(errors, "assumptions_shape")
        elif identifier in seen:
            _error(errors, "duplicate_assumption_id:" + identifier)
        elif identifier in condition_ids:
            _error(errors, "unknown_claimed_as_confirmed:" + identifier)
        seen.add(identifier)


def _tasks(value, case, errors):
    items = _unique_objects(value, "tasks", ("id", "text"), errors)
    if items is None:
        return set()
    declared = {item["id"] for item in case["task_definitions"]}
    actual = set()
    for item in items:
        identifier = item["id"]
        if not isinstance(identifier, str) or not isinstance(item["text"], str) or not item["text"].strip():
            _error(errors, "tasks_shape")
            continue
        if identifier in actual:
            _error(errors, "duplicate_task_id:" + identifier)
        actual.add(identifier)
    if actual != declared:
        _error(errors, "tasks_do_not_match_declarations")
    return declared


def _coverage(answer, case, expectation, errors):
    required = set(expectation.get("requirements", case["required_requirements"]))
    requirements = answer.get("requirements")
    if not isinstance(requirements, list) or any(not isinstance(item, dict) or set(item) != {"id", "text"} or not isinstance(item["id"], str) or not isinstance(item["text"], str) or not item["text"].strip() for item in requirements):
        _error(errors, "requirements_shape")
        return
    declared_ids = {item["id"] for item in case["requirement_declarations"]}
    requirement_ids, seen = set(), set()
    for item in requirements:
        identifier = item["id"]
        if identifier in seen:
            _error(errors, "duplicate_requirement_id:" + identifier)
        elif identifier not in declared_ids:
            _error(errors, "invented_requirement_id:" + identifier)
        seen.add(identifier)
        requirement_ids.add(identifier)
    for identifier in required:
        if identifier not in requirement_ids:
            _error(errors, "missing_requirement:" + identifier)
    tests = answer.get("acceptance_tests")
    if not isinstance(tests, list) or any(not isinstance(item, dict) or set(item) != {"id", "requirement_ids", "kind", "text"} or not isinstance(item.get("id"), str) or not item["id"] or not isinstance(item.get("kind"), str) or item["kind"] not in {"positive", "negative", "boundary"} or not isinstance(item.get("requirement_ids"), list) or not all(isinstance(identifier, str) for identifier in item["requirement_ids"]) or not isinstance(item.get("text"), str) or not item["text"].strip() for item in tests):
        _error(errors, "acceptance_tests_shape")
        return
    test_ids = set()
    for item in tests:
        if item["id"] in test_ids:
            _error(errors, "duplicate_acceptance_test_id:" + item["id"])
        test_ids.add(item["id"])
        if len(item["requirement_ids"]) != len(set(item["requirement_ids"])) or any(identifier not in requirement_ids for identifier in item["requirement_ids"]):
            _error(errors, "acceptance_test_unknown_or_duplicate_requirement_reference")
    kinds = set(expectation.get("test_kinds", case["required_test_kinds"]))
    for requirement_id in required:
        seen = {item["kind"] for item in tests if requirement_id in item["requirement_ids"]}
        if not seen:
            _error(errors, "missing_acceptance_test:" + requirement_id)
        for kind in kinds:
            if kind not in seen:
                _error(errors, "missing_test_coverage:%s:%s" % (requirement_id, kind))


def _has_path(edges, start, target):
    graph = {}
    for edge in edges:
        graph.setdefault(edge["from"], set()).add(edge["to"])
    pending, seen = [start], set()
    while pending:
        current = pending.pop()
        if current == target:
            return True
        if current not in seen:
            seen.add(current)
            pending.extend(graph.get(current, ()))
    return False


def _review_findings(value, case, expected, errors):
    items = _unique_objects(value, "review_findings", ("id", "status"), errors)
    if items is None:
        return
    declared = {item["id"] for item in case["issue_declarations"]}
    statuses, seen = {}, set()
    for item in items:
        identifier = item["id"]
        if not isinstance(identifier, str) or not isinstance(item["status"], str) or item["status"] not in {"resolved", "remaining"}:
            _error(errors, "review_findings_shape")
            continue
        if identifier in seen:
            _error(errors, "duplicate_review_issue_id:" + identifier)
        if identifier not in declared:
            _error(errors, "invented_review_issue_id:" + identifier)
        seen.add(identifier)
        statuses[identifier] = item["status"]
    for identifier in expected.get("resolved", []):
        if statuses.get(identifier) != "resolved":
            _error(errors, "review_status_not_resolved:" + identifier)
    for identifier in expected.get("remaining", []):
        if statuses.get(identifier) != "remaining":
            _error(errors, "review_status_not_remaining:" + identifier)


def validate_answer(case_id, turn_index, answer):
    """Fail closed on structural or objective-contract errors.

    A passing result means only that checkable obligations are present.  It is
    never a semantic or overall quality verdict.
    """
    errors = []
    cases = {case["id"]: case for case in build_cases()}
    if case_id not in cases:
        return {"objective_pass": False, "errors": ["unknown_case_id"], "semantic_review_required": True}
    if type(turn_index) is not int or turn_index not in (1, 2):
        return {"objective_pass": False, "errors": ["invalid_turn_index"], "semantic_review_required": True}
    case, turn = cases[case_id], cases[case_id]["turns"][turn_index - 1]
    parsed = _as_json_object(answer, errors)
    if parsed is None:
        return {"objective_pass": False, "errors": errors, "semantic_review_required": True}
    if parsed.get("stage") != case["stage"]:
        _error(errors, "stage_mismatch")
    facts = _id_list(parsed.get("fact_ids"), "fact_ids", errors)
    constraints = _id_list(parsed.get("constraint_ids"), "constraint_ids", errors)
    visible_fact_ids = {item["id"] for item in turn["facts"]}
    visible_constraint_ids = {item["id"] for item in turn["constraints"]}
    if not facts <= visible_fact_ids:
        _error(errors, "invented_fact_id")
    if not constraints <= visible_constraint_ids:
        _error(errors, "invented_constraint_id")
    for identifier in turn["expectation"].get("facts", []):
        if identifier not in facts:
            _error(errors, "missing_fact:" + identifier)
    for identifier in turn["expectation"].get("constraints", []):
        if identifier not in constraints:
            _error(errors, "missing_constraint:" + identifier)
    _conditions(parsed.get("updated_conditions"), turn["updated_conditions"], errors)
    _unknowns(parsed.get("unknowns"), turn["updated_conditions"], errors)
    condition_ids = {item["id"] for item in turn["updated_conditions"]}
    _assumptions(parsed.get("assumptions"), condition_ids, errors)
    task_ids = _tasks(parsed.get("tasks"), case, errors)
    if not isinstance(parsed.get("dependencies"), list) or not _acyclic(parsed["dependencies"], task_ids):
        _error(errors, "cyclic_or_invalid_dependencies")
    elif case_id == "PRE07" and turn_index == 2 and not _has_path(parsed["dependencies"], "backup", "migrate"):
        _error(errors, "missing_required_dependency:backup->migrate")
    _coverage(parsed, case, turn["expectation"], errors)
    if parsed.get("work_status") != WORK_STATUS:
        _error(errors, "false_completion_or_invalid_work_status")
    if not isinstance(parsed.get("prose"), str) or not 20 <= len(parsed["prose"].strip()) <= 2000:
        _error(errors, "prose_must_be_natural_nonempty_text")
    _review_findings(parsed.get("review_findings"), case, turn["expectation"].get("review", {}), errors)
    return {"objective_pass": not errors, "errors": errors, "semantic_review_required": True}


def freeze(output):
    """Write immutable case source and its SHA-256 receipt; never run a model."""
    root = Path(output)
    if root.exists():
        raise ValueError("output directory must not already exist")
    cases = build_cases()
    source = {"schema_version": SCHEMA_VERSION, "offline_only": True, "cases": cases}
    digest = _digest(source)
    root.mkdir(parents=True)
    (root / "preimplementation-cases.json").write_bytes(_canonical(source) + b"\n")
    receipt = {"schema_version": SCHEMA_VERSION, "source": "preimplementation-cases.json", "sha256": digest,
               "case_count": len(cases), "physical_requests_authorized": 0}
    (root / "preimplementation-receipt.json").write_bytes(_canonical(receipt) + b"\n")
    return receipt


def main(argv=None):
    parser = argparse.ArgumentParser(description="freeze offline pre-implementation benchmark fixtures")
    parser.add_argument("--output", required=True, help="new directory for source and SHA-256 receipt")
    args = parser.parse_args(argv)
    try:
        receipt = freeze(args.output)
    except (OSError, ValueError) as error:
        parser.error(str(error))
    print(json.dumps(receipt, ensure_ascii=False, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
