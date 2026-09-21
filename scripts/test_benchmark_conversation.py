# -*- coding: utf-8 -*-
"""多ターン比較器の、モデルを呼ばない部分を検査する。

**採点器の検査が本体である。** `todo_api_check` は実際にサーバーを起こして
叩くので、参照実装で通ることと、**1箇所ずつ壊した実装で落ちること**の両方を
見る。落ちない採点器は、何も実装しなくても合格を出す。
"""

from __future__ import annotations

import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import benchmark_conversation as bc  # noqa: E402
from benchmark_costs import price_usage  # noqa: E402

REFERENCE_SERVER = r"""import { createServer } from 'node:http';
import { DatabaseSync } from 'node:sqlite';

const db = new DatabaseSync(process.env.DB_PATH);
db.exec('CREATE TABLE IF NOT EXISTS todos (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0)');

const row = (r) => ({ id: r.id, title: r.title, done: r.done === 1 });
const send = (res, status, body) => {
  if (body === undefined) { res.writeHead(status); res.end(); return; }
  const text = JSON.stringify(body);
  res.writeHead(status, { 'Content-Type': 'application/json' });
  res.end(text);
};

const body = (req) => new Promise((resolve) => {
  let raw = '';
  req.on('data', (c) => { raw += c; });
  req.on('end', () => { try { resolve(JSON.parse(raw || '{}')); } catch { resolve({}); } });
});

createServer(async (req, res) => {
  const url = new URL(req.url, 'http://localhost');
  const match = url.pathname.match(/^\/api\/todos\/(\d+)$/);
  if (url.pathname === '/api/todos' && req.method === 'GET') {
    return send(res, 200, db.prepare('SELECT * FROM todos ORDER BY id').all().map(row));
  }
  if (url.pathname === '/api/todos' && req.method === 'POST') {
    const payload = await body(req);
    if (typeof payload.title !== 'string' || payload.title === '') return send(res, 400, { error: 'title required' });
    const info = db.prepare('INSERT INTO todos (title, done) VALUES (?, 0)').run(payload.title);
    return send(res, 201, { id: Number(info.lastInsertRowid), title: payload.title, done: false });
  }
  if (match && req.method === 'PATCH') {
    const id = Number(match[1]);
    const found = db.prepare('SELECT * FROM todos WHERE id = ?').get(id);
    if (!found) return send(res, 404, { error: 'not found' });
    const payload = await body(req);
    db.prepare('UPDATE todos SET done = ? WHERE id = ?').run(payload.done ? 1 : 0, id);
    return send(res, 200, row(db.prepare('SELECT * FROM todos WHERE id = ?').get(id)));
  }
  if (match && req.method === 'DELETE') {
    const id = Number(match[1]);
    const found = db.prepare('SELECT * FROM todos WHERE id = ?').get(id);
    if (!found) return send(res, 404, { error: 'not found' });
    db.prepare('DELETE FROM todos WHERE id = ?').run(id);
    return send(res, 204, undefined);
  }
  return send(res, 404, { error: 'not found' });
}).listen(Number(process.env.PORT));
"""

REFERENCE_APP = """export default function App() {
  return null;
}
"""

# 1箇所だけ壊す。採点器がその項目で落ちることを見る。
MUTANTS = {
    "empty_title_400": ("payload.title === ''", "payload.title === '\\u0000never'"),
    "patch_done_true": ("payload.done ? 1 : 0", "0"),
    "missing_patch_404": ("if (!found) return send(res, 404, { error: 'not found' });\n    const payload = await body(req);",
                          "const payload = await body(req);"),
    "create_201": ("return send(res, 201,", "return send(res, 200,"),
    "persisted": ("new DatabaseSync(process.env.DB_PATH)", "new DatabaseSync(':memory:')"),
}


def plan(keys, marker):
    return {key: (marker if key in ("plan_id", "schema_sql") else [marker]) for key in keys}


def reply_with(payload):
    return "計画です。\n\n" + json.dumps(payload, ensure_ascii=False)


