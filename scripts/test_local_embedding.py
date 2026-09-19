"""Offline contract tests for the bundled embedding helper."""

import contextlib
import importlib.util
import hashlib
import io
import json
import pathlib
import sys
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

    def test_only_the_two_pinned_models_are_accepted_and_the_size_must_match(self):
        # The allowlist is the whole of the model policy. A caller that can
        # name any model, or name a real one at the wrong size, can make the
        # helper produce vectors that silently do not belong to the index
        # they are compared against.
        self.assertEqual(
            HELPER.MODELS,
            {
                "intfloat/multilingual-e5-small": 384,
                "intfloat/multilingual-e5-base": 768,
            },
        )

        def parse(argv):
            saved = sys.argv
            sys.argv = ["local_embedding.py", *argv]
            try:
                with contextlib.redirect_stderr(io.StringIO()):
                    return HELPER.parse_args()
            finally:
                sys.argv = saved

        base = ["--model-path", "/m", "--revision", "r", "--max-tokens", "512"]
        for name, dimension in HELPER.MODELS.items():
            args = parse([*base, "--model-name", name, "--dimension", str(dimension)])
            self.assertEqual(args.dimension, dimension)
        for name, dimension in [
            ("intfloat/multilingual-e5-small", 768),   # real model, wrong size
            ("intfloat/multilingual-e5-base", 384),    # the other way round
            ("intfloat/multilingual-e5-large", 1024),  # not on the list
            ("", 384),
        ]:
            with self.assertRaises(ValueError, msg=f"{name}/{dimension}"):
                parse([*base, "--model-name", name, "--dimension", str(dimension)])
        # max_tokens stays fixed; the chunking contract is written against it.
        with self.assertRaises(ValueError):
            parse(["--model-path", "/m", "--revision", "r", "--max-tokens", "256",
                   "--model-name", "intfloat/multilingual-e5-small", "--dimension", "384"])

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
