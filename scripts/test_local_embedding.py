"""Offline contract tests for the bundled strict10 embedding helper."""

import importlib.util
import hashlib
import json
import pathlib
import tempfile
import unittest


SOURCE = pathlib.Path(__file__).with_name("local_embedding.py")
SPEC = importlib.util.spec_from_file_location("local_embedding", SOURCE)
HELPER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HELPER)


class Tokenizer:
    def encode(self, value, add_special_tokens=False):
        tokens = value.split()
        return (["<s>"] if add_special_tokens else []) + tokens

    def decode(self, tokens, skip_special_tokens=True):
        return " ".join(token for token in tokens if token != "<s>")


class LocalEmbeddingContractTest(unittest.TestCase):
    def test_query_is_chunked_and_prefixed_without_truncation(self):
        values = HELPER.chunks(Tokenizer(), "a b c d e", 4, "query")
        self.assertEqual(values, ["query: a b", "query: c d", "query: e"])

    def test_passage_over_limit_is_rejected(self):
        with self.assertRaises(ValueError):
            HELPER.chunks(Tokenizer(), "a b c d", 4, "passage")

    def test_helper_declares_offline_safe_loaders(self):
        source = SOURCE.read_text(encoding="utf-8")
        self.assertIn("local_files_only=True", source)
        self.assertIn("trust_remote_code=False", source)
        self.assertIn("use_safetensors=True", source)
        self.assertNotIn("import subprocess", source)

    def test_manifest_requires_revision_tokenizer_and_safetensors_hashes(self):
        with tempfile.TemporaryDirectory() as raw:
            root = pathlib.Path(raw)
            weight = root / "model.safetensors"
            tokenizer = root / "tokenizer.json"
            config = root / "config.json"
            config.write_text("{}")
            weight.write_bytes(b"weights")
            tokenizer.write_bytes(b"tokenizer")
            files = {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                     for path in (weight, tokenizer, config)}
            (root / "manifest.json").write_text(
                json.dumps({"revision": "fixed", "files": files}), encoding="utf-8"
            )
            HELPER.verify_manifest(root, "fixed")
            extra = root / "unlisted.json"
            extra.write_text("{}")
            with self.assertRaises(ValueError):
                HELPER.verify_manifest(root, "fixed")
            extra.unlink()
            files["model.safetensors"] = "0" * 64
            (root / "manifest.json").write_text(
                json.dumps({"revision": "fixed", "files": files}), encoding="utf-8"
            )
            with self.assertRaises(ValueError):
                HELPER.verify_manifest(root, "fixed")


if __name__ == "__main__":
    unittest.main()