class FixtureTests(unittest.TestCase):
    def test_todo_fixture_isolates_itself_and_empties_agents_md(self):
        with tempfile.TemporaryDirectory() as directory:
            # `benchmark_scale.build_fixture` は解決済みの絶対パスしか受けない
            # （`root.resolve() != root` で止まる）。測定本体も同じ形で渡す。
            root = Path(directory).resolve() / "fx"
            bc.build_fixture("todo", root)
            self.assertTrue((root / ".polaris").is_dir())
            self.assertEqual((root / "AGENTS.md").read_text(encoding="utf-8"), "")
            self.assertIn("node:sqlite", (root / "README.md").read_text(encoding="utf-8"))
            # Reject reuse rather than deleting previous work and evidence.
            for pattern in ("todo", "small"):
                (root / "leftover.txt").write_text("stale", encoding="utf-8")
                with self.assertRaises(FileExistsError):
                    bc.build_fixture(pattern, root)
                self.assertEqual((root / "leftover.txt").read_text(), "stale")

    def test_the_script_is_fixed_and_states_the_stack_the_user_chose(self):
        self.assertEqual(len(bc.TURNS["todo"]), 15)
        self.assertEqual(len(bc.TURNS["small"]), 4)
        self.assertEqual(len(bc.TURNS["analyze"]), 1)
        self.assertIn("sqlite", bc.TURNS["todo"][1])
        self.assertIn("react", bc.TURNS["todo"][1])
        # 却下は11番目の計画のあと、承認より前。
        self.assertIn("却下", bc.TURNS["todo"][11])
        self.assertIn("承認", bc.TURNS["todo"][13])
        # ブレスト中はコードを書かせない。
        for index in range(0, 13):
            self.assertIn("コードは書かないで", bc.TURNS["todo"][index], index)


    def test_polaris_is_given_more_internal_turns_than_the_default(self):
        # The published default of 20 ended the todo implementation turn with
        # `stopped: MaxTurns` and exit code 1. codex has no such cap, so
        # leaving it would score polaris cheap for stopping early rather than
        # for doing the same work. The deviation is deliberate and recorded.
        self.assertGreater(bc.POLARIS_MAX_TURNS, 20)


class RunResetTests(unittest.TestCase):
    def test_a_rerun_is_refused_without_deleting_or_dispatching(self):
        from unittest import mock
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory).resolve()
            fixture = out / "fx"
            fixture.mkdir()
            metrics = out / "s.metrics.jsonl"
            metrics.write_text('{"model":"gpt-6-astra","usage":{"input_tokens":999}}\n',
                               encoding="utf-8")
            (out / "s.data").mkdir()
            (out / "s.data" / "stale").write_text("x", encoding="utf-8")
            boom = subprocess.TimeoutExpired(cmd=["x"], timeout=1)
            with mock.patch.object(bc.subprocess, "run", side_effect=boom) as dispatch:
                with self.assertRaises(FileExistsError):
                    bc.run_polaris(fixture, out, "s", ["one"], "read-only", 1)
                dispatch.assert_not_called()
            self.assertIn('999', metrics.read_text())
            self.assertEqual((out / "s.data" / "stale").read_text(), "x")


class ProtocolTests(unittest.TestCase):
    def test_a_revised_plan_passes_and_an_unrevised_or_thin_one_does_not(self):
        replies = [""] * 15
        replies[10] = reply_with(plan(bc.TODO_PLAN_KEYS_FIRST, "v1"))
        replies[11] = reply_with(plan(bc.TODO_PLAN_KEYS_SECOND, "v2"))
        good = bc.todo_protocol(replies)
        self.assertTrue(good["pass"], good["detail"])

        # 鍵を足さずに送り返した。
        thin = list(replies)
        thin[11] = reply_with(plan(bc.TODO_PLAN_KEYS_FIRST, "v2"))
        self.assertFalse(bc.todo_protocol(thin)["pass"])

        # 鍵はあるが中身が1回目と同じ。直していない。
        same = list(replies)
        same[10] = reply_with(plan(bc.TODO_PLAN_KEYS_SECOND, "v1"))
        same[11] = reply_with(plan(bc.TODO_PLAN_KEYS_SECOND, "v1"))
        result = bc.todo_protocol(same)
        self.assertFalse(result["pass"])
        self.assertFalse(result["detail"]["plan_changed"])

        # そもそも計画のJSONが無い。
        none = list(replies)
        none[10] = "計画は文章で書きました。"
        self.assertFalse(bc.todo_protocol(none)["pass"])


