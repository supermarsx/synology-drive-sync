#!/usr/bin/env python3
"""Keep the crates.io package versions in step with the calendar release number.

Calendar release ``YY.N`` is published to crates.io as ``0.YY.N`` (26.66 is 0.26.66), for both
workspace crates: ``synology-drive-sync`` and ``synology-drive-sync-ffi``. The calendar number is
allocated by the release workflow *after* a commit lands on main, so no commit can know it in
advance. Two commands cover that:

``stamp TAG``
    Rewrite every place that names the crate version (both manifests, the exact version the FFI
    crate requires of the root crate, and both ``Cargo.lock`` entries) to ``0.YY.N``. The release
    workflow runs this in its throw-away checkout, immediately before ``cargo publish``.

``check``
    Run by CI on every commit. Fails unless every one of those places agrees on one
    ``0.YY.N`` version, and unless the published metadata and the files copied into the FFI crate (the C header and the
    license) are sound.

``version TAG``
    Print the crate version for a calendar tag without touching anything.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT_CRATE = "synology-drive-sync"
FFI_CRATE = "synology-drive-sync-ffi"
CALENDAR_TAG = re.compile(r"^([0-9]{2})\.([1-9][0-9]*)$")
CRATE_VERSION = re.compile(r"^0\.([0-9]{2})\.([1-9][0-9]*|0)$")
# crates.io limits.
MAX_DESCRIPTION = 200
MAX_KEYWORDS = 5
MAX_KEYWORD_LENGTH = 20

ROOT_MANIFEST = Path("Cargo.toml")
FFI_MANIFEST = Path("crates/synology-drive-sync-ffi/Cargo.toml")
LOCKFILE = Path("Cargo.lock")
HEADER = Path("include/sdsync.h")
FFI_HEADER = Path("crates/synology-drive-sync-ffi/include/sdsync.h")
LICENSE = Path("license.md")
FFI_LICENSE = Path("crates/synology-drive-sync-ffi/license.md")


class VersionError(ValueError):
    """A deterministic failure with a message fit to print."""


def crate_version(tag: str) -> str:
    """Map calendar tag ``YY.N`` to crate version ``0.YY.N``."""
    match = CALENDAR_TAG.fullmatch(tag)
    if match is None:
        raise VersionError(f"not a calendar release tag (expected YY.N, such as 26.66): {tag!r}")
    return f"0.{match.group(1)}.{match.group(2)}"


def _read(path: Path) -> str:
    try:
        # newline="" keeps whatever line endings the working copy has.
        with path.open(encoding="utf-8", newline="") as handle:
            return handle.read()
    except OSError as error:
        raise VersionError(f"cannot read {path}: {error}") from error


def _write(path: Path, text: str) -> None:
    try:
        with path.open("w", encoding="utf-8", newline="") as handle:
            handle.write(text)
    except OSError as error:
        raise VersionError(f"cannot write {path}: {error}") from error


# `version = "..."` as a whole line inside the `[package]` table (the first table in each manifest).
_PACKAGE_VERSION = re.compile(r'(?m)^(version = ")([^"\r\n]*)(")')
# The root crate as the FFI crate requires it: an exact version beside the checkout path.
_FFI_DEPENDENCY = re.compile(
    r'(?m)^(synology-drive-sync = \{ version = "=)([^"\r\n]*)(", path = "\.\./\.\." \})'
)


def _package_version_span(manifest: str, label: str) -> re.Match[str]:
    package_end = manifest.find("\n[", manifest.index("[package]") + 1)
    package = manifest[: package_end if package_end != -1 else len(manifest)]
    matches = list(_PACKAGE_VERSION.finditer(package))
    if len(matches) != 1:
        raise VersionError(f"{label}: expected exactly one [package] version line, found {len(matches)}")
    return matches[0]


def manifest_version(manifest: str, label: str) -> str:
    return _package_version_span(manifest, label).group(2)


def ffi_dependency_version(manifest: str) -> str:
    matches = list(_FFI_DEPENDENCY.finditer(manifest))
    if len(matches) != 1:
        raise VersionError(
            f"{FFI_MANIFEST}: expected exactly one `{ROOT_CRATE} = {{ version = \"=X\", path = ...}}` "
            f"dependency line, found {len(matches)}"
        )
    return matches[0].group(2)


def _lock_entry(lock: str, crate: str) -> re.Match[str]:
    pattern = re.compile(
        rf'(?m)^(\[\[package\]\]\r?\nname = "{re.escape(crate)}"\r?\nversion = ")([^"\r\n]*)(")'
    )
    matches = list(pattern.finditer(lock))
    if len(matches) != 1:
        raise VersionError(f"{LOCKFILE}: expected exactly one [[package]] entry for {crate}, found {len(matches)}")
    return matches[0]


def _replace(text: str, match: re.Match[str], value: str) -> str:
    return text[: match.start(2)] + value + text[match.end(2) :]


def stamp(root: Path, tag: str) -> str:
    version = crate_version(tag)

    root_manifest = _read(root / ROOT_MANIFEST)
    root_manifest = _replace(root_manifest, _package_version_span(root_manifest, str(ROOT_MANIFEST)), version)

    ffi_manifest = _read(root / FFI_MANIFEST)
    ffi_manifest = _replace(ffi_manifest, _package_version_span(ffi_manifest, str(FFI_MANIFEST)), version)
    dependency = list(_FFI_DEPENDENCY.finditer(ffi_manifest))
    ffi_dependency_version(ffi_manifest)  # raises unless there is exactly one
    ffi_manifest = _replace(ffi_manifest, dependency[0], version)

    lock = _read(root / LOCKFILE)
    lock = _replace(lock, _lock_entry(lock, ROOT_CRATE), version)
    lock = _replace(lock, _lock_entry(lock, FFI_CRATE), version)

    _write(root / ROOT_MANIFEST, root_manifest)
    _write(root / FFI_MANIFEST, ffi_manifest)
    _write(root / LOCKFILE, lock)

    check(root)
    return version


def _manifest_string(manifest: str, key: str, label: str) -> str:
    match = re.search(rf'(?m)^{re.escape(key)} = "((?:[^"\\\r\n]|\\.)*)"', manifest)
    if match is None:
        raise VersionError(f"{label}: missing `{key}`")
    return match.group(1)


def _manifest_keywords(manifest: str, label: str) -> list[str]:
    match = re.search(r"(?m)^keywords = \[([^\]]*)\]", manifest)
    if match is None:
        raise VersionError(f"{label}: missing `keywords`")
    return re.findall(r'"([^"]*)"', match.group(1))


def check(root: Path) -> str:
    """Return the agreed crate version, or raise naming every disagreement."""
    root_manifest = _read(root / ROOT_MANIFEST)
    ffi_manifest = _read(root / FFI_MANIFEST)
    lock = _read(root / LOCKFILE)

    seen = {
        f"{ROOT_MANIFEST} [package] version": manifest_version(root_manifest, str(ROOT_MANIFEST)),
        f"{FFI_MANIFEST} [package] version": manifest_version(ffi_manifest, str(FFI_MANIFEST)),
        f"{FFI_MANIFEST} dependency on {ROOT_CRATE}": ffi_dependency_version(ffi_manifest),
        f"{LOCKFILE} {ROOT_CRATE}": _lock_entry(lock, ROOT_CRATE).group(2),
        f"{LOCKFILE} {FFI_CRATE}": _lock_entry(lock, FFI_CRATE).group(2),
    }
    problems: list[str] = []
    distinct = set(seen.values())
    if len(distinct) != 1:
        problems.append(
            "the crate version is not the same everywhere (run `.github/scripts/crate-version.py "
            "stamp YY.N` to rewrite every place):\n"
            + "\n".join(f"  {place}: {value}" for place, value in seen.items())
        )
    for place, value in seen.items():
        if CRATE_VERSION.fullmatch(value) is None:
            problems.append(f"{place} is {value!r}, expected 0.YY.N such as 0.26.66")

    if re.search(r"(?m)^publish = false", ffi_manifest):
        problems.append(f"{FFI_MANIFEST} must not set `publish = false`; the C ABI crate is published")
    if re.search(r"(?m)^publish = false", root_manifest):
        problems.append(f"{ROOT_MANIFEST} must not set `publish = false`")

    for label, manifest in ((str(ROOT_MANIFEST), root_manifest), (str(FFI_MANIFEST), ffi_manifest)):
        description = _manifest_string(manifest, "description", label)
        if not 1 <= len(description) <= MAX_DESCRIPTION:
            problems.append(f"{label}: description is {len(description)} characters, crates.io allows 1-{MAX_DESCRIPTION}")
        keywords = _manifest_keywords(manifest, label)
        if not 1 <= len(keywords) <= MAX_KEYWORDS:
            problems.append(f"{label}: {len(keywords)} keywords, crates.io allows 1-{MAX_KEYWORDS}")
        for keyword in keywords:
            if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_+-]*", keyword) or len(keyword) > MAX_KEYWORD_LENGTH:
                problems.append(f"{label}: keyword {keyword!r} is not a valid crates.io keyword")
        readme = _manifest_string(manifest, "readme", label)
        crate_dir = (root / ROOT_MANIFEST).parent if label == str(ROOT_MANIFEST) else (root / FFI_MANIFEST).parent
        if not (crate_dir / readme).is_file():
            problems.append(f"{label}: readme {readme!r} does not exist beside the manifest")

    # The FFI crate can only ship files inside its own directory, so it carries copies of the
    # header and the license text; each must never differ from the original.
    for original, copy in ((HEADER, FFI_HEADER), (LICENSE, FFI_LICENSE)):
        try:
            if _read(root / original) != _read(root / copy):
                problems.append(f"{copy} differs from {original}; copy it into the FFI crate again")
        except VersionError as error:
            problems.append(str(error))

    if problems:
        raise VersionError("\n".join(problems))
    return next(iter(distinct))


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--root", type=Path, default=Path("."), help="repository root (default: current directory)")
    commands = parser.add_subparsers(dest="command", required=True)
    stamp_parser = commands.add_parser("stamp", help="rewrite the crate version everywhere from a calendar tag")
    stamp_parser.add_argument("tag", help="calendar release tag, YY.N")
    commands.add_parser("check", help="fail unless every place naming the crate version agrees")
    version_parser = commands.add_parser("version", help="print the crate version for a calendar tag")
    version_parser.add_argument("tag", help="calendar release tag, YY.N")
    return parser


def main(argv: list[str] | None = None) -> int:
    arguments = _parser().parse_args(argv)
    try:
        if arguments.command == "stamp":
            version = stamp(arguments.root, arguments.tag)
            print(f"stamped crates {ROOT_CRATE} and {FFI_CRATE} as {version} (calendar release {arguments.tag})")
        elif arguments.command == "check":
            version = check(arguments.root)
            print(f"crate version {version} is consistent across both manifests and Cargo.lock")
        else:
            print(crate_version(arguments.tag))
    except VersionError as error:
        print(f"crate-version: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
