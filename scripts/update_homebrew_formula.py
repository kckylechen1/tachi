#!/usr/bin/env python3

from __future__ import annotations

import argparse
import hashlib
import pathlib
import re
import sys
import urllib.request


DEFAULT_REPO = "kckylechen1/tachi"
DEFAULT_FORMULA_CLASS = "Tachi"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Update a Homebrew formula to the current Tachi release tarball and sha256."
    )
    parser.add_argument("formula", help="Path to the formula file to update")
    parser.add_argument(
        "--version", required=True, help="Version without leading v, e.g. 0.12.2"
    )
    parser.add_argument(
        "--repo",
        default=DEFAULT_REPO,
        help=f"GitHub repository in owner/name form (default: {DEFAULT_REPO})",
    )
    parser.add_argument(
        "--formula-class",
        default=DEFAULT_FORMULA_CLASS,
        help=f"Formula class name used for diagnostics (default: {DEFAULT_FORMULA_CLASS})",
    )
    return parser.parse_args()


def download_sha256(url: str) -> str:
    sha = hashlib.sha256()
    with urllib.request.urlopen(url) as response:
        while True:
            chunk = response.read(1024 * 1024)
            if not chunk:
                break
            sha.update(chunk)
    return sha.hexdigest()


def replace_or_fail(pattern: str, replacement: str, text: str, label: str) -> str:
    updated, count = re.subn(pattern, replacement, text, count=1, flags=re.MULTILINE)
    if count != 1:
        raise ValueError(f"Could not update {label} in formula")
    return updated


def main() -> int:
    args = parse_args()
    formula_path = pathlib.Path(args.formula)
    if not formula_path.exists():
        raise FileNotFoundError(f"Formula not found: {formula_path}")

    version = args.version.removeprefix("v")
    tag = f"v{version}"
    tarball_url = f"https://github.com/{args.repo}/archive/refs/tags/{tag}.tar.gz"
    sha256 = download_sha256(tarball_url)

    original = formula_path.read_text()
    updated = original
    updated = replace_or_fail(
        r'^\s*url ".*"$', f'  url "{tarball_url}"', updated, "url"
    )
    updated = replace_or_fail(
        r'^\s*sha256 ".*"$', f'  sha256 "{sha256}"', updated, "sha256"
    )

    formula_path.write_text(updated)

    print(f"Updated {args.formula_class} formula")
    print(f"  version: {version}")
    print(f"  url: {tarball_url}")
    print(f"  sha256: {sha256}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(1)
