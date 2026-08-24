#!/usr/bin/env python3
"""Validate DSM package templates and built SPKs without installing anything."""

from __future__ import annotations

import argparse
import io
import json
import re
import shutil
import struct
import subprocess
import sys
import tarfile
from pathlib import Path, PurePosixPath

from build_spk import (
    ARCHITECTURES,
    HERE,
    PackageError,
    elf_contract,
    elf_data_contract,
    normalized_versions,
)


PACKAGE = "synology-drive-sync"
REQUIRED_SCRIPTS = {
    "common",
    "preinst",
    "postinst",
    "preuninst",
    "postuninst",
    "preupgrade",
    "postupgrade",
    "start-stop-status",
}
REQUIRED_PAYLOAD = {
    "bin/synology-drive-sync",
    "bin/sdsync-dsm",
    "libexec/sdsync-common",
    "libexec/sdsync-controller",
    "libexec/sdsync-run",
    "share/licenses/synology-drive-sync-LICENSE",
    "share/licenses/musl-COPYRIGHT",
    "share/licenses/THIRD_PARTY_LICENSES.html",
}
REQUIRED_INFO = {
    "package",
    "version",
    "os_min_ver",
    "description",
    "arch",
    "maintainer",
}


class ValidationError(AssertionError):
    pass


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=sorted(ARCHITECTURES))
    parser.add_argument("--binary", type=Path)
    parser.add_argument("spk", nargs="*", type=Path)
    return parser.parse_args()


def safe_members(archive: tarfile.TarFile, label: str) -> dict[str, tarfile.TarInfo]:
    members: dict[str, tarfile.TarInfo] = {}
    for member in archive.getmembers():
        path = PurePosixPath(member.name)
        if path.is_absolute() or ".." in path.parts or member.name in ("", "."):
            raise ValidationError(f"{label} contains unsafe path {member.name!r}")
        if member.issym() or member.islnk() or member.isdev() or member.isfifo():
            raise ValidationError(f"{label} contains unsupported special member {member.name!r}")
        if not member.isfile() and not member.isdir():
            raise ValidationError(f"{label} contains unsupported member type {member.name!r}")
        if member.name in members:
            raise ValidationError(f"{label} contains duplicate member {member.name!r}")
        if member.mode & 0o022:
            raise ValidationError(f"{label} member is group/world writable: {member.name}")
        members[member.name] = member
    return members


def member_bytes(archive: tarfile.TarFile, member: tarfile.TarInfo) -> bytes:
    extracted = archive.extractfile(member)
    if extracted is None:
        raise ValidationError(f"cannot read archive member {member.name}")
    return extracted.read()


def require_regular_mode(
    members: dict[str, tarfile.TarInfo], name: str, mode: int, label: str
) -> None:
    member = members[name]
    if not member.isfile():
        raise ValidationError(f"{label} is not a regular file: {name}")
    actual = member.mode & 0o7777
    if actual != mode:
        raise ValidationError(
            f"{label} has mode {actual:04o}, expected {mode:04o}: {name}"
        )


def filename_info_version(path: Path, arch: str) -> str:
    prefix = f"{PACKAGE}-"
    suffix = f"-{arch}.spk"
    name = path.name
    if not name.startswith(prefix) or not name.endswith(suffix):
        raise ValidationError(
            f"SPK filename must be {PACKAGE}-VERSION-{arch}.spk: {name}"
        )
    release = name[len(prefix) : -len(suffix)]
    normalized_release, dsm_version = normalized_versions(release)
    if release != normalized_release:
        raise ValidationError(f"SPK filename version must not include a leading v: {name}")
    return dsm_version