class TimeoutTests(unittest.TestCase):
    """1ターンの時間切れは、その run の失敗であって campaign の終わりではない。

    2026-09-20に `subprocess.TimeoutExpired` を投げ返したまま走らせ、todoの
    実装ターンが1,800秒を越えた時点で repeat 2〜5 が1本も実行されなかった。
    """

    def driver_times_out(self, driver, arm):
        from unittest import mock
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory).resolve()
            fixture = out / "fx"
            fixture.mkdir()
            boom = subprocess.TimeoutExpired(cmd=["x"], timeout=7)
            with mock.patch.object(bc.subprocess, "run", side_effect=boom):
                got = driver(fixture, out, "s", ["one", "two"], "read-only", 7)
            self.assertEqual(got["arm"], arm)
            self.assertEqual(len(got["failures"]), 1)
            self.assertIn("timeout after 7s", got["failures"][0]["stderr_tail"])
            # 2ターン目は送っていない。
            self.assertLessEqual(got["turns_sent"], 1)

    def test_a_timed_out_turn_is_recorded_rather_than_raised(self):
        self.driver_times_out(bc.run_polaris, "polaris")
        self.driver_times_out(bc.run_codex, "codex")

    def test_the_default_turn_budget_exceeds_the_observed_implementation_turn(self):
        # The todo implementation turn ran past 1,800 seconds at --max-turns 60.
        self.assertGreater(bc.TURN_TIMEOUT, 1800)


