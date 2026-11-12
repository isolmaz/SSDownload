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

# Shared extension files, relative to both directories. manifest.json and
# service-worker.js are deliberately absent: Firefox gets its own manifest and
# loads background.scripts instead of a Chromium service worker.
SHARED_FILES = (
    "i18n.js",
    "codes.js",
    "events.js",
    "background.js",
    "relay-background.js",
    "relay-content.js",
    "relay-sha256.js",
    "content.js",
    "page-observer.js",
    "popup.html",
    "popup.js",
    "welcome.html",
    "welcome.js",
)

# Whole directory trees that are identical in both builds.
SHARED_TREES = ("icons", "_locales")


def shared_entries():
    """Yield (relative path, source path) for every file the sync owns."""
    for name in SHARED_FILES:
        yield Path(name), SOURCE_DIR / name
    for tree in SHARED_TREES:
        root = SOURCE_DIR / tree
        if not root.is_dir():
            continue
        for source in sorted(p for p in root.rglob("*") if p.is_file()):
            yield source.relative_to(SOURCE_DIR), source


def stale_entries():
    """Return (missing, differing) relative paths against the target tree."""
    missing = []
    differing = []
    for relative, source in shared_entries():
        target = TARGET_DIR / relative
        if not source.is_file():
            continue
        if not target.is_file():
            missing.append(relative)
            continue
        if source.read_bytes() != target.read_bytes():
            differing.append(relative)
    return missing, differing


def sync():
    """Write every shared file into the Firefox directory."""
    copied = 0
    for relative, source in shared_entries():
        if not source.is_file():
            continue
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
