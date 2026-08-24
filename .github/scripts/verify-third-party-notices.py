#!/usr/bin/env python3
"""Verify that generated notices cover every supported release dependency."""

from __future__ import annotations

import html.parser
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import NoReturn


REQUIRED_TARGETS = {
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "i686-unknown-linux-musl",
    "armv7-unknown-linux-musleabihf",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
}
PACKAGE_LINE = re.compile(r"^([A-Za-z0-9_-]+) v([^\s]+)")


class NoticeParser(html.parser.HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.anchor_texts: list[str] = []
        self.pre_texts: list[str] = []
        self._anchor: list[str] | None = None
        self._pre: list[str] | None = None

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        del attrs
        if tag == "a":
            self._anchor = []
        elif tag == "pre":
            self._pre = []

    def handle_endtag(self, tag: str) -> None:
        if tag == "a" and self._anchor is not None:
            self.anchor_texts.append("".join(self._anchor).strip())
            self._anchor = None
        elif tag == "pre" and self._pre is not None:
            self.pre_texts.append("".join(self._pre))
            self._pre = None

    def handle_data(self, data: str) -> None:
        if self._anchor is not None:
            self._anchor.append(data)
        if self._pre is not None:
            self._pre.append(data)


def fail(message: str) -> NoReturn:
    raise SystemExit(message)


def cargo_tree_packages(target: str) -> set[str]:
    result = subprocess.run(
        [
            "cargo",
            "tree",
            "--locked",
            "--target",
            target,
            "--edges",
            "normal,build",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    packages: set[str] = set()
    for line in result.stdout.splitlines():
        match = PACKAGE_LINE.match(line)
        if match:
            packages.add(f"{match.group(1)} {match.group(2)}")
    return packages


def main() -> None:
    if len(sys.argv) != 3:
        fail("usage: verify-third-party-notices.py LICENSES.json THIRD_PARTY_LICENSES.html")

    with Path("about.toml").open("rb") as config_file:
        config = tomllib.load(config_file)
    configured_targets = set(config.get("targets", []))
    if configured_targets != REQUIRED_TARGETS:
        missing = sorted(REQUIRED_TARGETS - configured_targets)
        extra = sorted(configured_targets - REQUIRED_TARGETS)
        fail(f"about.toml target mismatch; missing={missing}, extra={extra}")
    if config.get("ignore-dev-dependencies") is not True:
        fail("about.toml must exclude development-only dependencies from release notices")

    with Path(sys.argv[1]).open(encoding="utf-8") as json_file:
        report = json.load(json_file)

    noticed_packages: set[str] = set()
    license_texts: list[str] = []
    licenses = report.get("licenses")
    if not isinstance(licenses, list) or not licenses:
        fail("cargo-about report contains no licenses")
    for license_entry in licenses:
        text = license_entry.get("text")
        if not isinstance(text, str) or not text.strip():
            fail("cargo-about report contains an empty license text")
        # HTML parsing normalizes source CRLF/CR line endings to LF.
        license_texts.append(text.replace("\r\n", "\n").replace("\r", "\n"))
        for use in license_entry.get("used_by", []):
            crate = use.get("crate", {})
            name = crate.get("name")
            version = crate.get("version")
            if isinstance(name, str) and isinstance(version, str):
                noticed_packages.add(f"{name} {version}")

    expected_packages: set[str] = set()
    for target in sorted(REQUIRED_TARGETS):
        expected_packages.update(cargo_tree_packages(target))
    missing_packages = sorted(expected_packages - noticed_packages)
    if missing_packages:
        fail("dependencies missing from cargo-about report: " + ", ".join(missing_packages))

    parser = NoticeParser()
    parser.feed(Path(sys.argv[2]).read_text(encoding="utf-8"))
    missing_links = sorted(expected_packages - set(parser.anchor_texts))
    if missing_links:
        fail("dependencies missing from rendered notice: " + ", ".join(missing_links))
    if parser.pre_texts != license_texts:
        fail("rendered license texts differ from the cargo-about report")

    print(
        f"verified {len(expected_packages)} release dependencies across "
        f"{len(REQUIRED_TARGETS)} targets and {len(license_texts)} license blocks"
    )


if __name__ == "__main__":
    main()
