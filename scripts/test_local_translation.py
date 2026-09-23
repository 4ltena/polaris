"""Offline contract tests for the bundled CAT-Translate helper."""

import contextlib
import hashlib
import importlib.util
import io
import json
import pathlib
import sys
import tempfile
import unittest


SOURCE = pathlib.Path(__file__).with_name("local_translation.py")
SPEC = importlib.util.spec_from_file_location("local_translation", SOURCE)
HELPER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HELPER)


class Tokenizer:
    def apply_chat_template(self, messages, tokenize=False, add_generation_prompt=True):
        self.messages = messages
        return "prompt"

    def __call__(self, prompt, return_tensors="pt", add_special_tokens=False):
        return {"input_ids": Ids(5)}


class Ids:
    def __init__(self, count):
        self.shape = (1, count)


class FakeMps:
    def __init__(self, available=True, synchronize_error=None):
        self.available = available
        self.synchronize_error = synchronize_error
        self.synchronized = False

    def is_available(self):
        return self.available


class Probe:
    def __add__(self, other):
        return self

    def sum(self):
        return self

    def item(self):
        return 2


class FakeTorch:
    bfloat16 = object()

    def __init__(self, available=True, probe_error=None, synchronize_error=None):
        self.backends = type("Backends", (), {"mps": FakeMps(available, synchronize_error)})()
        self.mps = type("Mps", (), {"synchronize": self.synchronize})()
        self.probe_error = probe_error
        self.ones_calls = []

    def ones(self, shape, *, device, dtype):
        self.ones_calls.append((shape, device, dtype))
        if self.probe_error:
            raise self.probe_error
        return Probe()

    def synchronize(self):
        error = self.backends.mps.synchronize_error
        if error:
            raise error
        self.backends.mps.synchronized = True


class Tensor:
    def __init__(self):
        self.devices = []

    def to(self, device):
        self.devices.append(device)
        return self


class LocalTranslationContractTest(unittest.TestCase):
    def test_only_the_pinned_revision_and_two_directions_are_accepted(self):
        def parse(argv):
            saved = sys.argv
            sys.argv = ["local_translation.py", *argv]
            try:
                with contextlib.redirect_stderr(io.StringIO()):
                    return HELPER.parse_args()
            finally:
                sys.argv = saved

        accepted = parse(["--model-path", "/m", "--revision", HELPER.REVISION])
        self.assertEqual(accepted.revision, HELPER.REVISION)
        with self.assertRaises(ValueError):
            parse(["--model-path", "/m", "--revision", "latest"])
        self.assertEqual(HELPER.DIRECTIONS["en_to_ja"], ("English", "Japanese"))
        self.assertEqual(HELPER.DIRECTIONS["ja_to_en"], ("Japanese", "English"))
        with self.assertRaises(ValueError):
            HELPER.request_shape({"direction": "en_to_ja", "text": ""})
        with self.assertRaises(ValueError):
            HELPER.request_shape({"direction": "unknown", "text": "x"})

    def test_prompt_is_direction_specific_and_requires_only_a_translation(self):
        tokenizer = Tokenizer()
        _, count = HELPER.encode(tokenizer, "en_to_ja", "Keep {{SPAN_0}}.")
        self.assertEqual(count, 5)
        joined = "\n".join(item["content"] for item in tokenizer.messages)
        self.assertIn("English", joined)
        self.assertIn("Japanese", joined)
        self.assertIn("{{SPAN_0}}", joined)
        self.assertIn("Return only the translation", joined)

    def test_helper_declares_offline_deterministic_loaders(self):
        source = SOURCE.read_text(encoding="utf-8")
        self.assertIn("local_files_only=True", source)
        self.assertIn("trust_remote_code=False", source)
        self.assertIn("use_fast=False", source)
        self.assertIn("use_safetensors=True", source)
        self.assertIn("do_sample=False", source)
        self.assertIn("num_beams=1", source)
        self.assertNotIn("import subprocess", source)

    def test_manifest_requires_complete_hashed_snapshot(self):
        with tempfile.TemporaryDirectory() as raw:
            root = pathlib.Path(raw)
            for name, content in {
                "config.json": b"{}",
                "tokenizer.model": b"tokenizer",
                "tokenizer_config.json": b"{}",
                "special_tokens_map.json": b"{}",
                "chat_template.jinja": b"{{ messages }}",
                "model.safetensors": b"weights",
            }.items():
                (root / name).write_bytes(content)
            files = {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                     for path in root.iterdir()}
            (root / "manifest.json").write_text(
                json.dumps({"revision": HELPER.REVISION, "files": files}), encoding="utf-8"
            )
            HELPER.verify_manifest(root, HELPER.REVISION)
            (root / "unlisted.json").write_text("{}", encoding="utf-8")
            with self.assertRaises(ValueError):
                HELPER.verify_manifest(root, HELPER.REVISION)

    def test_slow_tokenizer_assets_are_required_without_tokenizer_json(self):
        self.assertIn("tokenizer.model", HELPER.REQUIRED_MODEL_FILES)
        self.assertNotIn("tokenizer.json", HELPER.REQUIRED_MODEL_FILES)
        with tempfile.TemporaryDirectory() as raw:
            root = pathlib.Path(raw)
            for name, content in {
                "config.json": b"{}",
                "tokenizer_config.json": b"{}",
                "special_tokens_map.json": b"{}",
                "chat_template.jinja": b"{{ messages }}",
                "model.safetensors": b"weights",
            }.items():
                (root / name).write_bytes(content)
            files = {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                     for path in root.iterdir()}
            (root / "manifest.json").write_text(
                json.dumps({"revision": HELPER.REVISION, "files": files}), encoding="utf-8"
            )
            with self.assertRaises(ValueError):
                HELPER.verify_manifest(root, HELPER.REVISION)

    def test_device_selection_uses_mps_only_after_bf16_compute_and_sync(self):
        torch = FakeTorch()
        self.assertEqual(HELPER.select_device(torch), "mps")
        self.assertEqual(torch.ones_calls, [((1,), "mps", torch.bfloat16)])
        self.assertTrue(torch.backends.mps.synchronized)

    def test_device_selection_uses_cpu_when_mps_is_unavailable_or_probe_fails(self):
        unavailable = FakeTorch(available=False)
        self.assertEqual(HELPER.select_device(unavailable), "cpu")
        self.assertEqual(unavailable.ones_calls, [])
        self.assertEqual(HELPER.select_device(FakeTorch(probe_error=RuntimeError("bf16"))), "cpu")
        self.assertEqual(
            HELPER.select_device(FakeTorch(synchronize_error=RuntimeError("sync"))), "cpu"
        )

    def test_every_encoded_tensor_moves_to_the_selected_device(self):
        input_ids = Tensor()
        attention_mask = Tensor()
        moved = HELPER.move_encoded(
            {"input_ids": input_ids, "attention_mask": attention_mask}, "mps"
        )
        self.assertIs(moved["input_ids"], input_ids)
        self.assertIs(moved["attention_mask"], attention_mask)
        self.assertEqual(input_ids.devices, ["mps"])
        self.assertEqual(attention_mask.devices, ["mps"])


if __name__ == "__main__":
    unittest.main()
