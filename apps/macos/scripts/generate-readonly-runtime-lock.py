#!/usr/bin/env python3
"""Generate the package-trusted lock after a runtime tree and DMG exist.

Release order is deliberate:
  1. build the closed runtime tree;
  2. build a read-only DMG from exactly that tree;
  3. run this script to hash both the tree and final DMG;
  4. copy the generated JSON into Resources before compiling the Rust service.

The script never invents a source or accepts a missing resource/hash.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
from pathlib import Path


MAX_FILES = 100_000
MAX_TREE_BYTES = 16 * 1024 * 1024 * 1024


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def tree_manifest(root: Path) -> dict[str, object]:
    if not root.is_dir() or root.is_symlink():
        raise ValueError("runtime tree must be a physical directory")
    files: list[dict[str, object]] = []
    total = 0
    for directory, names, filenames in os.walk(root, followlinks=False):
        base = Path(directory)
        for name in names:
            path = base / name
            if path.is_symlink() or not path.is_dir():
                raise ValueError(f"runtime tree directory is not physical: {path}")
        for name in filenames:
            path = base / name
            metadata = path.lstat()
            if path.is_symlink() or not stat.S_ISREG(metadata.st_mode):
                raise ValueError(f"runtime tree contains alias or special file: {path}")
            relative = path.relative_to(root).as_posix()
            if not relative or any(part in ("", ".", "..") for part in relative.split("/")):
                raise ValueError(f"invalid runtime path: {relative}")
            total += metadata.st_size
            if total > MAX_TREE_BYTES or len(files) >= MAX_FILES:
                raise ValueError("runtime tree exceeds release bounds")
            files.append({
                "path": relative,
                "size": metadata.st_size,
                "sha256": digest(path),
                "executable": bool(metadata.st_mode & 0o111),
            })
    files.sort(key=lambda item: str(item["path"]))
    if not any(item["path"] == "runtime/bin/python3.11" and item["executable"] for item in files):
        raise ValueError("runtime/bin/python3.11 is missing or not executable")
    return {"schema_version": 1, "files": files}


def canonical(document: dict[str, object]) -> bytes:
    # Matches the Rust schema field order embedded in the service binary.
    return json.dumps(document, separators=(",", ":")).encode()


def merged_document(existing_path: Path | None, entry: dict[str, object]) -> dict[str, object]:
    """Replace one platform/architecture entry without dropping other release images."""
    images: list[dict[str, object]] = []
    if existing_path is not None:
        existing = json.loads(existing_path.read_text(encoding="utf-8"))
        if not isinstance(existing, dict) or existing.get("schema_version") != 2:
            raise ValueError("existing runtime lock must use schema version 2")
        raw_images = existing.get("images")
        if not isinstance(raw_images, list):
            raise ValueError("existing runtime lock images must be a list")
        seen: set[tuple[object, object, object]] = set()
        for image in raw_images:
            if not isinstance(image, dict):
                raise ValueError("existing runtime lock contains a non-object image")
            key = (image.get("id"), image.get("platform"), image.get("architecture"))
            if None in key or key in seen:
                raise ValueError("existing runtime lock contains a duplicate or incomplete image")
            seen.add(key)
            images.append(image)

    replacement = (entry["id"], entry["platform"], entry["architecture"])
    images = [
        image for image in images
        if (image.get("id"), image.get("platform"), image.get("architecture")) != replacement
    ]
    images.append(entry)
    images.sort(key=lambda image: (
        str(image.get("id", "")), str(image.get("platform", "")),
        str(image.get("architecture", "")), str(image.get("version", "")),
    ))
    return {"schema_version": 2, "images": images}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tree-root", type=Path, required=True)
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--architecture", choices=("arm64", "x86_64"), required=True)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--source-url")
    source.add_argument("--bundled-resource")
    parser.add_argument(
        "--merge-existing", type=Path,
        help="preserve other platform/architecture entries from this schema-v2 lock",
    )
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    if not args.version or "/" in args.version or args.version in (".", ".."):
        raise ValueError("invalid version")
    image = args.image
    image_metadata = image.lstat()
    if image.is_symlink() or not stat.S_ISREG(image_metadata.st_mode) or image_metadata.st_size <= 0:
        raise ValueError("image must be a nonempty physical file")
    if args.source_url and not args.source_url.startswith("https://"):
        raise ValueError("source URL must use https")
    if args.bundled_resource and (args.bundled_resource.startswith("/") or ".." in Path(args.bundled_resource).parts):
        raise ValueError("bundled resource must stay inside Resources")

    tree = tree_manifest(args.tree_root)
    tree_hash = hashlib.sha256(canonical(tree)).hexdigest()
    entry: dict[str, object] = {
        "id": "python-e5-cat",
        "version": args.version,
        "platform": "macos",
        "architecture": args.architecture,
        "image_filename": image.name,
        "image_size": image_metadata.st_size,
        "image_sha256": digest(image),
        "tree_manifest_sha256": tree_hash,
        "tree_manifest": tree,
    }
    entry["source_url" if args.source_url else "bundled_resource"] = (
        args.source_url or args.bundled_resource
    )
    document = merged_document(args.merge_existing, entry)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    temporary = args.output.with_name(f".{args.output.name}.tmp-{os.getpid()}")
    temporary.write_bytes(json.dumps(document, indent=2, sort_keys=True).encode() + b"\n")
    os.replace(temporary, args.output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
