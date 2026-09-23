#!/usr/bin/env python3
"""Fixed offline CAT-Translate JSONL helper.

The caller owns process placement, model installation and publication.  This
helper accepts only the CAT-Translate 3.3B snapshot pinned below, never fetches
code or weights, and produces one deterministic translation for each request.
"""

import argparse
import hashlib
import json
import pathlib
import sys
import time


MODEL_NAME = "cyberagent/CAT-Translate-3.3b"
REVISION = "47e382331d005acd54a42cdf088a76aa88788e0c"
MAX_INPUT_BYTES = 32 * 1024
MAX_INPUT_TOKENS = 2_048
MAX_OUTPUT_TOKENS = 1_024
MAX_OUTPUT_BYTES = 16 * 1024
MAX_MANIFEST_FILE_BYTES = 8 * 1024 * 1024 * 1024
DIRECTIONS = {
    "en_to_ja": ("English", "Japanese"),
    "ja_to_en": ("Japanese", "English"),
}
REQUIRED_MODEL_FILES = {
    "config.json",
    "tokenizer.model",
    "tokenizer_config.json",
    "special_tokens_map.json",
    "chat_template.jinja",
}


def parse_args():
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--model-path", required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--preflight", action="store_true")
    args = parser.parse_args()
    if args.revision != REVISION:
        raise ValueError("fixed translation revision is invalid")
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
    if actual != set(files) or not REQUIRED_MODEL_FILES.issubset(files):
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


def request_shape(value):
    if set(value) != {"direction", "text"}:
        raise ValueError("request shape is invalid")
    direction = value["direction"]
    text = value["text"]
    if direction not in DIRECTIONS:
        raise ValueError("translation direction is invalid")
    if not isinstance(text, str) or not text or len(text.encode("utf-8")) > MAX_INPUT_BYTES:
        raise ValueError("request text is invalid")
    return direction, text


def messages_for(direction, text):
    source, target = DIRECTIONS[direction]
    return [
        {
            "role": "system",
            "content": "You are a translation engine. Return only the translation. Preserve every placeholder exactly.",
        },
        {
            "role": "user",
            "content": f"Translate the following {source} text into {target}. Return only the translation.\n\n{text}",
        },
    ]


def encode(tokenizer, direction, text):
    prompt = tokenizer.apply_chat_template(
        messages_for(direction, text), tokenize=False, add_generation_prompt=True
    )
    encoded = tokenizer(prompt, return_tensors="pt", add_special_tokens=False)
    count = int(encoded["input_ids"].shape[1])
    if count > MAX_INPUT_TOKENS:
        raise ValueError("input exceeds the fixed tokenizer limit")
    return encoded, count


def select_device(torch):
    """Use MPS only after a real BF16 compute and synchronization probe."""
    try:
        mps = torch.backends.mps
        if not mps.is_available():
            return "cpu"
        probe = torch.ones((1,), device="mps", dtype=torch.bfloat16)
        _ = (probe + probe).sum().item()
        torch.mps.synchronize()
        return "mps"
    except Exception:
        return "cpu"


def move_encoded(encoded, device):
    return {name: tensor.to(device) for name, tensor in encoded.items()}


def translate(model, tokenizer, torch, device, direction, text):
    encoded, input_tokens = encode(tokenizer, direction, text)
    encoded = move_encoded(encoded, device)
    started = time.monotonic()
    with torch.inference_mode():
        generated = model.generate(
            **encoded,
            do_sample=False,
            num_beams=1,
            max_new_tokens=MAX_OUTPUT_TOKENS,
            use_cache=True,
        )
    output_ids = generated[0][encoded["input_ids"].shape[1]:].to("cpu")
    output_tokens = int(output_ids.shape[0])
    result = tokenizer.decode(output_ids, skip_special_tokens=True).strip()
    if not result or len(result.encode("utf-8")) > MAX_OUTPUT_BYTES:
        raise ValueError("translation output is invalid or exceeds the fixed limit")
    return {
        "text": result,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "finish_reason": "length" if output_tokens >= MAX_OUTPUT_TOKENS else "stop",
        "truncated": output_tokens >= MAX_OUTPUT_TOKENS,
        "model": MODEL_NAME,
        "revision": REVISION,
        "device": device,
        "latency_ms": int((time.monotonic() - started) * 1000),
    }


def main():
    args = parse_args()
    import torch
    from transformers import AutoModelForCausalLM, AutoTokenizer

    verify_manifest(args.model_path, args.revision)
    tokenizer = AutoTokenizer.from_pretrained(
        args.model_path,
        local_files_only=True,
        trust_remote_code=False,
        use_fast=False,
    )
    device = select_device(torch)
    model = AutoModelForCausalLM.from_pretrained(
        args.model_path,
        local_files_only=True,
        trust_remote_code=False,
        use_safetensors=True,
    )
    model = model.to(device)
    model.eval()
    if args.preflight:
        from importlib.metadata import version
        print(json.dumps({
            "model": MODEL_NAME,
            "revision": REVISION,
            "device": device,
            "packages": {name: version(name) for name in
                         ("torch", "transformers", "tokenizers", "safetensors", "sentencepiece")},
        }, separators=(",", ":")), flush=True)
        return
    for line in sys.stdin:
        direction, text = request_shape(json.loads(line))
        print(json.dumps(translate(model, tokenizer, torch, device, direction, text),
                         separators=(",", ":")), flush=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(json.dumps({"error": str(error)}, separators=(",", ":")),
              file=sys.stderr, flush=True)
        raise SystemExit(2)
