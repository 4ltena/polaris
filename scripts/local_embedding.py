#!/usr/bin/env python3
"""Fixed offline JSONL embedding helper for strict10.

The parent owns process placement and an OS sandbox. This helper performs no
network access, subprocess creation, cache download, or dynamic code loading.
"""

import argparse
import hashlib
import json
import math
import pathlib
import sys

MAX_MANIFEST_FILE_BYTES = 8 * 1024 * 1024 * 1024


def parse_args():
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--model-path", required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--dimension", type=int, required=True)
    parser.add_argument("--max-tokens", type=int, required=True)
    parser.add_argument("--preflight", action="store_true")
    args = parser.parse_args()
    if args.dimension != 384 or args.max_tokens != 512 or not args.revision:
        raise ValueError("fixed embedding helper arguments are invalid")
    return args


def verify_manifest(model_path, revision):
    root = pathlib.Path(model_path).resolve(strict=True)
    manifest_path = root / "manifest.json"
    if manifest_path.is_symlink() or not manifest_path.is_file():
        raise ValueError("model manifest is absent or linked")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if set(manifest) != {"revision", "files"} or manifest["revision"] != revision:
        raise ValueError("model manifest revision is invalid")
    files = manifest["files"]
    if not isinstance(files, dict) or not files:
        raise ValueError("model manifest files are invalid")
    actual = set()
    for path in root.rglob("*"):
        if path.is_symlink():
            raise ValueError("model snapshot contains a symlink")
        if path.is_file() and path != manifest_path:
            actual.add(path.relative_to(root).as_posix())
    if actual != set(files) or "config.json" not in files:
        raise ValueError("model manifest must cover the entire snapshot and config")
    for relative, expected in files.items():
        if not isinstance(relative, str) or not isinstance(expected, str) or len(expected) != 64:
            raise ValueError("model manifest entry is invalid")
        relative_path = pathlib.PurePosixPath(relative)
        if relative_path.is_absolute() or ".." in relative_path.parts or relative != relative_path.as_posix():
            raise ValueError("model manifest path is not canonical")
        path = root / relative
        if path.is_symlink() or not path.is_file() or root not in path.resolve().parents:
            raise ValueError("model manifest path is invalid")
        if path.stat().st_size > MAX_MANIFEST_FILE_BYTES:
            raise ValueError("model manifest file exceeds the fixed limit")
        digest = hashlib.sha256()
        with path.open("rb") as handle:
            for block in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(block)
        if digest.hexdigest() != expected.lower():
            raise ValueError("model manifest SHA-256 mismatch")
    if not any(name.endswith(".safetensors") for name in files):
        raise ValueError("model manifest has no safetensors weights")
    if not any("tokenizer" in name for name in files):
        raise ValueError("model manifest has no tokenizer files")


def mean_pool(last_hidden_state, attention_mask):
    mask = attention_mask.unsqueeze(-1).expand(last_hidden_state.size()).float()
    return (last_hidden_state * mask).sum(1) / mask.sum(1).clamp(min=1e-9)


def chunks(tokenizer, text, maximum, kind):
    prefix = "query: " if kind == "query" else "passage: "
    token_ids = tokenizer.encode(text, add_special_tokens=False)
    available = maximum - len(tokenizer.encode(prefix, add_special_tokens=True))
    if available <= 0:
        raise ValueError("tokenizer prefix exceeds the fixed limit")
    if kind == "passage" and len(token_ids) > available:
        raise ValueError("passage exceeds 512 tokenizer tokens; truncation is forbidden")
    if kind == "passage":
        return [prefix + text]
    # Decode each source-token chunk so the next normal tokenizer invocation adds
    # exactly the prefix and special tokens. The hard check below catches drift.
    return [prefix + tokenizer.decode(token_ids[offset : offset + available], skip_special_tokens=True)
            for offset in range(0, len(token_ids), available)] or [prefix]


def embed(model, tokenizer, torch, values, maximum):
    encoded = tokenizer(values, padding=True, truncation=False, return_tensors="pt")
    if encoded["input_ids"].shape[1] > maximum:
        raise ValueError("input exceeds 512 tokenizer tokens; truncation is forbidden")
    with torch.no_grad():
        embeddings = mean_pool(model(**encoded).last_hidden_state, encoded["attention_mask"])
        embeddings = torch.nn.functional.normalize(embeddings, p=2, dim=1)
    result = embeddings.cpu().tolist()
    if any(len(vector) != 384 or not all(math.isfinite(value) for value in vector) for vector in result):
        raise ValueError("model returned an invalid embedding")
    return result


def main():
    args = parse_args()
    import torch
    from transformers import AutoModel, AutoTokenizer

    verify_manifest(args.model_path, args.revision)
    tokenizer = AutoTokenizer.from_pretrained(
        args.model_path, local_files_only=True, trust_remote_code=False
    )
    model = AutoModel.from_pretrained(
        args.model_path, local_files_only=True, trust_remote_code=False, use_safetensors=True
    )
    model.eval()
    if args.preflight:
        from importlib.metadata import version
        if model.config.hidden_size != 384:
            raise ValueError("fixed model dimension is invalid")
        print(json.dumps({"model": "intfloat/multilingual-e5-small", "dimension": 384,
                          "revision": args.revision,
                          "packages": {name: version(name) for name in
                                       ("torch", "transformers", "tokenizers", "safetensors")}},
                         separators=(",", ":")), flush=True)
        return
    for line in sys.stdin:
        request = json.loads(line)
        if set(request) != {"kind", "text"} or request["kind"] not in {"query", "passage"}:
            raise ValueError("request shape is invalid")
        if not isinstance(request["text"], str) or not request["text"] or len(request["text"].encode()) > 65536:
            raise ValueError("request text is invalid")
        texts = chunks(tokenizer, request["text"], args.max_tokens, request["kind"])
        if len(texts) > 32:
            raise ValueError("query requires too many fixed-size chunks")
        tokens = sum(len(tokenizer.encode(text, add_special_tokens=True)) for text in texts)
        print(json.dumps({"vectors": embed(model, tokenizer, torch, texts, args.max_tokens), "input_tokens": tokens}, separators=(",", ":")), flush=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(json.dumps({"error": str(error)}, separators=(",", ":")), file=sys.stderr, flush=True)
        raise SystemExit(2)