class UsageTests(unittest.TestCase):
    def test_polaris_usage_sums_rows_and_keeps_missing_separate_from_zero(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "m.jsonl"
            path.write_text("\n".join([
                json.dumps({"model": "gpt-6-astra", "usage": {
                    "input_tokens": 100, "cache_read_tokens": 10,
                    "output_tokens": 5, "reasoning_tokens": 2}}),
                json.dumps({"model": "gpt-6-astra", "usage": {
                    "input_tokens": 200, "output_tokens": 7}}),
                json.dumps({"model": "gpt-6-astra", "usage": None}),
                "{ not json",
            ]) + "\n", encoding="utf-8")
            got = bc.polaris_usage(path)
            self.assertEqual(got["requests"], 3)
            self.assertIsNone(got["input"])
            self.assertIsNone(got["cache_read"])
            self.assertIsNone(got["output"])
            self.assertIsNone(bc.priced(got))
            self.assertEqual(got["known_tokens"]["input"], 300)
            self.assertEqual(got["known_tokens"]["cache_read"], 10)
            self.assertEqual(got["known_tokens"]["output"], 12)
            self.assertGreater(got["missing"], 0)
            self.assertEqual(got["accounting"]["tokens"]["input"], None)
            self.assertEqual(list(got["models"]), ["gpt-6-astra"])
            self.assertEqual(got["models"]["gpt-6-astra"]["requests"], 3)
            self.assertIsNone(got["models"]["gpt-6-astra"]["input"])
            self.assertEqual(got["models"]["gpt-6-astra"]["known_tokens"]["input"], 300)

    def metrics(self, directory, rows):
        path = Path(directory) / "m.jsonl"
        path.write_text("\n".join(json.dumps(
            {"model": m, "effort": "xhigh",
             "usage": {"input_tokens": i, "cache_read_tokens": cr,
                       "cache_write_tokens": 0, "output_tokens": o}})
            for m, i, cr, o in rows) + "\n", encoding="utf-8")
        return path

    def test_missing_usage_is_never_priced_as_zero(self):
        self.assertIsNone(bc.priced(None))
        self.assertIsNone(bc.priced({"requests": 0, "models": {}}))
        with tempfile.TemporaryDirectory() as directory:
            got = bc.polaris_usage(self.metrics(directory, [("gpt-6-astra", 1000, 0, 0)]))
            self.assertEqual(bc.priced(got), "0.010000")

    def test_each_model_is_priced_with_its_own_rate(self):
        # astra は課金入力 $10/M、luna は $0.20/M。50倍ある。合算に1つの
        # 単価をかけると、段の振り分けが効いた run の費用が壊れる。
        from decimal import Decimal
        with tempfile.TemporaryDirectory() as directory:
            got = bc.polaris_usage(self.metrics(directory, [
                ("gpt-6-astra", 100_000, 0, 0), ("gpt-5.6-luna", 100_000, 0, 0)]))
            mixed = Decimal(bc.priced(got))
            self.assertEqual(mixed,
                             price_usage("gpt-6-astra", 100_000, 0, 0, 0)
                             + price_usage("gpt-5.6-luna", 100_000, 0, 0, 0))
            # 合算をastraでかけた額とは違う。違わなければ分けていない。
            self.assertNotEqual(mixed, price_usage("gpt-6-astra", 200_000, 0, 0, 0))

    def test_the_long_context_surcharge_follows_the_request_not_the_total(self):
        # `rates.json` の long_context は入力272,000を超えた**要求**の単価を
        # 倍にする。合計に当てると、1件も超えていない run が超えたことに
        # なる。
        from decimal import Decimal
        with tempfile.TemporaryDirectory() as directory:
            got = bc.polaris_usage(self.metrics(directory, [
                ("gpt-6-astra", 200_000, 0, 0), ("gpt-6-astra", 200_000, 0, 0)]))
            self.assertEqual(Decimal(bc.priced(got)),
                             2 * price_usage("gpt-6-astra", 200_000, 0, 0, 0))
            # 合計400,000で計算すると閾値を越えて2倍になり、一致しない。
            self.assertLess(Decimal(bc.priced(got)),
                            price_usage("gpt-6-astra", 400_000, 0, 0, 0))

    def test_a_model_without_a_rate_yields_no_price_rather_than_a_wrong_one(self):
        # 表に無いモデルの費用は不明とする。既知モデルの単価を流用した
        # 数字は、測定に見えて測定ではない。
        with tempfile.TemporaryDirectory() as directory:
            got = bc.polaris_usage(self.metrics(directory, [("unlisted-test-model", 1000, 0, 0)]))
            self.assertIsNone(bc.priced(got))
            self.assertEqual(got["unpriced_models"], {"unlisted-test-model": 1})
            self.assertIsNone(got["usd_ceiling"])
        with tempfile.TemporaryDirectory() as directory:
            got = bc.polaris_usage(self.metrics(directory, [
                ("gpt-6-astra", 1000, 0, 0), ("jev-latest", 10, 0, 1)]))
            self.assertIsNone(bc.priced(got))

    def test_polaris_usage_keeps_a_per_model_breakdown(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "m.jsonl"
            path.write_text("\n".join([
                json.dumps({"model": "gpt-6-astra", "effort": "xhigh", "usage": {
                    "input_tokens": 100, "cache_read_tokens": 0,
                    "cache_write_tokens": 0, "output_tokens": 5}}),
                json.dumps({"model": "gpt-5.6-luna", "effort": "xhigh", "usage": {
                    "input_tokens": 900, "cache_read_tokens": 0,
                    "cache_write_tokens": 0, "output_tokens": 50}}),
            ]) + "\n", encoding="utf-8")
            got = bc.polaris_usage(path)
            self.assertEqual(sorted(got["models"]), ["gpt-5.6-luna", "gpt-6-astra"])
            self.assertEqual(got["models"]["gpt-5.6-luna"]["input"], 900)
            self.assertEqual(got["models"]["gpt-6-astra"]["input"], 100)
            self.assertEqual(got["input"], 1000)
            self.assertEqual(got["models"]["gpt-6-astra"]["efforts"], ["xhigh"])
            # 合算をastraでかけた額より安い。lunaぶんが安く効いている。
            from decimal import Decimal
            self.assertLess(Decimal(bc.priced(got)),
                            price_usage("gpt-6-astra", 1000, 0, 0, 55))
            self.assertEqual(got["unpriced_models"], {})

    def test_partial_cache_preserves_known_output_cost_without_exact_price(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "m.jsonl"
            path.write_text(json.dumps({"model": "gpt-6-astra", "usage": {
                "input_tokens": 400000, "cache_write_tokens": 0,
                "output_tokens": 10}}) + "\n", encoding="utf-8")
            got = bc.polaris_usage(path)
            self.assertEqual(got["input"], 400000)
            self.assertIsNone(got["cache_read"])
            self.assertIsNone(bc.priced(got))
            self.assertEqual(got["known_usd"], "0.000750")
            self.assertIsNone(got["usd_ceiling"])

    def test_corrupt_and_missing_metrics_never_become_exact_zero(self):
        with tempfile.TemporaryDirectory() as directory:
            path = self.metrics(directory, [("gpt-6-astra", 1000, 0, 0)])
            with path.open("a") as output:
                output.write("broken\n[]\n")
            got = bc.polaris_usage(path)
            self.assertIsNone(got["input"])
            self.assertIsNone(bc.priced(got))
            self.assertEqual(got["known_tokens"]["input"], 1000)
            self.assertEqual(got["known_usd"], "0.010000")
            missing = bc.polaris_usage(Path(directory) / "missing.jsonl")
            self.assertIsNone(missing["input"])
            self.assertIsNone(bc.priced(missing))

    def test_priced_uses_canonical_accounting_instead_of_a_stale_compatibility_price(self):
        self.assertIsNone(bc.priced({"requests": 1, "usd": "1.000000",
                                    "accounting": {"usd": None}}))

    def test_codex_turn_usage_counts_each_turn_and_flags_a_silent_turn(self):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory)
            (out / "s.turn01.jsonl").write_text(json.dumps({
                "type": "turn.completed",
                "usage": {"input_tokens": 10, "cached_input_tokens": 4,
                          "cache_write_input_tokens": 0, "output_tokens": 3,
                          "reasoning_output_tokens": 1}}) + "\n", encoding="utf-8")
            (out / "s.turn02.jsonl").write_text(json.dumps(
                {"type": "item.completed"}) + "\n", encoding="utf-8")
            got = bc.codex_turn_usage(out, "s", 2)
            self.assertEqual((got["requests"], got["input"], got["cache_read"]), (1, 10, 4))
            self.assertEqual(got["missing"], 1)


class CodexPerModelTests(unittest.TestCase):
    """rolloutの帰属は追記順で決まる。混在するモデルへ単価を流用しない。"""

    def rollout(self, path, events):
        path.write_text("\n".join(json.dumps(e) for e in events) + "\n", encoding="utf-8")

    def context(self, model, effort=None):
        return {"type": "turn_context", "payload": {"model": model, "effort": effort}}

    def usage(self, tokens, cumulative=None, requests=1):
        last = {
            "input_tokens": tokens, "cached_input_tokens": 0,
            "cache_write_input_tokens": 0, "output_tokens": 1,
            "reasoning_output_tokens": 0}
        total = dict(last, input_tokens=tokens if cumulative is None else cumulative,
                     output_tokens=requests)
        return {"type": "event_msg", "payload": {"type": "token_count", "info": {
            "last_token_usage": last, "total_token_usage": total}}}

    def test_requests_follow_the_most_recent_declared_model(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            # A resumed thread replays its earlier contexts first; the usage
            # events only appear after the model actually in force.
            self.rollout(path, [self.context("gpt-6-astra"), self.context("gpt-6-astra"),
                                self.context("gpt-5.6-terra", "high"),
                                self.usage(100), self.usage(200, 300, 2)])
            got = bc.codex_usage_by_model([path])
            self.assertEqual(list(got["models"]), ["gpt-5.6-terra"])
            self.assertEqual(got["models"]["gpt-5.6-terra"]["input"], 300)
            self.assertEqual(got["models"]["gpt-5.6-terra"]["efforts"], ["high"])
            self.assertEqual(got["requests"], 2)

    def test_two_models_split_rather_than_merge(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            self.rollout(path, [self.context("gpt-6-astra"), self.usage(10),
                                self.context("gpt-5.6-luna"), self.usage(90, 100, 2)])
            got = bc.codex_usage_by_model([path])
            self.assertEqual(got["models"]["gpt-6-astra"]["input"], 10)
            self.assertEqual(got["models"]["gpt-5.6-luna"]["input"], 90)
            self.assertEqual(got["input"], 100)
            # Both are in the table, so this one does price - and it is
            # cheaper than charging all 100 at astra.
            from decimal import Decimal
            self.assertLess(Decimal(bc.priced(got)),
                            price_usage("gpt-6-astra", 100, 0, 0, 2))

    def test_an_unpriced_model_in_the_rollout_withholds_the_price(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            self.rollout(path, [self.context("gpt-6-astra"), self.usage(10),
                                self.context("unlisted-test-model", "high"), self.usage(90, 100, 2)])
            got = bc.codex_usage_by_model([path])
            self.assertIsNone(bc.priced(got))
            self.assertEqual(got["unpriced_models"], {"unlisted-test-model": 1})
            # 分かっている分は捨てない。astraの1件は要求単位で確定している。
            from decimal import Decimal
            self.assertEqual(Decimal(got["usd_floor"]),
                             price_usage("gpt-6-astra", 10, 0, 0, 1))
            self.assertEqual(got["unpriced_usage"]["input"], 90)
            self.assertIsNone(got["usd_ceiling"])

    def test_known_requests_are_priced_before_summing_across_long_context_threshold(self):
        from decimal import Decimal
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            self.rollout(path, [self.context("gpt-6-astra", "high"),
                                self.usage(200_000), self.usage(200_000, 400_000, 2)])
            got = bc.codex_usage_by_model([path])
            self.assertEqual(Decimal(bc.priced(got)),
                             2 * price_usage("gpt-6-astra", 200_000, 0, 0, 1))
            self.assertLess(Decimal(bc.priced(got)),
                            price_usage("gpt-6-astra", 400_000, 0, 0, 2))

    def test_a_request_before_any_declaration_is_missing_not_assigned(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            self.rollout(path, [self.usage(50), self.context("gpt-6-astra"),
                                self.usage(10, 60, 2)])
            got = bc.codex_usage_by_model([path])
            self.assertEqual(got["missing"], 1)
            self.assertEqual(got["models"]["gpt-6-astra"]["input"], 10)
            self.assertEqual(got["models"]["unknown"]["known_tokens"]["input"], 50)
            self.assertEqual(got["known_tokens"]["input"], 60)
            self.assertIsNone(bc.priced(got))
            self.assertIsNone(got["usd_ceiling"])

    def test_repeated_cumulative_notification_is_counted_once_after_model_switch(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            self.rollout(path, [self.context("gpt-6-astra"), self.usage(100),
                                self.context("gpt-5.6-luna"), self.usage(100),
                                self.usage(100, 200, 2)])
            got = bc.codex_usage_by_model([path])
            self.assertEqual(got["requests"], 2)
            self.assertEqual(got["input"], 200)
            self.assertEqual(got["models"]["gpt-5.6-luna"]["requests"], 1)
            self.assertEqual(len(got["duplicate_notifications"]), 1)
            self.assertEqual(got["accounting"]["usd"], bc.priced(got))

    def test_missing_cache_and_cumulative_gaps_withhold_exact_totals(self):
        partial = self.usage(100)
        del partial["payload"]["info"]["last_token_usage"]["cached_input_tokens"]
        for events in ([partial], [self.usage(100), self.usage(100, 300, 3)]):
            with self.subTest(events=events), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "rollout.jsonl"
                self.rollout(path, [self.context("gpt-6-astra"), *events])
                got = bc.codex_usage_by_model([path])
                self.assertIsNone(got["input"])
                self.assertIsNone(bc.priced(got))
                self.assertGreater(got["known_tokens"]["input"], 0)
                self.assertTrue(got["accounting"]["issues"])

    def test_corrupt_and_missing_rollouts_preserve_known_subtotal(self):
        for corrupt in ("broken\n", "[]\n", '{"type":"event_msg","payload":null}\n'):
            with self.subTest(corrupt=corrupt), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "rollout.jsonl"
                self.rollout(path, [self.context("gpt-6-astra"), self.usage(100)])
                with path.open("a") as output:
                    output.write(corrupt)
                got = bc.codex_usage_by_model([path])
                self.assertIsNone(got["input"])
                self.assertIsNone(bc.priced(got))
                self.assertEqual(got["known_tokens"]["input"], 100)
                self.assertEqual(got["known_usd"], "0.001050")
        with tempfile.TemporaryDirectory() as directory:
            got = bc.codex_usage_by_model([Path(directory) / "missing.jsonl"])
            self.assertIsNone(got["input"])
            self.assertIsNone(bc.priced(got))
            self.assertTrue(got["accounting"]["issues"])


@unittest.skipIf(shutil.which("node") is None, "node が無い")
class ApiScorerTests(unittest.TestCase):
    """採点器そのものを検査する。**壊した実装で落ちなければ採点していない。**"""

    def write(self, root: Path, server: str) -> None:
        root.mkdir(parents=True, exist_ok=True)
        (root / "server.mjs").write_text(server, encoding="utf-8")
        (root / "web").mkdir(exist_ok=True)
        (root / "web" / "App.jsx").write_text(REFERENCE_APP, encoding="utf-8")

    def test_the_reference_implementation_passes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "fx"
            self.write(root, REFERENCE_SERVER)
            got = bc.todo_api_check(root, 20)
            self.assertTrue(got["pass"], got["detail"])
            self.assertTrue(got["detail"]["app_jsx_exists"])

    def test_each_single_break_is_caught_by_its_own_check(self):
        for name, (before, after) in MUTANTS.items():
            with self.subTest(mutant=name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory) / "fx"
                self.assertIn(before, REFERENCE_SERVER, name)
                self.write(root, REFERENCE_SERVER.replace(before, after, 1))
                got = bc.todo_api_check(root, 20)
                self.assertFalse(got["pass"], f"{name} が通ってしまう")
                self.assertIs(got["detail"].get(name), False,
                              f"{name} 以外の項目で落ちている: {got['detail']}")

    def test_a_missing_server_fails_without_hanging(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "fx"
            root.mkdir()
            got = bc.todo_api_check(root, 5)
            self.assertFalse(got["pass"])
            self.assertFalse(got["detail"]["server_exists"])

    def test_a_server_that_never_listens_fails_on_started(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "fx"
            self.write(root, "process.exit(1);\n")
            got = bc.todo_api_check(root, 5)
            self.assertFalse(got["pass"])
            self.assertFalse(got["detail"]["started"])


class OutputGuardTests(unittest.TestCase):
    def test_output_inside_a_repository_is_refused(self):
        script = Path(__file__).with_name("benchmark_conversation.py")
        with tempfile.TemporaryDirectory() as directory:
            inside = Path(directory) / "repo" / "runs"
            (Path(directory) / "repo").mkdir()
            (Path(directory) / "repo" / ".git").mkdir()
            inside.mkdir()
            proc = subprocess.run(
                [sys.executable, str(script), "--arm", "polaris", "--pattern",
                 "analyze", "--output", str(inside)],
                capture_output=True, text=True)
            self.assertEqual(proc.returncode, 2)
            self.assertIn("リポジトリの外", proc.stderr)


if __name__ == "__main__":
    unittest.main()