def parse_info(payload: bytes) -> dict[str, str]:
    values: dict[str, str] = {}
    for line in payload.decode("utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        match = re.fullmatch(r'([A-Za-z0-9_]+)="([^"\r\n]*)"', line)
        if not match:
            raise ValidationError(f"malformed INFO line: {line!r}")
        key, value = match.groups()
        if key in values:
            raise ValidationError(f"duplicate INFO field: {key}")
        values[key] = value
    missing = REQUIRED_INFO - values.keys()
    if missing:
        raise ValidationError(f"INFO is missing fields: {sorted(missing)}")
    return values


def png_dimensions(payload: bytes) -> tuple[int, int]:
    if len(payload) < 24 or payload[:8] != b"\x89PNG\r\n\x1a\n" or payload[12:16] != b"IHDR":
        raise ValidationError("package icon is not a PNG with an IHDR chunk")
    return struct.unpack_from(">II", payload, 16)


def validate_privilege(payload: bytes) -> None:
    model = json.loads(payload)
    if model.get("defaults", {}).get("run-as") != "package":
        raise ValidationError("conf/privilege must default to run-as package")
    forbidden = json.dumps(model)
    if '"root"' in forbidden or "capabilities" in forbidden:
        raise ValidationError("conf/privilege requests root or Linux capabilities")
    actions = {entry.get("action") for entry in model.get("ctrl-script", [])}
    expected = {
        "preinst", "postinst", "preuninst", "postuninst", "preupgrade",
        "postupgrade", "prestart", "start", "prestop", "stop", "status",
    }
    if actions != expected:
        raise ValidationError("conf/privilege does not explicitly cover every lifecycle action")


def validate_source() -> None:
    required_files = [
        HERE / "INFO.template",
        HERE / "conf/privilege",
        HERE / "licenses/musl-COPYRIGHT",
        HERE / "build-spk.sh",
        HERE / "build_spk.py",
        HERE / "package/bin/sdsync-dsm",
        HERE / "package/libexec/sdsync-common",
        HERE / "package/libexec/sdsync-controller",
        HERE / "package/libexec/sdsync-run",
    ] + [HERE / "scripts" / name for name in REQUIRED_SCRIPTS]
    missing = [str(path.relative_to(HERE)) for path in required_files if not path.is_file()]
    if missing:
        raise ValidationError(f"source package is missing files: {missing}")
    validate_privilege((HERE / "conf/privilege").read_bytes())
    template = (HERE / "INFO.template").read_text(encoding="utf-8")
    for token in ("@DSM_VERSION@", "@ARCH@", "@EXTRACT_SIZE_KIB@"):
        if template.count(token) != 1:
            raise ValidationError(f"INFO.template must contain {token} exactly once")
    manager = (HERE / "package/bin/sdsync-dsm").read_text(encoding="utf-8")
    for contract in (
        "configure-profile", "set-password", "set-totp", "doctor", "plan",
        "run", "enable", "disable", "/home/Drive/",
    ):
        if contract not in manager:
            raise ValidationError(f"DSM manager is missing contract {contract!r}")
    forbidden = ("--password-value", "--totp-value", "eval ", ". \"$config_file\"")
    for path in required_files:
        if path.suffix == ".py" or not path.is_file():
            continue
        text = path.read_text(encoding="utf-8")
        for marker in forbidden:
            if marker in text:
                raise ValidationError(f"{path.name} contains forbidden construct {marker!r}")
    shell = shutil.which("sh") if sys.platform != "win32" else None
    if shell:
        shell_files = [HERE / "build-spk.sh"] + [
            HERE / "scripts" / name for name in REQUIRED_SCRIPTS
        ] + [
            HERE / "package/bin/sdsync-dsm",
            HERE / "package/libexec/sdsync-common",
            HERE / "package/libexec/sdsync-controller",
            HERE / "package/libexec/sdsync-run",
        ]
        for path in shell_files:
            result = subprocess.run([shell, "-n", str(path)], capture_output=True, text=True)
            if result.returncode:
                raise ValidationError(f"shell syntax failed for {path}: {result.stderr.strip()}")


def validate_spk(
    path: Path, requested_arch: str | None, expected_binary: bytes | None = None
) -> str:
    if path.is_symlink() or not path.is_file():
        raise ValidationError(f"SPK is not a non-symlink regular file: {path}")
    with tarfile.open(path, "r:*") as outer:
        members = safe_members(outer, path.name)
        required_outer = {
            "INFO", "package.tgz", "PACKAGE_ICON.PNG", "PACKAGE_ICON_256.PNG",
            "LICENSE", "LICENSES/musl-COPYRIGHT",
            "LICENSES/THIRD_PARTY_LICENSES.html", "conf/privilege",
        } | {f"scripts/{name}" for name in REQUIRED_SCRIPTS}
        missing = required_outer - members.keys()
        if missing:
            raise ValidationError(f"{path.name} is missing members: {sorted(missing)}")
        info = parse_info(member_bytes(outer, members["INFO"]))
        info_arch = info["arch"]
        matching_arches = [
            name
            for name, contract in ARCHITECTURES.items()
            if contract.info_value == info_arch
        ]
        if len(matching_arches) != 1:
            raise ValidationError(f"unsupported INFO arch value: {info_arch}")
        arch = matching_arches[0]
        if requested_arch and requested_arch != arch:
            raise ValidationError(
                f"INFO arch {info_arch} resolves to {arch}, not requested {requested_arch}"
            )
        if info["package"] != PACKAGE or info["os_min_ver"] < "7.0-40000":
            raise ValidationError("INFO package identity or DSM minimum is invalid")
        expected_info_version = filename_info_version(path, arch)
        if info["version"] != expected_info_version:
            raise ValidationError(
                f"INFO version {info['version']!r} does not match filename version "
                f"{expected_info_version!r}"
            )
        validate_privilege(member_bytes(outer, members["conf/privilege"]))
        for script in REQUIRED_SCRIPTS:
            require_regular_mode(members, f"scripts/{script}", 0o755, "lifecycle script")
        if png_dimensions(member_bytes(outer, members["PACKAGE_ICON.PNG"])) != (64, 64):
            raise ValidationError("PACKAGE_ICON.PNG must be 64x64")
        if png_dimensions(member_bytes(outer, members["PACKAGE_ICON_256.PNG"])) != (256, 256):
            raise ValidationError("PACKAGE_ICON_256.PNG must be 256x256")
        payload = member_bytes(outer, members["package.tgz"])
    with tarfile.open(fileobj=io.BytesIO(payload), mode="r:gz") as inner:
        inner_members = safe_members(inner, f"{path.name}:package.tgz")
        missing = REQUIRED_PAYLOAD - inner_members.keys()
        if missing:
            raise ValidationError(f"package.tgz is missing members: {sorted(missing)}")
        executables = (
            "bin/synology-drive-sync", "bin/sdsync-dsm", "libexec/sdsync-common",
            "libexec/sdsync-controller", "libexec/sdsync-run",
        )
        for executable in executables:
            require_regular_mode(inner_members, executable, 0o755, "payload executable")
        embedded_binary = member_bytes(inner, inner_members["bin/synology-drive-sync"])
        elf_data_contract(embedded_binary, arch)
        if expected_binary is not None and embedded_binary != expected_binary:
            raise ValidationError("--binary bytes do not match the executable embedded in the SPK")
    return arch


def main() -> int:
    args = arguments()
    validate_source()
    expected_binary = None
    if args.binary:
        if not args.arch:
            raise ValidationError("--binary requires --arch")
        elf_contract(args.binary, args.arch)
        expected_binary = args.binary.read_bytes()
    for path in args.spk:
        arch = validate_spk(path, args.arch, expected_binary)
        print(f"validated {path} ({arch})")
    if not args.spk and not args.binary:
        print("validated DSM package source templates")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, PackageError, ValidationError, tarfile.TarError, json.JSONDecodeError) as error:
        print(f"SPK validation failed: {error}", file=sys.stderr)
        raise SystemExit(1)
