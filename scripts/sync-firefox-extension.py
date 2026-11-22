#!/usr/bin/env python3
"""Copy the extension files shared with the Firefox build.

`browser/chromium` is the source of truth for the extension's shared code.
Firefox loads the same scripts through `background.scripts`, so every shared
file must exist byte for byte in `browser/firefox`; only the manifest and the
service-worker bootstrap differ. This script writes those copies and can
verify, without writing, that they are current.

Usage:
    python scripts/sync-firefox-extension.py
    python scripts/sync-firefox-extension.py --check

The repository root is resolved from this script's path, so the command works
from any current directory, and only the standard library is used.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SOURCE_DIR = REPO_ROOT / "browser" / "chromium"
TARGET_DIR = REPO_ROOT / "browser" / "firefox"

# Every file under browser/chromium is shared except these: Firefox gets its own
# manifest and loads background.scripts instead of a Chromium service worker.
CHROMIUM_ONLY = frozenset({"manifest.json", "service-worker.js"})
FIREFOX_ONLY = frozenset({"manifest.json"})


def shared_entries():
    """Yield (relative path, source path) for every file the sync owns."""
    for source in sorted(p for p in SOURCE_DIR.rglob("*") if p.is_file()):
        relative = source.relative_to(SOURCE_DIR)
        if relative.as_posix() not in CHROMIUM_ONLY:
            yield relative, source


def extra_entries():
    """Return files in the target tree that the sync neither owns nor expects."""
    owned = {relative for relative, _ in shared_entries()}
    return sorted(
        relative
        for relative in (p.relative_to(TARGET_DIR) for p in TARGET_DIR.rglob("*") if p.is_file())
        if relative not in owned and relative.as_posix() not in FIREFOX_ONLY
    )


def stale_entries():
    """Return (missing, differing) relative paths against the target tree."""
    missing = []
    differing = []
    for relative, source in shared_entries():
        target = TARGET_DIR / relative
        if not target.is_file():
            missing.append(relative)
            continue
        if source.read_bytes() != target.read_bytes():
            differing.append(relative)
    return missing, differing


def sync():
    """Write every shared file into the Firefox directory and drop stale ones."""
    if TARGET_DIR.is_dir():
        for relative in extra_entries():
            (TARGET_DIR / relative).unlink()
    copied = 0
    for relative, source in shared_entries():
        target = TARGET_DIR / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(source.read_bytes())
        copied += 1
    return copied


def check():
    """Exit non-zero when browser/firefox is missing or out of date."""
    problems = []
    if not TARGET_DIR.is_dir():
        problems.append(f"{TARGET_DIR} does not exist")
    else:
        missing, differing = stale_entries()
        problems.extend(f"missing: {path.as_posix()}" for path in missing)
        problems.extend(f"out of date: {path.as_posix()}" for path in differing)
        problems.extend(f"not in browser/chromium: {path.as_posix()}" for path in extra_entries())
    if problems:
        print("browser/firefox is not in sync with browser/chromium:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        print("Run: python scripts/sync-firefox-extension.py", file=sys.stderr)
        return 1
    print(f"browser/firefox is in sync ({TARGET_DIR})")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Sync shared extension files from browser/chromium to browser/firefox."
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="verify the copies instead of writing them; exits non-zero on drift",
    )
    args = parser.parse_args(argv)

    if args.check:
        return check()

    if not SOURCE_DIR.is_dir():
        print(f"source directory not found: {SOURCE_DIR}", file=sys.stderr)
        return 1
    copied = sync()
    print(f"copied {copied} shared file(s) to {TARGET_DIR}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
