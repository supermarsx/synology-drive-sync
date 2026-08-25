#!/usr/bin/env python3
"""Builder, archive, and executable DSM lifecycle regression tests."""

from __future__ import annotations

import copy
import io
import json
import os
import re
import select
import signal
import shutil
import socket
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
from pathlib import Path

if os.name == "posix":
    import pty
    import termios


HERE = Path(__file__).resolve().parent
REPOSITORY = HERE.parents[1]
sys.path.insert(0, str(HERE))

import validate_spk  # noqa: E402


ARM_EABI5_HARD_FLOAT = 0x05000400


def fake_elf(
    machine: int,
    *,
    elf_class: int = 2,
    data_encoding: int = 1,
    elf_flags: int = 0,
    interpreter: bool = False,
    needed_library: bool = False,
    no_headers: bool = False,
) -> bytes:
    size = 160
    payload = bytearray(size)
    payload[:16] = b"\x7fELF" + bytes((elf_class, data_encoding, 1)) + b"\0" * 9
    byte_order = ">" if data_encoding == 2 else "<"
    program_count = 0 if no_headers else 1
    if elf_class == 2:
        struct.pack_into(
            f"{byte_order}HHIQQQIHHHHHH",
            payload,
            16,
            2,
            machine,
            1,
            0x400000,
            64,
            0,
            elf_flags,
            64,
            56,
            program_count,
            0,
            0,
            0,
        )
    else:
        struct.pack_into(
            f"{byte_order}HHIIIIIHHHHHH",
            payload,
            16,
            2,
            machine,
            1,
            0x10000,
            52,
            0,
            elf_flags,
            52,
            32,
            program_count,
            0,
            0,
            0,
        )
    if not no_headers:
        kind = 2 if needed_library else 3 if interpreter else 1
        flags = 4 if interpreter or needed_library else 5
        file_offset = 128 if needed_library else 0
        file_size = (16 if elf_class == 2 else 8) if needed_library else size
        if elf_class == 2:
            struct.pack_into(
                f"{byte_order}IIQQQQQQ",
                payload,
                64,
                kind,
                flags,
                file_offset,
                0x400000,
                0,
                file_size,
                file_size,
                4096,
            )
        else:
            struct.pack_into(
                f"{byte_order}IIIIIIII",
                payload,
                52,
                kind,
                file_offset,
                0x10000,
                0,
                file_size,
                file_size,
                flags,
                4096,
            )
        if needed_library:
            struct.pack_into(
                f"{byte_order}{'qQ' if elf_class == 2 else 'iI'}",
                payload,
                file_offset,
                1,
                0,
            )
    return bytes(payload)


def repack_outer(
    source: Path,
    destination: Path,
    *,
    info_payload: bytes | None = None,
    payload_overrides: dict[str, bytes] | None = None,
    mode_overrides: dict[str, int] | None = None,
    type_overrides: dict[str, bytes] | None = None,
    pax_overrides: dict[str, dict[str, str]] | None = None,
    global_pax_headers: dict[str, str] | None = None,
) -> None:
    payload_overrides = payload_overrides or {}
    mode_overrides = mode_overrides or {}
    type_overrides = type_overrides or {}
    pax_overrides = pax_overrides or {}
    with tarfile.open(source, "r:") as original, tarfile.open(
        destination,
        "w",
        format=tarfile.PAX_FORMAT,
        pax_headers=global_pax_headers,
    ) as rebuilt:
        for original_member in original.getmembers():
            member = copy.copy(original_member)
            member.pax_headers = dict(original_member.pax_headers)
            payload = (
                original.extractfile(original_member).read()
                if original_member.isfile()
                else None
            )
            if member.name == "INFO" and info_payload is not None:
                payload = info_payload
                member.size = len(payload)
            if member.name in payload_overrides:
                payload = payload_overrides[member.name]
                member.size = len(payload)
            if member.name in mode_overrides:
                member.mode = mode_overrides[member.name]
            if member.name in type_overrides:
                member.type = type_overrides[member.name]
                member.size = 0
                payload = None
            if member.name in pax_overrides:
                member.pax_headers.update(pax_overrides[member.name])
            rebuilt.addfile(member, io.BytesIO(payload) if payload is not None else None)


def repack_payload_mode(payload: bytes, member_name: str, mode: int) -> bytes:
    rebuilt_payload = io.BytesIO()
    with tarfile.open(fileobj=io.BytesIO(payload), mode="r:gz") as original, tarfile.open(
        fileobj=rebuilt_payload, mode="w:gz", format=tarfile.PAX_FORMAT
    ) as rebuilt:
        for original_member in original.getmembers():
            member = copy.copy(original_member)
            content = (
                original.extractfile(original_member).read()
                if original_member.isfile()
                else None
            )
            if member.name == member_name:
                member.mode = mode
            rebuilt.addfile(member, io.BytesIO(content) if content is not None else None)
    return rebuilt_payload.getvalue()


def repack_payload_pax(
    payload: bytes,
    *,
    pax_overrides: dict[str, dict[str, str]] | None = None,
    global_pax_headers: dict[str, str] | None = None,
) -> bytes:
    pax_overrides = pax_overrides or {}
    rebuilt_payload = io.BytesIO()
    with tarfile.open(fileobj=io.BytesIO(payload), mode="r:gz") as original, tarfile.open(
        fileobj=rebuilt_payload,
        mode="w:gz",
        format=tarfile.PAX_FORMAT,
        pax_headers=global_pax_headers,
    ) as rebuilt:
        for original_member in original.getmembers():
            member = copy.copy(original_member)
            member.pax_headers = dict(original_member.pax_headers)
            content = (
                original.extractfile(original_member).read()
                if original_member.isfile()
                else None
            )
            if member.name in pax_overrides:
                member.pax_headers.update(pax_overrides[member.name])
            rebuilt.addfile(member, io.BytesIO(content) if content is not None else None)
    return rebuilt_payload.getvalue()


def repack_payload_with_extra(
    payload: bytes, member_name: str, member_payload: bytes
) -> bytes:
    rebuilt_payload = io.BytesIO()
    with tarfile.open(fileobj=io.BytesIO(payload), mode="r:gz") as original, tarfile.open(
        fileobj=rebuilt_payload, mode="w:gz", format=tarfile.PAX_FORMAT
    ) as rebuilt:
        for original_member in original.getmembers():
            member = copy.copy(original_member)
            content = (
                original.extractfile(original_member).read()
                if original_member.isfile()
                else None
            )
            rebuilt.addfile(member, io.BytesIO(content) if content is not None else None)
        extra = tarfile.TarInfo(member_name)
        extra.mode = 0o644
        extra.size = len(member_payload)
        extra.mtime = 1700000000
        extra.uid = extra.gid = 0
        extra.uname = extra.gname = "root"
        rebuilt.addfile(extra, io.BytesIO(member_payload))
    return rebuilt_payload.getvalue()


class BuilderTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="sdsync-spk-test-")
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def build(
        self,
        arch: str,
        machine: int,
        output: str = "out",
        *,
        elf_class: int = 2,
        elf_flags: int = 0,
    ) -> Path:
        binary = self.root / f"{arch}.elf"
        binary.write_bytes(
            fake_elf(machine, elf_class=elf_class, elf_flags=elf_flags)
        )
        binary.chmod(0o755)
        api_binary = self.root / f"{arch}.api.elf"
        api_binary.write_bytes(
            fake_elf(machine, elf_class=elf_class, elf_flags=elf_flags)
        )
        api_binary.chmod(0o755)
        self.last_api_binary = api_binary
        destination = self.root / output
        environment = os.environ.copy()
        environment["SOURCE_DATE_EPOCH"] = "1700000000"
        result = subprocess.run(
            [
                sys.executable,
                str(HERE / "build_spk.py"),
                "--binary", str(binary),
                "--api-binary", str(api_binary),
                "--arch", arch,
                "--version", "v1.2.3",
                "--output", str(destination),
            ],
            capture_output=True,
            text=True,
            env=environment,
            timeout=30,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        artifact = destination / f"synology-drive-sync-1.2.3-{arch}.spk"
        self.assertTrue(artifact.is_file())
        return artifact

    def test_builds_and_validates_every_supported_architecture(self) -> None:
        fixtures = (
            ("x86_64", 62, 2, 0, "x86_64"),
            ("i686", 3, 1, 0, "i686"),
            (
                "armv7",
                40,
                1,
                ARM_EABI5_HARD_FLOAT,
                "armv7 armada370 armada375 armada38x armadaxp comcerto2k monaco",
            ),
            ("armv8", 183, 2, 0, "armv8"),
        )
        for arch, machine, elf_class, elf_flags, info_arch in fixtures:
            artifact = self.build(
                arch,
                machine,
                arch,
                elf_class=elf_class,
                elf_flags=elf_flags,
            )
            result = subprocess.run(
                [sys.executable, str(HERE / "validate_spk.py"), "--arch", arch, str(artifact)],
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(f"({arch})", result.stdout)
            with tarfile.open(artifact, "r:") as outer:
                outer_members = {member.name for member in outer.getmembers()}
                info = outer.extractfile("INFO").read()  # type: ignore[union-attr]
                package_payload = outer.extractfile("package.tgz").read()  # type: ignore[union-attr]
                privilege = json.loads(
                    outer.extractfile("conf/privilege").read()  # type: ignore[union-attr]
                )
            self.assertNotIn("conf/resource", outer_members)
            self.assertIn(f'arch="{info_arch}"'.encode(), info)
            self.assertEqual(
                privilege,
                {"defaults": {"run-as": "package"}, "join-groupname": "http"},
            )
            self.assertNotIn("tool", privilege)
            with tarfile.open(fileobj=io.BytesIO(package_payload), mode="r:gz") as package:
                package_members = {member.name for member in package.getmembers()}
                installed_api = package.extractfile("bin/sdsync-dsm-api").read()  # type: ignore[union-attr]
                cgi_api = package.extractfile("ui/api.cgi").read()  # type: ignore[union-attr]
                common = package.extractfile("libexec/sdsync-common").read()  # type: ignore[union-attr]
                ui_config_payload = package.extractfile("ui/config").read()  # type: ignore[union-attr]
                ui_config = json.loads(ui_config_payload)
                ui_entrypoint = validate_spk.validate_ui_config(ui_config_payload)
                self.assertEqual(installed_api, self.last_api_binary.read_bytes())
                self.assertEqual(cgi_api, installed_api)
                self.assertEqual(
                    ui_config[".url"]["com.supermarsx.SynologyDriveSync"]["url"],
                    "/webman/3rdparty/synology-drive-sync/index.html",
                )
                self.assertEqual(ui_entrypoint, "ui/index.html")
                self.assertIn(ui_entrypoint, package_members)
                self.assertTrue(package.getmember(ui_entrypoint).isfile())
                self.assertEqual(package.getmember(ui_entrypoint).mode, 0o644)
                self.assertNotIn(b"/usr/syno/bin/synonotify", common)
                self.assertIn(b"/usr/syno/bin/synodsmnotify", common)
                self.assertNotIn("ui/texts/enu/mails", package_members)
                self.assertEqual(package.getmember("bin/sdsync-dsm-api").mode, 0o755)
                self.assertEqual(package.getmember("ui/api.cgi").mode, 0o755)
                self.assertFalse(
                    any(member.mode & 0o6000 for member in package.getmembers())
                )

    def test_validator_rejects_noncanonical_webman_routes(self) -> None:
        source = json.loads((HERE / "package/ui/config").read_text(encoding="utf-8"))
        cases = {
            "legacy-relative": "3rdparty/synology-drive-sync/index.html",
            "root-without-webman": "/3rdparty/synology-drive-sync/index.html",
            "wrong-package": "/webman/3rdparty/another-package/index.html",
            "traversal": "/webman/3rdparty/synology-drive-sync/../index.html",
        }
        for name, route in cases.items():
            config = copy.deepcopy(source)
            config[".url"]["com.supermarsx.SynologyDriveSync"]["url"] = route
            with self.subTest(name=name), self.assertRaisesRegex(
                validate_spk.ValidationError, "canonical DSM Webman entry point"
            ):
                validate_spk.validate_ui_config(json.dumps(config).encode("utf-8"))

    def test_builder_omits_and_validator_rejects_reserved_resource_manifest(self) -> None:
        artifact = self.build("x86_64", 62)
        with tarfile.open(artifact, "r:") as archive:
            self.assertNotIn("conf/resource", {member.name for member in archive})

        for index, resource_name in enumerate(
            (
                "conf/resource",
                "conf/resource/unexpected.json",
                "./conf/resource",
            )
        ):
            tampered = self.root / f"reserved-resource-{index}" / artifact.name
            tampered.parent.mkdir()
            shutil.copyfile(artifact, tampered)
            resource = b'{"sysnotify":{}}'
            member = tarfile.TarInfo(resource_name)
            member.mode = 0o644
            member.size = len(resource)
            member.mtime = 1700000000
            member.uid = member.gid = 0
            member.uname = member.gname = "root"
            with tarfile.open(tampered, "a") as archive:
                archive.addfile(member, io.BytesIO(resource))

            result = subprocess.run(
                [sys.executable, str(HERE / "validate_spk.py"), str(tampered)],
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("reserved conf/resource", result.stderr)

    def test_validator_rejects_unreviewed_outer_and_inner_members(self) -> None:
        artifact = self.build("x86_64", 62)
        with tarfile.open(artifact, "r:") as archive:
            package_payload = archive.extractfile("package.tgz").read()  # type: ignore[union-attr]

        outer_extra = self.root / "outer-extra" / artifact.name
        outer_extra.parent.mkdir()
        shutil.copyfile(artifact, outer_extra)
        payload = b"unreviewed\n"
        member = tarfile.TarInfo("conf/unreviewed")
        member.mode = 0o644
        member.size = len(payload)
        member.mtime = 1700000000
        member.uid = member.gid = 0
        member.uname = member.gname = "root"
        with tarfile.open(outer_extra, "a") as archive:
            archive.addfile(member, io.BytesIO(payload))

        inner_extra = self.root / "inner-extra" / artifact.name
        inner_extra.parent.mkdir()
        repack_outer(
            artifact,
            inner_extra,
            payload_overrides={
                "package.tgz": repack_payload_with_extra(
                    package_payload, "share/unreviewed", payload
                )
            },
        )

        for candidate, marker in (
            (outer_extra, "unexpected outer members"),
            (inner_extra, "unexpected inner members"),
        ):
            result = subprocess.run(
                [sys.executable, str(HERE / "validate_spk.py"), str(candidate)],
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0, candidate.parent.name)
            self.assertIn(marker, result.stderr, candidate.parent.name)

    def test_armv7_info_has_family_and_required_platform_aliases(self) -> None:
        artifact = self.build(
            "armv7",
            40,
            elf_class=1,
            elf_flags=ARM_EABI5_HARD_FLOAT,
        )
        with tarfile.open(artifact, "r:") as outer:
            info = outer.extractfile("INFO").read()  # type: ignore[union-attr]
        expected = (
            b'arch="armv7 armada370 armada375 armada38x armadaxp '
            b'comcerto2k monaco"'
        )
        self.assertIn(expected, info)

        incomplete = self.root / "incomplete-aliases" / artifact.name
        incomplete.parent.mkdir()
        repack_outer(
            artifact,
            incomplete,
            info_payload=info.replace(expected, b'arch="armv7"'),
        )
        result = subprocess.run(
            [sys.executable, str(HERE / "validate_spk.py"), str(incomplete)],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsupported INFO arch value", result.stderr)

    def test_rejects_wrong_elf_class_and_endianness(self) -> None:
        cases = (
            (
                "i686-elf64",
                fake_elf(3, elf_class=2),
                "i686",
                "expected ELF32",
            ),
            (
                "x86-elf32",
                fake_elf(62, elf_class=1),
                "x86_64",
                "expected ELF64",
            ),
            (
                "i686-big-endian",
                fake_elf(3, elf_class=1, data_encoding=2),
                "i686",
                "expected little-endian",
            ),
        )
        for name, payload, arch, marker in cases:
            result = self.run_rejected_build(name, payload, arch)
            self.assertIn(marker, result.stderr)

    def test_rejects_swapped_armv7_and_i686_machines(self) -> None:
        cases = (
            (
                "arm-in-i686",
                fake_elf(
                    40,
                    elf_class=1,
                    elf_flags=ARM_EABI5_HARD_FLOAT,
                ),
                "i686",
                "expected 3",
            ),
            (
                "i686-in-arm",
                fake_elf(3, elf_class=1),
                "armv7",
                "expected 40",
            ),
        )
        for name, payload, arch, marker in cases:
            result = self.run_rejected_build(name, payload, arch)
            self.assertIn(marker, result.stderr)

    def test_rejects_armv7_wrong_eabi_or_float_abi(self) -> None:
        cases = (
            ("eabi4", 0x04000400, "expected EABI5"),
            ("soft-float", 0x05000200, "expected EABI5 hard-float"),
            ("unspecified-float", 0x05000000, "expected EABI5 hard-float"),
            ("conflicting-float", 0x05000600, "expected EABI5 hard-float"),
        )
        for name, flags, marker in cases:
            result = self.run_rejected_build(
                name,
                fake_elf(40, elf_class=1, elf_flags=flags),
                "armv7",
            )
            self.assertIn(marker, result.stderr)

    def run_rejected_build(
        self, name: str, payload: bytes, arch: str
    ) -> subprocess.CompletedProcess[str]:
        binary = self.root / f"{name}.elf"
        binary.write_bytes(payload)
        expected_machine, expected_class, expected_flags = {
            "x86_64": (62, 2, 0),
            "i686": (3, 1, 0),
            "armv7": (40, 1, ARM_EABI5_HARD_FLOAT),
            "armv8": (183, 2, 0),
        }[arch]
        api_binary = self.root / f"{name}.api.elf"
        api_binary.write_bytes(
            fake_elf(expected_machine, elf_class=expected_class, elf_flags=expected_flags)
        )
        api_binary.chmod(0o755)
        result = subprocess.run(
            [
                sys.executable,
                str(HERE / "build_spk.py"),
                "--binary",
                str(binary),
                "--api-binary",
                str(api_binary),
                "--arch",
                arch,
                "--version",
                "1.0.0",
                "--output",
                str(self.root / f"bad-{name}"),
            ],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        return result

    def test_output_is_reproducible_and_extractsize_is_uncompressed_content(self) -> None:
        first = self.build("x86_64", 62, "one")
        second = self.build("x86_64", 62, "two")
        self.assertEqual(first.read_bytes(), second.read_bytes())
        with tarfile.open(first, "r:") as outer:
            info = outer.extractfile("INFO").read().decode("utf-8")  # type: ignore[union-attr]
            compressed_size = outer.getmember("package.tgz").size
            package_payload = outer.extractfile("package.tgz").read()  # type: ignore[union-attr]
        extract_line = next(line for line in info.splitlines() if line.startswith('extractsize="'))
        extract_kib = int(extract_line.split('"')[1])
        with tarfile.open(fileobj=io.BytesIO(package_payload), mode="r:gz") as package:
            payload_bytes = sum(
                member.size for member in package.getmembers() if member.isfile()
            )
        self.assertEqual(extract_kib, (payload_bytes + 1023) // 1024)
        self.assertGreater(extract_kib * 1024, compressed_size)
        self.assertIn('version="1.2.3-1"', info)

    def test_validator_rejects_malformed_zero_and_inexact_extractsize(self) -> None:
        artifact = self.build("x86_64", 62)
        with tarfile.open(artifact, "r:") as archive:
            original_info = archive.extractfile("INFO").read()  # type: ignore[union-attr]
        match = re.search(rb'(?m)^extractsize="([1-9][0-9]*)"$', original_info)
        self.assertIsNotNone(match)
        declared = int(match.group(1))  # type: ignore[union-attr]
        original_line = f'extractsize="{declared}"'.encode()
        cases = (
            ("malformed", b'extractsize="12 KiB"', "canonical positive integer"),
            ("zero", b'extractsize="0"', "canonical positive integer"),
            (
                "off-by-one",
                f'extractsize="{declared + 1}"'.encode(),
                "does not match package.tgz regular-file size",
            ),
        )
        for name, replacement, marker in cases:
            tampered = self.root / name / artifact.name
            tampered.parent.mkdir()
            repack_outer(
                artifact,
                tampered,
                info_payload=original_info.replace(original_line, replacement, 1),
            )
            result = subprocess.run(
                [sys.executable, str(HERE / "validate_spk.py"), str(tampered)],
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0, name)
            self.assertIn(marker, result.stderr, name)

    def test_validator_requires_thirdparty_and_exact_info_schema(self) -> None:
        artifact = self.build("x86_64", 62)
        with tarfile.open(artifact, "r:") as archive:
            original_info = archive.extractfile("INFO").read()  # type: ignore[union-attr]
        cases = (
            (
                "not-third-party",
                original_info.replace(b'thirdparty="yes"', b'thirdparty="no"', 1),
                "fixed field 'thirdparty' must be 'yes'",
            ),
            (
                "unknown-field",
                original_info + b'unreviewed_metadata="yes"\n',
                "unknown fields",
            ),
        )
        for name, info_payload, marker in cases:
            tampered = self.root / name / artifact.name
            tampered.parent.mkdir()
            repack_outer(artifact, tampered, info_payload=info_payload)
            result = subprocess.run(
                [sys.executable, str(HERE / "validate_spk.py"), str(tampered)],
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0, name)
            self.assertIn(marker, result.stderr, name)

    def test_rejects_wrong_machine_dynamic_interpreter_and_headerless_elf(self) -> None:
        cases = (
            ("wrong-machine", fake_elf(183), "x86_64", "machine"),
            ("elf64-interpreter", fake_elf(62, interpreter=True), "x86_64", "interpreter"),
            (
                "elf32-interpreter",
                fake_elf(3, elf_class=1, interpreter=True),
                "i686",
                "interpreter",
            ),
            (
                "elf32-needed",
                fake_elf(3, elf_class=1, needed_library=True),
                "i686",
                "DT_NEEDED",
            ),
            ("headerless", fake_elf(62, no_headers=True), "x86_64", "program headers"),
        )
        for name, payload, arch, marker in cases:
            result = self.run_rejected_build(name, payload, arch)
            self.assertIn(marker, result.stderr)

    @unittest.skipUnless(os.name == "posix", "creating a symlink is not portable on Windows")
    def test_builder_rejects_a_symlinked_binary_before_resolving_it(self) -> None:
        binary = self.root / "real.elf"
        binary.write_bytes(fake_elf(62))
        link = self.root / "linked.elf"
        link.symlink_to(binary)
        api_binary = self.root / "linked.api.elf"
        api_binary.write_bytes(fake_elf(62))
        api_binary.chmod(0o755)
        result = subprocess.run(
            [
                sys.executable,
                str(HERE / "build_spk.py"),
                "--binary",
                str(link),
                "--api-binary",
                str(api_binary),
                "--arch",
                "x86_64",
                "--version",
                "1.0.0",
                "--output",
                str(self.root / "linked"),
            ],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("non-symlink regular file", result.stderr)

    def test_validator_binds_filename_info_version_modes_types_and_binary_bytes(self) -> None:
        artifact = self.build("x86_64", 62)
        with tarfile.open(artifact, "r:") as archive:
            original_info = archive.extractfile("INFO").read()  # type: ignore[union-attr]

        wrong_info = original_info.replace(b'version="1.2.3-1"', b'version="9.9.9-9"')
        bad_info = self.root / "bad-info" / artifact.name
        bad_info.parent.mkdir()
        repack_outer(artifact, bad_info, info_payload=wrong_info)

        bad_mode = self.root / "bad-mode" / artifact.name
        bad_mode.parent.mkdir()
        repack_outer(artifact, bad_mode, mode_overrides={"scripts/preinst": 0o644})

        bad_type = self.root / "bad-type" / artifact.name
        bad_type.parent.mkdir()
        repack_outer(
            artifact, bad_type, type_overrides={"scripts/preinst": tarfile.DIRTYPE}
        )

        renamed = self.root / "synology-drive-sync-9.9.9-x86_64.spk"
        shutil.copyfile(artifact, renamed)

        for candidate, marker in (
            (bad_info, "filename"),
            (bad_mode, "mode"),
            (bad_type, "regular file"),
            (renamed, "does not match filename"),
        ):
            result = subprocess.run(
                [sys.executable, str(HERE / "validate_spk.py"), str(candidate)],
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0, candidate.name)
            self.assertIn(marker, result.stderr, candidate.name)

        alternate = bytearray(fake_elf(62))
        alternate[-1] = 1
        alternate_binary = self.root / "alternate.elf"
        alternate_binary.write_bytes(alternate)
        mismatch = subprocess.run(
            [
                sys.executable,
                str(HERE / "validate_spk.py"),
                "--binary",
                str(alternate_binary),
                "--api-binary",
                str(self.last_api_binary),
                "--arch",
                "x86_64",
                str(artifact),
            ],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        self.assertNotEqual(mismatch.returncode, 0)
        self.assertIn("do not match", mismatch.stderr)

    def test_archive_and_privilege_manifest_are_never_setid(self) -> None:
        artifact = self.build("x86_64", 62)
        with tarfile.open(artifact, "r:") as archive:
            self.assertFalse(
                any(member.mode & 0o6000 for member in archive.getmembers())
            )
            package_payload = archive.extractfile("package.tgz").read()  # type: ignore[union-attr]
            privilege_payload = archive.extractfile("conf/privilege").read()  # type: ignore[union-attr]

        archived_setuid = self.root / "archived-setuid" / artifact.name
        archived_setuid.parent.mkdir()
        repack_outer(
            artifact,
            archived_setuid,
            payload_overrides={
                "package.tgz": repack_payload_mode(package_payload, "ui/api.cgi", 0o4755)
            },
        )

        privilege = json.loads(privilege_payload)
        self.assertEqual(
            privilege,
            {"defaults": {"run-as": "package"}, "join-groupname": "http"},
        )
        privilege["tool"] = [{
            "relpath": "ui/api.cgi",
            "user": "package",
            "group": "package",
            "permission": "4755",
        }]
        setuid_manifest = self.root / "setuid-manifest" / artifact.name
        setuid_manifest.parent.mkdir()
        repack_outer(
            artifact,
            setuid_manifest,
            payload_overrides={"conf/privilege": json.dumps(privilege).encode()},
        )

        outer_setgid = self.root / "outer-setgid" / artifact.name
        outer_setgid.parent.mkdir()
        repack_outer(
            artifact,
            outer_setgid,
            mode_overrides={"scripts/preinst": 0o2755},
        )

        for candidate, marker in (
            (archived_setuid, "setuid/setgid archive member: ui/api.cgi"),
            (setuid_manifest, "tool permission requests setuid/setgid"),
            (outer_setgid, "setuid/setgid archive member: scripts/preinst"),
        ):
            result = subprocess.run(
                [sys.executable, str(HERE / "validate_spk.py"), str(candidate)],
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0, candidate.name)
            self.assertIn(marker, result.stderr, candidate.name)

    def test_validator_rejects_outer_and_inner_pax_privilege_metadata(self) -> None:
        artifact = self.build("x86_64", 62)
        with tarfile.open(artifact, "r:") as archive:
            package_payload = archive.extractfile("package.tgz").read()  # type: ignore[union-attr]

        outer_member = self.root / "outer-member-pax" / artifact.name
        outer_member.parent.mkdir()
        repack_outer(
            artifact,
            outer_member,
            pax_overrides={
                "scripts/preinst": {
                    "SCHILY.xattr.security.capability": "unexpected-capability"
                }
            },
        )

        outer_global = self.root / "outer-global-pax" / artifact.name
        outer_global.parent.mkdir()
        repack_outer(
            artifact,
            outer_global,
            global_pax_headers={"SCHILY.acl.access": "unexpected-acl"},
        )

        inner_member = self.root / "inner-member-pax" / artifact.name
        inner_member.parent.mkdir()
        repack_outer(
            artifact,
            inner_member,
            payload_overrides={
                "package.tgz": repack_payload_pax(
                    package_payload,
                    pax_overrides={
                        "bin/sdsync-dsm-api": {
                            "SCHILY.acl.access": "unexpected-acl"
                        }
                    },
                )
            },
        )

        inner_global = self.root / "inner-global-pax" / artifact.name
        inner_global.parent.mkdir()
        repack_outer(
            artifact,
            inner_global,
            payload_overrides={
                "package.tgz": repack_payload_pax(
                    package_payload,
                    global_pax_headers={
                        "SCHILY.xattr.security.capability": "unexpected-capability"
                    },
                )
            },
        )

        for candidate, marker in (
            (outer_member, "unsupported member PAX headers"),
            (outer_global, "unsupported global PAX headers"),
            (inner_member, "unsupported member PAX headers"),
            (inner_global, "unsupported global PAX headers"),
        ):
            result = subprocess.run(
                [sys.executable, str(HERE / "validate_spk.py"), str(candidate)],
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0, candidate.name)
            self.assertIn(marker, result.stderr, candidate.name)

    def test_template_validator_is_a_standalone_gate(self) -> None:
        result = subprocess.run(
            [sys.executable, str(HERE / "validate_spk.py")],
            cwd=REPOSITORY,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("source templates", result.stdout)


@unittest.skipUnless(os.name == "posix", "DSM shell lifecycle mocks require a POSIX host")
class RuntimeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="sdsync-dsm-runtime-")
        self.root = Path(self.temporary.name)
        self.drop_uid = os.getuid() if os.getuid() != 0 else 65534
        self.drop_gid = os.getgid() if os.getuid() != 0 else 65534
        self.real_home = self.root / "apphome"
        self.real_var = self.root / "appdata"
        self.real_target = self.root / "appstore"
        self.fhs = self.root / "var-packages" / "synology-drive-sync"
        for path in (self.real_home, self.real_var, self.fhs):
            path.mkdir(parents=True, mode=0o700, exist_ok=True)
        shutil.copytree(HERE / "package", self.real_target)
        self.lifecycle_dir = self.root / "lifecycle"
        shutil.copytree(HERE / "scripts", self.lifecycle_dir)
        for path in self.real_target.rglob("*"):
            if path.is_file():
                path.chmod(0o755)
            elif path.is_dir():
                path.chmod(0o755)
        os.symlink(self.real_home, self.fhs / "home", target_is_directory=True)
        os.symlink(self.real_var, self.fhs / "var", target_is_directory=True)
        os.symlink(self.real_target, self.fhs / "target", target_is_directory=True)
        self.capture = self.root / "core.args"
        core = self.real_target / "bin/synology-drive-sync"
        core.write_text(
            "#!/bin/sh\n"
            ': "${SDSYNC_TEST_CAPTURE:?}"\n'
            'printf \'%s\\n\' "$*" >> "$SDSYNC_TEST_CAPTURE"\n'
            'case " $* " in *" config validate "*) exit 0 ;; esac\n'
            'if [ "${SDSYNC_TEST_HOLD:-false}" = true ]; then '
            '[ -z "${SDSYNC_TEST_CORE_PID_FILE:-}" ] || '
            'printf \'%s\\n\' "$$" > "$SDSYNC_TEST_CORE_PID_FILE"; '
            "trap 'sleep \"${SDSYNC_TEST_TERM_DELAY:-0}\"; exit 143' TERM INT; "
            "while :; do sleep 1; done; fi\n"
            "exit 0\n",
            encoding="utf-8",
        )
        core.chmod(0o755)
        self.write_api_mock()
        self.fake_system_bin = self.root / "fake-system-bin"
        self.fake_system_bin.mkdir(mode=0o700)
        fake_getent = self.fake_system_bin / "getent"
        fake_getent.write_text(
            "#!/bin/sh\n"
            'if [ "$#" -eq 2 ] && [ "$1" = group ] && [ "$2" = http ]; then\n'
            f"  printf 'http:x:{self.drop_gid}:\\n'\n"
            "  exit 0\n"
            "fi\n"
            "exit 2\n",
            encoding="utf-8",
        )
        fake_getent.chmod(0o755)
        self.source_one = self.root / "Source Folder"
        self.source_two = self.root / "Second Source"
        self.source_one.mkdir()
        self.source_two.mkdir()
        self.environment = os.environ.copy()
        self.environment.update(
            {
                "SYNOPKG_PKGDEST": str(self.fhs / "target"),
                "SYNOPKG_PKGHOME": str(self.fhs / "home"),
                "SYNOPKG_PKGVAR": str(self.fhs / "var"),
                "SYNOPKG_PKGNAME": "synology-drive-sync",
                "SYNOPKG_DSM_VERSION_MAJOR": "7",
                "SDSYNC_TEST_CAPTURE": str(self.capture),
                "SDSYNC_TEST_API_SOCKET": str(self.real_target / "ui/api.sock"),
                "SDSYNC_DSM_STOP_TIMEOUT": "10",
                "PATH": f"{self.fake_system_bin}:{os.environ['PATH']}",
            }
        )
        self.manager = self.fhs / "target/bin/sdsync-dsm"
        self.lifecycle = self.lifecycle_dir / "start-stop-status"
        if os.getuid() == 0:
            for path in [self.root, *self.root.rglob("*")]:
                # Model DSM: FHS links stay root-owned while their @apphome,
                # @appdata, and @appstore targets belong to the package user.
                if not path.is_symlink():
                    os.lchown(path, self.drop_uid, self.drop_gid)
        installed = self.shell(self.lifecycle_dir / "postinst")
        self.assertEqual(installed.returncode, 0, installed.stderr)

    def write_api_mock(
        self,
        *,
        queue_capture: Path | None = None,
        queue_lock: Path | None = None,
        consumer_tree_pid_file: Path | None = None,
        consumer_tree_ready_file: Path | None = None,
        consumer_tree_done_file: Path | None = None,
    ) -> None:
        bridge = self.real_target / "bin/sdsync-dsm-api"
        consume = ""
        if queue_capture is not None and queue_lock is not None:
            consume = f'''\nif len(sys.argv) == 4 and sys.argv[1] == "--consume-job":
    request = Path(sys.argv[2])
    response = Path(sys.argv[3])
    job_id = request.stem
    capture = Path({str(queue_capture)!r})
    lock = Path({str(queue_lock)!r})
    try:
        lock.mkdir()
    except FileExistsError:
        with capture.open("a", encoding="utf-8") as stream:
            stream.write("overlap\\n")
        raise SystemExit(75)
    try:
        secret = request.with_suffix(".secret")
        has_secret = "yes" if secret.is_file() else "no"
        with capture.open("a", encoding="utf-8") as stream:
            stream.write(f"{{job_id}} {{int(time.time())}} {{os.getuid()}} {{os.getgid()}} {{has_secret}}\\n")
        time.sleep(0.1)
        secret.unlink(missing_ok=True)
        response.write_text('{{"schema":"sdsync.dsm-result.v1","ok":true,"message":"queued"}}\\n', encoding="utf-8")
    finally:
        lock.rmdir()
    raise SystemExit(0)
'''
        consumer_tree = ""
        if (
            consumer_tree_pid_file is not None
            and consumer_tree_ready_file is not None
            and consumer_tree_done_file is not None
        ):
            consumer_tree = f'''
if len(sys.argv) == 4 and sys.argv[1] == "--consume-job":
    tree_environment = os.environ.copy()
    tree_environment["SDSYNC_TEST_TREE_PID_FILE"] = {str(consumer_tree_pid_file)!r}
    manager = None
    stopping = False
    def stop_consumer_tree(_signum, _frame):
        global stopping
        stopping = True
    signal.signal(signal.SIGTERM, stop_consumer_tree)
    signal.signal(signal.SIGINT, stop_consumer_tree)
    signal.signal(signal.SIGHUP, stop_consumer_tree)
    manager = subprocess.Popen(
        [
            "/bin/sh",
            "-c",
            "trap 'kill -TERM \\\"$worker\\\" 2>/dev/null || true; wait \\\"$worker\\\" 2>/dev/null || true; exit 143' TERM INT HUP; sleep 30 & worker=$!; printf '%s %s\\n' \\\"$$\\\" \\\"$worker\\\" > \\\"$SDSYNC_TEST_TREE_PID_FILE\\\"; wait \\\"$worker\\\"",
        ],
        env=tree_environment,
        start_new_session=True,
    )
    Path({str(consumer_tree_ready_file)!r}).write_text("consumer-ready\\n", encoding="utf-8")
    while manager.poll() is None:
        if stopping:
            try:
                # The Rust regression covers cooperative TERM handling. This
                # packaging fixture forcefully collapses its synthetic tree so
                # the assertion isolates whether lifecycle waits for consumer
                # cleanup before declaring the package stopped.
                os.killpg(manager.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            manager.wait(timeout=5)
            Path({str(consumer_tree_done_file)!r}).write_text("tree-terminal\\n", encoding="utf-8")
            raise SystemExit(143)
        time.sleep(0.02)
    raise SystemExit(manager.returncode)
'''
        bridge.write_text(
            f"#!{sys.executable}\n"
            "import os\n"
            "from pathlib import Path\n"
            "import signal\n"
            "import socket\n"
            "import stat\n"
            "import subprocess\n"
            "import sys\n"
            "import time\n"
            "\n"
            f"{consumer_tree}"
            "if len(sys.argv) == 4 and sys.argv[1] == '--consume-job' and "
            "os.environ.get('SDSYNC_TEST_HOLD_CONSUMER') == 'true':\n"
            "    running = True\n"
            "    def stop_consumer(_signum, _frame):\n"
            "        global running\n"
            "        running = False\n"
            "    signal.signal(signal.SIGTERM, stop_consumer)\n"
            "    signal.signal(signal.SIGINT, stop_consumer)\n"
            "    while running:\n"
            "        time.sleep(0.1)\n"
            "    raise SystemExit(0)\n"
            "\n"
            "if sys.argv[1:] == ['--serve']:\n"
            "    socket_path = os.environ['SDSYNC_TEST_API_SOCKET']\n"
            "    time.sleep(float(os.environ.get('SDSYNC_TEST_API_START_DELAY', '0')))\n"
            "    socket_file = Path(socket_path)\n"
            "    if socket_file.exists() or socket_file.is_symlink():\n"
            "        before = os.lstat(socket_path)\n"
            "        mode = stat.S_IMODE(before.st_mode)\n"
            "        safe = (stat.S_ISSOCK(before.st_mode) and "
            "before.st_uid == os.getuid() and before.st_nlink == 1 and "
            "(mode == 0o600 or (mode == 0o660 and before.st_gid == os.getgid())))\n"
            "        if not safe:\n"
            "            raise SystemExit(73)\n"
            "        probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)\n"
            "        probe.settimeout(0.25)\n"
            "        try:\n"
            "            probe.connect(socket_path)\n"
            "        except ConnectionRefusedError:\n"
            "            probe.close()\n"
            "            after = os.lstat(socket_path)\n"
            "            if (before.st_dev, before.st_ino, before.st_uid, before.st_gid, "
            "before.st_mode, before.st_nlink) != "
            "(after.st_dev, after.st_ino, after.st_uid, after.st_gid, "
            "after.st_mode, after.st_nlink):\n"
            "                raise SystemExit(73)\n"
            "            os.unlink(socket_path)\n"
            "        except FileNotFoundError:\n"
            "            probe.close()\n"
            "        except OSError:\n"
            "            probe.close()\n"
            "            raise SystemExit(73)\n"
            "        else:\n"
            "            probe.close()\n"
            "            raise SystemExit(75)\n"
            "    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)\n"
            "    server.bind(socket_path)\n"
            "    os.chmod(socket_path, 0o660)\n"
            "    server.listen(4)\n"
            "    server.settimeout(0.1)\n"
            "    running = True\n"
            "    def stop(_signum, _frame):\n"
            "        global running\n"
            "        running = False\n"
            "    signal.signal(signal.SIGTERM, stop)\n"
            "    signal.signal(signal.SIGINT, stop)\n"
            "    while running:\n"
            "        try:\n"
            "            connection, _ = server.accept()\n"
            "            connection.close()\n"
            "        except socket.timeout:\n"
            "            pass\n"
            "        except OSError:\n"
            "            if running:\n"
            "                raise\n"
            "    server.close()\n"
            "    raise SystemExit(0)\n"
            f"{consume}"
            "raise SystemExit(64)\n",
            encoding="utf-8",
        )
        bridge.chmod(0o755)
        if os.getuid() == 0:
            os.chown(bridge, self.drop_uid, self.drop_gid)

    def tearDown(self) -> None:
        stopped = self.shell(self.lifecycle, "stop", timeout=15)
        if stopped.returncode not in (0, 3):
            print(stopped.stderr, file=sys.stderr)
        self.temporary.cleanup()

    def shell(
        self, script: Path, *arguments: str, input_text: str | None = None,
        extra_environment: dict[str, str] | None = None, timeout: int = 20,
        drop_identity: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        environment = self.environment.copy()
        if extra_environment:
            environment.update(extra_environment)
        return subprocess.run(
            ["/bin/sh", str(script), *arguments],
            input=input_text,
            capture_output=True,
            text=True,
            env=environment,
            timeout=timeout,
            check=False,
            preexec_fn=(
                (lambda: (os.setgroups([]), os.setgid(self.drop_gid), os.setuid(self.drop_uid)))
                if os.getuid() == 0 and drop_identity
                else None
            ),
        )

    def shell_process(
        self,
        script: Path,
        *arguments: str,
        extra_environment: dict[str, str] | None = None,
    ) -> subprocess.Popen[str]:
        environment = self.environment.copy()
        if extra_environment:
            environment.update(extra_environment)
        return subprocess.Popen(
            ["/bin/sh", str(script), *arguments],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=environment,
            preexec_fn=(
                (lambda: (os.setgroups([]), os.setgid(self.drop_gid), os.setuid(self.drop_uid)))
                if os.getuid() == 0
                else None
            ),
        )

    def instrument_launch_assignment_signal(
        self,
        script: Path,
        launch_line: str,
        assignment_line: str,
        indentation: str,
    ) -> None:
        source = script.read_text(encoding="utf-8")
        needle = f"{launch_line}\n{assignment_line}"
        self.assertEqual(source.count(needle), 1, f"launch site changed in {script}")
        replacement = (
            f"{launch_line}\n"
            f'{indentation}while [ ! -e "${{SDSYNC_TEST_LAUNCH_READY:?}}" ]; do '
            "/bin/sleep 0.01; done\n"
            f'{indentation}kill -TERM "$$"\n'
            f"{assignment_line}"
        )
        script.write_text(source.replace(needle, replacement), encoding="utf-8")

    def write_terminable_launch_child(self, path: Path, *, core: bool = False) -> None:
        validate = 'case " $* " in *" config validate "*) exit 0 ;; esac\n' if core else ""
        path.write_text(
            "#!/bin/sh\n"
            f"{validate}"
            ': "${SDSYNC_TEST_LAUNCH_READY:?}"\n'
            ': "${SDSYNC_TEST_LAUNCHED_PID:?}"\n'
            ': "${SDSYNC_TEST_TERM_OBSERVED:?}"\n'
            "trap ': > \"$SDSYNC_TEST_TERM_OBSERVED\"; exit 143' TERM INT HUP\n"
            'printf \'%s\\n\' "$$" > "$SDSYNC_TEST_LAUNCHED_PID"\n'
            ': > "$SDSYNC_TEST_LAUNCH_READY"\n'
            "while :; do /bin/sleep 1; done\n",
            encoding="utf-8",
        )
        path.chmod(0o755)
        if os.getuid() == 0:
            os.chown(path, self.drop_uid, self.drop_gid)

    def assert_injected_launch_is_reaped(
        self,
        script: Path,
        arguments: tuple[str, ...],
        environment: dict[str, str],
        pid_file: Path,
        ready_file: Path,
        term_file: Path,
        expected_exit: int,
    ) -> None:
        process = self.shell_process(script, *arguments, extra_environment=environment)
        child_pid = None
        try:
            ready_deadline = time.monotonic() + 5
            while not ready_file.is_file() and time.monotonic() < ready_deadline:
                if process.poll() is not None:
                    break
                time.sleep(0.01)
            if pid_file.is_file():
                child_pid = int(pid_file.read_text(encoding="utf-8").strip())
            self.assertTrue(ready_file.is_file(), "instrumented child did not reach launch window")
            try:
                exit_code = process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.terminate()
                process.wait(timeout=5)
                self.fail("parent did not close the launch-assignment signal window")
            stdout, stderr = process.communicate()
            self.assertEqual(exit_code, expected_exit, stdout + stderr)
            self.assertTrue(term_file.is_file(), "launched child did not observe forwarded TERM")
            self.assertIsNotNone(child_pid)
            with self.assertRaises(ProcessLookupError):
                os.kill(child_pid, 0)  # type: ignore[arg-type]
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            if child_pid is not None:
                try:
                    os.kill(child_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def configure(self, name: str, source: Path, remote: str, default: bool = False) -> subprocess.CompletedProcess[str]:
        arguments = [
            "configure-profile", "--name", name, "--source", str(source),
            "--url", "https://files.example.test/proxy/", "--username", f"{name}-bot",
            "--remote", remote,
        ]
        if default:
            arguments.append("--default")
        return self.shell(self.manager, *arguments)

    def api(self, *arguments: str, input_text: str | None = None) -> tuple[subprocess.CompletedProcess[str], dict[str, object]]:
        result = self.shell(self.manager, "api", *arguments, input_text=input_text)
        try:
            payload = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            self.fail(f"API returned invalid JSON for {arguments}: {error}: {result.stdout!r} / {result.stderr!r}")
        self.assertIsInstance(payload, dict)
        return result, payload

    def fast_clock_environment(self, *, step: int = 31) -> dict[str, str]:
        fake_bin = self.root / f"fake-clock-bin-{step}"
        fake_bin.mkdir()
        clock = self.root / f"fake-clock-{step}"
        clock.write_text("1000\n", encoding="utf-8")
        fake_date = fake_bin / "date"
        fake_date.write_text(
            "#!/bin/sh\n"
            ': "${SDSYNC_TEST_CLOCK:?}"\n'
            'case "${1:-}" in\n'
            "  +%u) echo 1 ;;\n"
            "  +%H%M) echo 1200 ;;\n"
            "  +%Y-%m-%d) echo 2026-08-24 ;;\n"
            "  +%s) IFS= read -r now < \"$SDSYNC_TEST_CLOCK\"; "
            f"now=$((now + {step})); "
            'printf \'%s\\n\' "$now" > "$SDSYNC_TEST_CLOCK"; printf \'%s\\n\' "$now" ;;\n'
            "  *) exec /bin/date \"$@\" ;;\n"
            "esac\n",
            encoding="utf-8",
        )
        fake_sleep = fake_bin / "sleep"
        fake_sleep.write_text(
            "#!/bin/sh\n"
            "# Preserve lifecycle readiness/shutdown probes; only accelerate the\n"
            "# controller's multi-second scheduling sleeps.\n"
            "case ${1:-} in 1) exec /bin/sleep 1 ;; *) exec /bin/sleep 0.03 ;; esac\n",
            encoding="utf-8",
        )
        fake_inotify = fake_bin / "inotifywait"
        fake_inotify.write_text("#!/bin/sh\nexit 69\n", encoding="utf-8")
        for executable in (fake_date, fake_sleep, fake_inotify):
            executable.chmod(0o755)
        if os.getuid() == 0:
            for path in (fake_bin, clock, fake_date, fake_sleep, fake_inotify):
                os.chown(path, self.drop_uid, self.drop_gid)
        return {
            "PATH": f"{fake_bin}:{self.environment['PATH']}",
            "SDSYNC_TEST_CLOCK": str(clock),
        }

    def test_api_snapshot_advanced_config_order_and_secret_non_disclosure(self) -> None:
        ca_file = self.root / "trusted-ca.pem"
        ca_file.write_text("test-ca\n", encoding="utf-8")
        advanced = self.shell(
            self.manager,
            "configure-profile", "--name", "zeta", "--source", str(self.source_one),
            "--url", "https://files.example.test/", "--username", "zeta-bot",
            "--remote", "/home/Drive/Zeta", "--compare", "metadata", "--jobs", "4",
            "--allow-empty-source", "true", "--clear-excludes", "--exclude", "*.tmp",
            "--retries", "5", "--timeout", "123", "--connect-timeout", "9",
            "--max-rate", "4096", "--ca-certificate", str(ca_file),
            "--verbose", "2", "--quiet", "false", "--log-level", "debug",
            "--log-format", "json", "--progress", "never", "--output", "json",
            "--remote-log-url", "https://logs.example.test/ingest", "--remote-log-mode", "required",
            "--default",
        )
        self.assertEqual(advanced.returncode, 0, advanced.stderr)
        self.assertEqual(self.configure("alpha", self.source_two, "/home/Drive/Alpha").returncode, 0)

        secret = "a-secret-that-must-never-appear"
        replaced, replace_payload = self.api(
            "set-secret", "--profile", "zeta", "--kind", "password", "--mode", "replace",
            input_text=secret + "\n",
        )
        self.assertEqual(replaced.returncode, 0, replaced.stderr)
        self.assertTrue(replace_payload["has_password"])
        self.assertNotIn(secret, replaced.stdout + replaced.stderr)
        too_many, too_many_payload = self.api(
            "set-secret", "--profile", "zeta", "--kind", "totp", "--mode", "replace",
            input_text="first\nsecond\n",
        )
        self.assertEqual(too_many.returncode, 64)
        self.assertEqual(too_many_payload["code"], "invalid_request")
        self.assertNotIn("first", too_many.stdout + too_many.stderr)

        snapshot, payload = self.api("snapshot")
        self.assertEqual(snapshot.returncode, 0, snapshot.stderr)
        self.assertEqual(payload["schema"], "sdsync.dsm-api.v1")
        profiles = payload["profiles"]
        self.assertEqual([profile["name"] for profile in profiles], ["alpha", "zeta"])
        zeta = profiles[1]
        self.assertEqual(zeta["excludes"], ["*.tmp"])
        self.assertEqual(zeta["upload_timeout_seconds"], 123)
        self.assertEqual(zeta["max_rate_bytes_per_second"], 4096)
        self.assertEqual(zeta["remote_log_mode"], "required")
        self.assertTrue(zeta["has_password"])
        self.assertNotIn(secret, snapshot.stdout + snapshot.stderr)
        self.assertNotIn(".password", snapshot.stdout)
        self.assertEqual(payload["capabilities"], {"mutations": False, "secrets": False, "write_test": False})

        cleared, clear_payload = self.api(
            "set-secret", "--profile", "zeta", "--kind", "password", "--mode", "clear"
        )
        self.assertEqual(cleared.returncode, 0, cleared.stderr)
        self.assertFalse(clear_payload["has_password"])

    def test_api_routine_alert_schedule_and_invalid_action_contracts(self) -> None:
        self.assertEqual(self.configure("alpha", self.source_one, "/home/Drive/Alpha", True).returncode, 0)
        self.assertEqual(self.configure("beta", self.source_two, "/home/Drive/Beta").returncode, 0)
        for profile in ("alpha", "beta"):
            result, _ = self.api(
                "set-secret", "--profile", profile, "--kind", "password", "--mode", "replace",
                input_text="test-password\n",
            )
            self.assertEqual(result.returncode, 0, result.stderr)
        routine, _ = self.api(
            "routine", "--profile", "beta", "--enabled", "true", "--action", "sync",
            "--mode", "realtime", "--interval", "300", "--weekdays", "1,2,3,4,5,6,7",
            "--time-window-start", "00:00", "--time-window-end", "23:59",
            "--debounce-seconds", "3", "--retry-count", "4", "--retry-backoff-seconds", "30",
            "--poll-seconds", "5", "--allow-delete", "false", "--max-total-delete", "100",
            "--depends-on", "alpha",
        )
        self.assertEqual(routine.returncode, 0, routine.stderr)
        scheduled, _ = self.api(
            "schedule", "--enabled", "true", "--interval", "600",
            "--allow-delete", "false", "--max-total-delete", "55",
        )
        self.assertEqual(scheduled.returncode, 0, scheduled.stderr)
        alerts, _ = self.api(
            "alert-policy", "--enabled", "true", "--on-success", "false",
            "--on-failure", "true", "--failure-threshold", "2", "--cooldown", "600",
        )
        self.assertEqual(alerts.returncode, 0, alerts.stderr)
        snapshot, payload = self.api("snapshot")
        self.assertEqual(snapshot.returncode, 0, snapshot.stderr)
        self.assertEqual(payload["schedule"]["max_total_delete"], 55)
        self.assertEqual(payload["alerts"]["failure_threshold"], 2)
        self.assertEqual(payload["routines"][0]["depends_on"], ["alpha"])
        self.assertEqual(payload["routines"][0]["mode"], "realtime")

        invalid, invalid_payload = self.api(
            "action", "--kind", "doctor", "--scope", "alpha", "--allow-delete", "true"
        )
        self.assertEqual(invalid.returncode, 64)
        self.assertEqual(invalid_payload["code"], "invalid_request")
        invalid_write, invalid_write_payload = self.api(
            "action", "--kind", "plan", "--scope", "alpha", "--write-test", "true"
        )
        self.assertEqual(invalid_write.returncode, 64)
        self.assertEqual(invalid_write_payload["code"], "invalid_request")

        self.assertEqual(
            self.api(
                "routine", "--profile", "alpha", "--enabled", "true", "--action", "sync",
                "--mode", "interval", "--interval", "300", "--weekdays", "1,2,3,4,5,6,7",
                "--time-window-start", "00:00", "--time-window-end", "23:59",
                "--debounce-seconds", "5", "--retry-count", "1", "--retry-backoff-seconds", "30",
                "--poll-seconds", "30", "--allow-delete", "false", "--max-total-delete", "100",
                "--depends-on", "beta",
            )[0].returncode,
            64,
        )

    def test_api_logs_activity_and_corrupt_state_fail_closed(self) -> None:
        self.assertEqual(
            self.configure("logprofile", self.source_one, "/home/Drive/Logs", True).returncode,
            0,
        )
        secret, _ = self.api(
            "set-secret", "--profile", "logprofile", "--kind", "password", "--mode", "replace",
            input_text="a-secret-that-must-never-appear\n",
        )
        self.assertEqual(secret.returncode, 0, secret.stderr)
        controller_log = self.real_var / "log/controller.log"
        controller_log.write_text(
            f"\x1b[31mfailed at {self.real_home}/secrets/example.password with a-secret-that-must-never-appear\x1b[0m\n"
            + ("x" * 300_000) + "\n",
            encoding="utf-8",
        )
        logs, payload = self.api("logs", "--lines", "10")
        self.assertEqual(logs.returncode, 0, logs.stderr)
        self.assertEqual(payload["schema"], "sdsync.dsm-logs.v1")
        rendered = json.dumps(payload)
        self.assertNotIn("\x1b", rendered)
        self.assertNotIn(str(self.real_home), rendered)
        self.assertNotIn(".password", rendered)
        self.assertNotIn("a-secret-that-must-never-appear", rendered)
        self.assertLess(len(logs.stdout), 300_000)
        activity, activity_payload = self.api("activity", "--lines", "10")
        self.assertEqual(activity.returncode, 0, activity.stderr)
        self.assertEqual(activity_payload["schema"], "sdsync.dsm-activity.v1")

        run_state = self.real_var / "state/run.state"
        run_state.write_text("state=running\nstate=failed\n", encoding="utf-8")
        corrupt, corrupt_payload = self.api("snapshot")
        self.assertEqual(corrupt.returncode, 73)
        self.assertIn(corrupt_payload["code"], {"corrupt_state", "unsafe_state"})

    def test_api_doctor_health_cache_and_raw_cgi_fail_closed(self) -> None:
        self.assertEqual(self.configure("personal", self.source_one, "/home/Drive/Test", True).returncode, 0)
        secret, _ = self.api(
            "set-secret", "--profile", "personal", "--kind", "password", "--mode", "replace",
            input_text="test-password\n",
        )
        self.assertEqual(secret.returncode, 0, secret.stderr)
        doctor, doctor_payload = self.api(
            "action", "--kind", "doctor", "--scope", "personal", "--write-test", "true"
        )
        self.assertEqual(doctor.returncode, 0, doctor.stderr)
        self.assertEqual(doctor_payload["status"], "succeeded")
        snapshot, payload = self.api("snapshot")
        self.assertEqual(snapshot.returncode, 0, snapshot.stderr)
        self.assertEqual(payload["profiles"][0]["health"]["state"], "succeeded")
        self.assertTrue(payload["profiles"][0]["health"]["write_test"])
        rejected = self.shell(
            self.manager, "api", "snapshot", extra_environment={"REQUEST_METHOD": "GET"}
        )
        self.assertEqual(rejected.returncode, 77)
        self.assertEqual(json.loads(rejected.stdout)["code"], "bridge_required")

    def test_controller_executes_daily_profile_routine_once(self) -> None:
        self.assertEqual(self.configure("personal", self.source_one, "/home/Drive/Test", True).returncode, 0)
        secret, _ = self.api(
            "set-secret", "--profile", "personal", "--kind", "password", "--mode", "replace",
            input_text="test-password\n",
        )
        self.assertEqual(secret.returncode, 0, secret.stderr)
        routine, _ = self.api(
            "routine", "--profile", "personal", "--enabled", "true", "--action", "sync",
            "--mode", "daily", "--interval", "3600", "--weekdays", "1,2,3,4,5,6,7",
            "--time-window-start", "00:00", "--time-window-end", "23:59",
            "--debounce-seconds", "5", "--retry-count", "1", "--retry-backoff-seconds", "30",
            "--poll-seconds", "30", "--allow-delete", "false", "--max-total-delete", "100",
        )
        self.assertEqual(routine.returncode, 0, routine.stderr)
        self.capture.write_text("", encoding="utf-8")
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stderr)
        state_path = self.real_var / "state/routines/personal.state"
        for _ in range(200):
            if state_path.is_file() and "state=succeeded" in state_path.read_text(encoding="utf-8"):
                break
            time.sleep(0.05)
        else:
            self.fail("daily routine did not finish")
        captured = self.capture.read_text(encoding="utf-8")
        self.assertIn("sync --profile personal --no-delete", captured)
        time.sleep(0.2)
        self.assertEqual(
            self.capture.read_text(encoding="utf-8").count("sync --profile personal --no-delete"),
            1,
        )

    def test_routine_runner_launch_signal_is_forwarded_after_pid_assignment(self) -> None:
        self.assertEqual(
            self.configure("personal", self.source_one, "/home/Drive/Test", True).returncode,
            0,
        )
        routine, _ = self.api(
            "routine", "--profile", "personal", "--enabled", "true", "--action", "sync",
            "--mode", "daily", "--interval", "3600", "--weekdays", "1,2,3,4,5,6,7",
            "--time-window-start", "00:00", "--time-window-end", "23:59",
            "--debounce-seconds", "5", "--retry-count", "1", "--retry-backoff-seconds", "30",
            "--poll-seconds", "30", "--allow-delete", "false", "--max-total-delete", "100",
        )
        self.assertEqual(routine.returncode, 0, routine.stderr)

        runner = self.real_target / "libexec/sdsync-run"
        self.write_terminable_launch_child(runner)
        controller = self.real_target / "libexec/sdsync-controller"
        self.instrument_launch_assignment_signal(
            controller,
            '        "$runner" "$routine_action" "$routine_profile" "$routine_delete" scheduled - &',
            "        active_pid=$!",
            "        ",
        )
        ready = self.root / "routine-launch.ready"
        pid_file = self.root / "routine-launch.pid"
        terminated = self.root / "routine-launch.term"
        environment = {
            "SDSYNC_TEST_LAUNCH_READY": str(ready),
            "SDSYNC_TEST_LAUNCHED_PID": str(pid_file),
            "SDSYNC_TEST_TERM_OBSERVED": str(terminated),
        }
        self.assert_injected_launch_is_reaped(
            controller, (), environment, pid_file, ready, terminated, 0
        )
        self.assertFalse((self.real_var / "run/controller.lock").exists())

    def test_controller_interval_and_realtime_polling_fallback_trigger(self) -> None:
        self.assertEqual(self.configure("interval", self.source_one, "/home/Drive/Interval", True).returncode, 0)
        self.assertEqual(self.configure("watch", self.source_two, "/home/Drive/Watch").returncode, 0)
        for profile in ("interval", "watch"):
            secret, _ = self.api(
                "set-secret", "--profile", profile, "--kind", "password", "--mode", "replace",
                input_text="test-password\n",
            )
            self.assertEqual(secret.returncode, 0, secret.stderr)
        interval, _ = self.api(
            "routine", "--profile", "interval", "--enabled", "true", "--action", "sync",
            "--mode", "interval", "--interval", "600", "--weekdays", "1,2,3,4,5,6,7",
            "--time-window-start", "00:00", "--time-window-end", "23:59",
            "--debounce-seconds", "1", "--retry-count", "1", "--retry-backoff-seconds", "10",
            "--poll-seconds", "5", "--allow-delete", "false", "--max-total-delete", "100",
        )
        self.assertEqual(interval.returncode, 0, interval.stderr)
        watched_file = self.source_two / "watched.txt"
        watched_file.write_text("one\n", encoding="utf-8")
        realtime, _ = self.api(
            "routine", "--profile", "watch", "--enabled", "true", "--action", "sync",
            "--mode", "realtime", "--interval", "60", "--weekdays", "1,2,3,4,5,6,7",
            "--time-window-start", "00:00", "--time-window-end", "23:59",
            "--debounce-seconds", "1", "--retry-count", "1", "--retry-backoff-seconds", "10",
            "--poll-seconds", "5", "--allow-delete", "false", "--max-total-delete", "100",
        )
        self.assertEqual(realtime.returncode, 0, realtime.stderr)
        self.capture.write_text("", encoding="utf-8")
        fast_environment = self.fast_clock_environment(step=31)
        started = self.shell(self.lifecycle, "start", extra_environment=fast_environment, timeout=15)
        self.assertEqual(started.returncode, 0, started.stderr)
        interval_state = self.real_var / "state/routines/interval.state"
        watch_state = self.real_var / "state/routines/watch.state"
        for _ in range(200):
            if (
                interval_state.is_file()
                and "state=succeeded" in interval_state.read_text(encoding="utf-8")
                and watch_state.is_file()
                and "fingerprint=none" not in watch_state.read_text(encoding="utf-8")
                and "backend=polling" in watch_state.read_text(encoding="utf-8")
            ):
                break
            time.sleep(0.03)
        else:
            self.fail(
                "interval trigger or realtime polling initialization did not occur: "
                f"interval={interval_state.read_text(encoding='utf-8') if interval_state.exists() else 'missing'}; "
                f"watch={watch_state.read_text(encoding='utf-8') if watch_state.exists() else 'missing'}; "
                f"log={(self.real_var / 'log/controller.log').read_text(encoding='utf-8') if (self.real_var / 'log/controller.log').exists() else 'missing'}"
            )
        watched_file.write_text("two\n", encoding="utf-8")
        for _ in range(200):
            if "sync --profile watch --no-delete" in self.capture.read_text(encoding="utf-8"):
                break
            time.sleep(0.03)
        else:
            self.fail("realtime polling fallback did not observe a filesystem change")
        captured = self.capture.read_text(encoding="utf-8")
        self.assertIn("sync --profile interval --no-delete", captured)
        self.assertIn("sync --profile watch --no-delete", captured)

    def test_controller_dependency_deferral_then_success_and_retry_backoff(self) -> None:
        self.assertEqual(self.configure("base", self.source_one, "/home/Drive/Base", True).returncode, 0)
        self.assertEqual(self.configure("dependent", self.source_two, "/home/Drive/Dependent").returncode, 0)
        for profile in ("base", "dependent"):
            secret, _ = self.api(
                "set-secret", "--profile", profile, "--kind", "password", "--mode", "replace",
                input_text="test-password\n",
            )
            self.assertEqual(secret.returncode, 0, secret.stderr)
        routine, _ = self.api(
            "routine", "--profile", "dependent", "--enabled", "true", "--action", "sync",
            "--mode", "daily", "--interval", "60", "--weekdays", "1,2,3,4,5,6,7",
            "--time-window-start", "00:00", "--time-window-end", "23:59",
            "--debounce-seconds", "1", "--retry-count", "2", "--retry-backoff-seconds", "10",
            "--poll-seconds", "5", "--allow-delete", "false", "--max-total-delete", "100",
            "--depends-on", "base",
        )
        self.assertEqual(routine.returncode, 0, routine.stderr)
        fast_environment = self.fast_clock_environment(step=31)
        self.capture.write_text("", encoding="utf-8")
        started = self.shell(self.lifecycle, "start", extra_environment=fast_environment, timeout=15)
        self.assertEqual(started.returncode, 0, started.stderr)
        dependent_state = self.real_var / "state/routines/dependent.state"
        for _ in range(100):
            if dependent_state.is_file() and "state=deferred" in dependent_state.read_text(encoding="utf-8"):
                break
            time.sleep(0.03)
        else:
            self.fail("dependency did not defer the routine")
        base_plan = self.shell(self.manager, "plan", "base")
        self.assertEqual(base_plan.returncode, 0, base_plan.stderr)

        core = self.real_target / "bin/synology-drive-sync"
        retry_counter = self.root / "retry-counter"
        core.write_text(
            "#!/bin/sh\n"
            ': "${SDSYNC_TEST_CAPTURE:?}"\n'
            'printf \'%s\\n\' "$*" >> "$SDSYNC_TEST_CAPTURE"\n'
            'case " $* " in *" config validate "*) exit 0 ;; esac\n'
            f'counter="{retry_counter}"\n'
            'count=0; [ ! -f "$counter" ] || IFS= read -r count < "$counter"\n'
            'count=$((count + 1)); printf \'%s\\n\' "$count" > "$counter"\n'
            '[ "$count" -gt 1 ] || exit 42\n'
            "exit 0\n",
            encoding="utf-8",
        )
        core.chmod(0o755)
        if os.getuid() == 0:
            os.chown(core, self.drop_uid, self.drop_gid)
        for _ in range(300):
            if (
                retry_counter.is_file()
                and int(retry_counter.read_text(encoding="utf-8").strip()) >= 2
                and "state=succeeded" in dependent_state.read_text(encoding="utf-8")
            ):
                break
            time.sleep(0.03)
        else:
            self.fail("failed routine was not retried after its backoff")
        self.assertIn("retry_attempt=0", dependent_state.read_text(encoding="utf-8"))

    def test_routine_delete_ceiling_rejects_weaker_aggregate_bound(self) -> None:
        configured = self.shell(
            self.manager,
            "configure-profile", "--name", "mirror", "--source", str(self.source_one),
            "--url", "https://files.example.test/", "--username", "mirror-bot",
            "--remote", "/home/Drive/Mirror", "--delete", "--max-delete", "20", "--default",
        )
        self.assertEqual(configured.returncode, 0, configured.stderr)
        rejected, payload = self.api(
            "routine", "--profile", "mirror", "--enabled", "true", "--action", "sync",
            "--mode", "interval", "--interval", "60", "--weekdays", "1,2,3,4,5,6,7",
            "--time-window-start", "00:00", "--time-window-end", "23:59",
            "--debounce-seconds", "1", "--retry-count", "1", "--retry-backoff-seconds", "10",
            "--poll-seconds", "5", "--allow-delete", "true", "--max-total-delete", "19",
        )
        self.assertEqual(rejected.returncode, 64)
        self.assertEqual(payload["code"], "invalid_request")

    def test_alert_threshold_cooldown_fixed_event_and_unavailable_fallback(self) -> None:
        configured, _ = self.api(
            "alert-policy", "--enabled", "true", "--on-success", "false",
            "--on-failure", "true", "--failure-threshold", "2", "--cooldown", "60",
        )
        self.assertEqual(configured.returncode, 0, configured.stderr)
        clock = self.root / "alert-clock"
        clock.write_text("1000\n", encoding="utf-8")
        capture = self.root / "notifications"
        helper = self.root / "alert-helper.sh"
        helper.write_text(
            "#!/bin/sh\nset -eu\n"
            f'. "{self.real_target / "libexec/sdsync-common"}"\n'
            "ensure_layout\n"
            f'clock="{clock}"\n'
            f'capture="{capture}"\n'
            'epoch_now() { IFS= read -r value < "$clock"; printf \'%s\\n\' "$value"; }\n'
            'safe_notify() { printf \'%s %s %s\\n\' "$1" "$2" "$3" >> "$capture"; }\n'
            "handle_alert_result alpha failed 42\n"
            "handle_alert_result alpha failed 42\n"
            "handle_alert_result alpha failed 42\n"
            'printf \'1061\\n\' > "$clock"\n'
            "handle_alert_result alpha failed 42\n",
            encoding="utf-8",
        )
        helper.chmod(0o755)
        if os.getuid() == 0:
            for path in (clock, helper):
                os.chown(path, self.drop_uid, self.drop_gid)
        result = self.shell(helper)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            capture.read_text(encoding="utf-8").splitlines(),
            ["sync_failed alpha 42", "sync_failed alpha 42"],
        )
        self.assertIn(
            "failure_count=4",
            (self.real_var / "state/alerts.state").read_text(encoding="utf-8"),
        )

        unavailable = self.root / "notification-unavailable.sh"
        unavailable.write_text(
            "#!/bin/sh\nset -eu\n"
            f'. "{self.real_target / "libexec/sdsync-common"}"\n'
            "ensure_layout\n"
            "if [ ! -x /usr/syno/bin/synodsmnotify ]; then\n"
            "  set +e; safe_notify sync_failed alpha 42; code=$?; set -e; [ \"$code\" -eq 69 ]\n"
            "fi\n",
            encoding="utf-8",
        )
        unavailable.chmod(0o755)
        if os.getuid() == 0:
            os.chown(unavailable, self.drop_uid, self.drop_gid)
        fallback = self.shell(unavailable)
        self.assertEqual(fallback.returncode, 0, fallback.stderr)
        activity, payload = self.api("activity", "--lines", "20")
        self.assertEqual(activity.returncode, 0, activity.stderr)
        if not Path("/usr/syno/bin/synodsmnotify").exists():
            self.assertIn("notification.unavailable", [event["code"] for event in payload["events"]])

    def test_direct_dsm_notifications_use_fixed_argv_and_record_failures(self) -> None:
        common = self.real_target / "libexec/sdsync-common"
        source = common.read_text(encoding="utf-8")
        self.assertNotIn("/usr/syno/bin/synonotify", source)
        notifier = self.root / "synodsmnotify"
        capture = self.root / "synodsmnotify.args"
        notifier.write_text(
            "#!/bin/sh\n"
            f'printf \'%s\\n\' --CALL-- "$@" >> "{capture}"\n'
            "exit 23\n",
            encoding="utf-8",
        )
        notifier.chmod(0o755)
        common.write_text(
            source.replace("/usr/syno/bin/synodsmnotify", str(notifier)),
            encoding="utf-8",
        )
        profile = "A" * 62 + "-_"
        helper = self.root / "direct-notification.sh"
        helper.write_text(
            "#!/bin/sh\nset -eu\n"
            f'. "{common}"\n'
            "ensure_layout\n"
            f'profile="{profile}"\n'
            "for event in sync_succeeded sync_failed doctor_failed; do\n"
            "  set +e\n"
            "  safe_notify \"$event\" \"$profile\" 4294967295\n"
            "  code=$?\n"
            "  set -e\n"
            "  [ \"$code\" -eq 23 ]\n"
            "done\n"
            "for rejected in event profile exit; do\n"
            "  set +e\n"
            "  case $rejected in\n"
            "    event) safe_notify unregistered \"$profile\" 42 ;;\n"
            "    profile) safe_notify sync_failed 'bad/profile' 42 ;;\n"
            "    exit) safe_notify sync_failed \"$profile\" 42x ;;\n"
            "  esac\n"
            "  code=$?\n"
            "  set -e\n"
            "  [ \"$code\" -eq 64 ]\n"
            "done\n",
            encoding="utf-8",
        )
        helper.chmod(0o755)
        if os.getuid() == 0:
            for path in (notifier, common, helper):
                os.chown(path, self.drop_uid, self.drop_gid)

        result = self.shell(helper)
        self.assertEqual(result.returncode, 0, result.stderr)
        expected: list[str] = []
        for event in ("sync_succeeded", "sync_failed", "doctor_failed"):
            expected.extend(
                [
                    "--CALL--",
                    "-c",
                    "com.supermarsx.SynologyDriveSync",
                    "@administrators",
                    f"synology-drive-sync:notifications:{event}_title",
                    f"synology-drive-sync:notifications:{event}_message",
                ]
            )
        captured = capture.read_text(encoding="utf-8").splitlines()
        self.assertEqual(captured, expected)
        self.assertNotIn(profile, captured)
        self.assertNotIn("4294967295", captured)
        activity, payload = self.api("activity", "--lines", "20")
        self.assertEqual(activity.returncode, 0, activity.stderr)
        unavailable = [
            event for event in payload["events"]
            if event["code"] == "notification.unavailable"
        ]
        self.assertEqual(len(unavailable), 3)
        self.assertTrue(all(event["profile"] == profile for event in unavailable))
        self.assertTrue(
            all(
                event["message"]
                == "DSM desktop notification delivery unavailable"
                for event in unavailable
            )
        )

    def test_controller_private_queue_is_sequential_bounded_and_rejects_unsafe_entries(self) -> None:
        bridge_capture = self.root / "bridge-capture"
        bridge_lock = self.root / "bridge-lock"
        self.write_api_mock(queue_capture=bridge_capture, queue_lock=bridge_lock)
        requests = self.real_var / "control/requests"
        processing = self.real_var / "control/processing"
        responses = self.real_var / "control/responses"

        def private_file(path: Path, payload: str) -> None:
            path.write_text(payload, encoding="utf-8")
            path.chmod(0o600)
            if os.getuid() == 0:
                os.chown(path, self.drop_uid, self.drop_gid)

        symlink_id = "0" * 48
        collision_id = "1" * 48
        first_id = "2" * 48
        secret_id = "3" * 48
        response_collision_id = "4" * 48
        expired_response_id = "5" * 48
        expired_secret_id = "6" * 48
        retained_request_secret_id = "7" * 48
        expired_request_secret_id = "8" * 48
        retained_request_id = "9" * 48
        expired_request_id = "a" * 48
        orphaned_processing_id = "b" * 48
        symlink_target = self.root / "queue-symlink-target"
        symlink_target.write_text("{}\n", encoding="utf-8")
        os.symlink(symlink_target, requests / f"{symlink_id}.json")
        private_file(requests / f"{collision_id}.json", "{}\n")
        private_file(processing / f"{collision_id}.json", "{}\n")
        private_file(requests / f"{first_id}.json", "{}\n")
        private_file(requests / f"{secret_id}.json", "{}\n")
        private_file(requests / f"{secret_id}.secret", "one-line-secret\n")
        private_file(requests / f"{response_collision_id}.json", "{}\n")
        response_target = self.root / "queue-response-target"
        response_target.write_text("do-not-overwrite\n", encoding="utf-8")
        os.symlink(response_target, responses / f"{response_collision_id}.json")
        expired_response = responses / f"{expired_response_id}.json"
        private_file(expired_response, '{"schema":"sdsync.dsm-result.v1","ok":true}\n')
        expired_secret = processing / f"{expired_secret_id}.secret"
        private_file(expired_secret, "expired-secret\n")
        retained_request_secret = requests / f"{retained_request_secret_id}.secret"
        private_file(retained_request_secret, "retained-secret\n")
        expired_request_secret = requests / f"{expired_request_secret_id}.secret"
        private_file(expired_request_secret, "expired-request-secret\n")
        retained_request = requests / f"{retained_request_id}.json"
        private_file(retained_request, "{}\n")
        private_file(processing / f"{retained_request_id}.json", "{}\n")
        expired_request = requests / f"{expired_request_id}.json"
        private_file(expired_request, "{}\n")
        orphaned_processing = processing / f"{orphaned_processing_id}.json"
        private_file(orphaned_processing, "{}\n")
        bounded_responses = []
        for index in range(100, 358):
            bounded_response = responses / f"{index:048x}.json"
            private_file(bounded_response, '{"schema":"sdsync.dsm-result.v1","ok":true}\n')
            bounded_responses.append(bounded_response)
        expired_at = time.time() - 3700
        os.utime(expired_response, (expired_at, expired_at))
        os.utime(expired_secret, (expired_at, expired_at))
        os.utime(retained_request_secret, (expired_at, expired_at))
        os.utime(retained_request, (expired_at, expired_at))
        os.utime(orphaned_processing, (expired_at, expired_at))
        request_expired_at = time.time() - 86_500
        os.utime(expired_request_secret, (request_expired_at, request_expired_at))
        os.utime(expired_request, (request_expired_at, request_expired_at))

        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stderr)
        for _ in range(600):
            if (responses / f"{first_id}.json").is_file() and (responses / f"{secret_id}.json").is_file():
                break
            time.sleep(0.03)
        else:
            self.fail("controller did not consume the two safe queued requests")
        records = bridge_capture.read_text(encoding="utf-8").splitlines()
        self.assertFalse(any(record == "overlap" for record in records))
        parsed = [record.split() for record in records]
        self.assertEqual([record[0] for record in parsed[:2]], [first_id, secret_id])
        self.assertGreaterEqual(int(parsed[1][1]) - int(parsed[0][1]), 1)
        self.assertEqual(parsed[1][-1], "yes")
        if os.getuid() == 0:
            self.assertEqual(parsed[0][2], str(self.drop_uid))
            self.assertEqual(parsed[0][3], str(self.drop_gid))
        self.assertFalse((processing / f"{secret_id}.secret").exists())
        self.assertFalse((requests / f"{secret_id}.secret").exists())
        controller_log_path = self.real_var / "log/controller.log"
        controller_log = ""
        for _ in range(600):
            controller_log = controller_log_path.read_text(encoding="utf-8")
            if "control_response_rejected" in controller_log:
                break
            time.sleep(0.03)
        self.assertIn("control_request_rejected", controller_log)
        self.assertIn("control_request_collision", controller_log)
        self.assertIn("control_response_rejected", controller_log)
        self.assertEqual(response_target.read_text(encoding="utf-8"), "do-not-overwrite\n")
        self.assertNotIn(response_collision_id, bridge_capture.read_text(encoding="utf-8"))
        self.assertFalse(expired_response.exists())
        self.assertFalse(expired_secret.exists())
        self.assertTrue(retained_request_secret.exists())
        self.assertFalse(expired_request_secret.exists())
        self.assertTrue(retained_request.exists())
        self.assertFalse(expired_request.exists())
        self.assertFalse(orphaned_processing.exists())
        self.assertNotIn(orphaned_processing_id, bridge_capture.read_text(encoding="utf-8"))
        for _ in range(100):
            safe_responses = [
                path for path in responses.glob("*.json")
                if not path.is_symlink() and path.is_file() and len(path.stem) == 48
            ]
            if len(safe_responses) <= 256:
                break
            time.sleep(0.03)
        self.assertLessEqual(len(safe_responses), 256)
        self.assertIn("kind=processing_indeterminate", controller_log_path.read_text(encoding="utf-8"))

    def test_lifecycle_stop_waits_for_queued_tree_before_any_run_lock(self) -> None:
        tree_pid_file = self.root / "queued-tree.pids"
        tree_ready_file = self.root / "queued-tree.ready"
        tree_done_file = self.root / "queued-tree.done"
        self.write_api_mock(
            consumer_tree_pid_file=tree_pid_file,
            consumer_tree_ready_file=tree_ready_file,
            consumer_tree_done_file=tree_done_file,
        )
        job_id = "c" * 48
        requests = self.real_var / "control/requests"
        processing = self.real_var / "control/processing"
        request = requests / f"{job_id}.json"
        secret = requests / f"{job_id}.secret"
        secret_literal = "queued-shutdown-secret-should-not-leak"
        request.write_text("{}\n", encoding="utf-8")
        secret.write_text(f"{secret_literal}\n", encoding="utf-8")
        request.chmod(0o600)
        secret.chmod(0o600)
        if os.getuid() == 0:
            os.chown(request, self.drop_uid, self.drop_gid)
            os.chown(secret, self.drop_uid, self.drop_gid)

        tree_pids: list[int] = []
        try:
            started = self.shell(self.lifecycle, "start", timeout=15)
            self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
            for _ in range(300):
                if tree_pid_file.is_file() and tree_ready_file.is_file():
                    tree_pids = [
                        int(value)
                        for value in tree_pid_file.read_text(encoding="utf-8").split()
                    ]
                    if len(tree_pids) == 2:
                        break
                time.sleep(0.02)
            self.assertEqual(len(tree_pids), 2, "queued consumer tree did not start")
            self.assertFalse(
                (self.real_var / "run/run.lock").exists(),
                "fixture unexpectedly crossed into the runner-lock path",
            )
            self.assertTrue((processing / f"{job_id}.secret").is_file())

            stopped = self.shell(self.lifecycle, "stop", timeout=15)
            self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)
            controller_log = self.real_var / "log/controller.log"
            self.assertTrue(
                tree_done_file.is_file(),
                controller_log.read_text(encoding="utf-8", errors="replace")
                if controller_log.is_file()
                else "controller log missing",
            )
            for tree_pid in tree_pids:
                with self.assertRaises(ProcessLookupError):
                    os.kill(tree_pid, 0)
            self.assertFalse((processing / f"{job_id}.json").exists())
            self.assertFalse((processing / f"{job_id}.secret").exists())
            self.assertFalse(request.exists())
            self.assertFalse(secret.exists())
            logs = "\n".join(
                path.read_text(encoding="utf-8", errors="replace")
                for path in (self.real_var / "log").glob("*.log")
            )
            self.assertNotIn(secret_literal, logs)
        finally:
            if tree_pids:
                try:
                    os.killpg(tree_pids[0], signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_profiles_secrets_arbitrary_home_target_and_foreground_plan(self) -> None:
        if os.getuid() == 0:
            root_only_source = self.root / "root-only-source"
            root_only_source.mkdir(mode=0o700)
            refused_root = self.shell(
                self.manager, "configure-profile", "--name", "root-bypass",
                "--source", str(root_only_source), "--url", "https://files.example.test/",
                "--username", "root", "--remote", "/home/Drive/Root",
                drop_identity=False,
            )
            self.assertEqual(refused_root.returncode, 77)
            self.assertIn("as root", refused_root.stderr)
        first = self.configure("personal", self.source_one, "/home/Drive/Preferred Folder", True)
        second = self.configure("archive", self.source_two, "/ArchiveTeam/Documents")
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(second.returncode, 0, second.stderr)
        if os.getuid() == 0:
            refused_plan = self.shell(self.manager, "plan", "personal", drop_identity=False)
            self.assertEqual(refused_plan.returncode, 77)
            self.assertIn("as root", refused_plan.stderr)
        config = (self.real_home / "config/config.toml").read_text(encoding="utf-8")
        self.assertIn('/home/Drive/Preferred Folder', config)
        self.assertIn('[profiles.personal]', config)
        self.assertIn('[profiles.archive]', config)

        secret_input = self.root / "password.input"
        secret_input.write_text("not-a-real-password\n", encoding="utf-8")
        for profile in ("personal", "archive"):
            stored = self.shell(self.manager, "set-password", profile, "--from-file", str(secret_input))
            self.assertEqual(stored.returncode, 0, stored.stderr)
            mode = stat.S_IMODE((self.real_home / f"secrets/{profile}.password").stat().st_mode)
            self.assertEqual(mode, 0o600)

        plan = self.shell(self.manager, "plan", "personal")
        self.assertEqual(plan.returncode, 0, plan.stderr)
        captured = self.capture.read_text(encoding="utf-8")
        self.assertIn("plan --profile personal --no-delete", captured)
        conflict = self.shell(self.manager, "plan", "personal", "archive")
        self.assertEqual(conflict.returncode, 64)

    def test_default_scope_explicit_all_caps_and_command_specific_options(self) -> None:
        self.assertEqual(
            self.configure("alpha", self.source_one, "/home/Drive/Alpha").returncode,
            0,
        )
        self.assertEqual(
            self.configure("beta", self.source_two, "/home/Drive/Beta", True).returncode,
            0,
        )
        password = self.root / "password"
        password.write_text("test-password\n", encoding="utf-8")
        for profile in ("alpha", "beta"):
            stored = self.shell(
                self.manager, "set-password", profile, "--from-file", str(password)
            )
            self.assertEqual(stored.returncode, 0, stored.stderr)

        self.capture.write_text("", encoding="utf-8")
        default_plan = self.shell(self.manager, "plan")
        self.assertEqual(default_plan.returncode, 0, default_plan.stderr)
        self.assertIn(
            "plan --profile beta --no-delete",
            self.capture.read_text(encoding="utf-8"),
        )

        enabled = self.shell(
            self.manager, "enable", "--interval", "3600", "--max-total-delete", "999"
        )
        self.assertEqual(enabled.returncode, 0, enabled.stderr)
        self.capture.write_text("", encoding="utf-8")
        all_plan = self.shell(
            self.manager,
            "plan",
            "--all",
            "--allow-delete",
            "--max-total-delete",
            "7",
        )
        self.assertEqual(all_plan.returncode, 0, all_plan.stderr)
        self.assertIn(
            "plan --all-profiles --max-total-delete 7",
            self.capture.read_text(encoding="utf-8"),
        )

        self.capture.write_text("", encoding="utf-8")
        default_batch_cap = self.shell(self.manager, "plan", "--all")
        self.assertEqual(default_batch_cap.returncode, 0, default_batch_cap.stderr)
        captured = self.capture.read_text(encoding="utf-8")
        self.assertIn("plan --all-profiles --max-total-delete 100", captured)
        self.assertNotIn("max-total-delete 999", captured)

        rejected = (
            ("plan", "beta", "--max-total-delete", "5"),
            ("doctor", "--allow-delete"),
            ("plan", "--write-test"),
            ("doctor", "--max-total-delete", "5"),
        )
        for arguments in rejected:
            result = self.shell(self.manager, *arguments)
            self.assertEqual(result.returncode, 64, arguments)

    def test_zero_argument_commands_reject_trailing_arguments_and_help_succeeds(self) -> None:
        self.assertEqual(self.shell(self.manager, "help").returncode, 0)
        rejected = (
            ("help", "extra"),
            ("list-profiles", "extra"),
            ("disable", "extra"),
            ("status", "extra"),
            ("paths", "extra"),
        )
        for arguments in rejected:
            result = self.shell(self.manager, *arguments)
            self.assertEqual(result.returncode, 64, arguments)

    def test_schedule_mutation_lock_stale_recovery_and_run_refusal(self) -> None:
        self.assertEqual(self.configure("personal", self.source_one, "/home/Drive/Test", True).returncode, 0)
        password = self.root / "password"
        password.write_text("test-password\n", encoding="utf-8")
        self.assertEqual(self.shell(self.manager, "set-password", "personal", "--from-file", str(password)).returncode, 0)
        enabled = self.shell(self.manager, "enable", "--interval", "60")
        self.assertEqual(enabled.returncode, 0, enabled.stderr)
        self.assertIn("enabled=true", (self.real_home / "config/schedule.conf").read_text(encoding="utf-8"))
        self.assertEqual(self.shell(self.manager, "disable").returncode, 0)

        management = self.real_var / "run/management.lock"
        management.mkdir(mode=0o700)
        (management / "pid").write_text("99999999\n", encoding="utf-8")
        recovered = self.shell(self.manager, "set-password", "personal", input_text="new-password\n")
        self.assertEqual(recovered.returncode, 0, recovered.stderr)
        self.assertFalse(management.exists())

        run_lock = self.real_var / "run/run.lock"
        run_lock.mkdir(mode=0o700)
        (run_lock / "pid").write_text(f"{os.getpid()}\n", encoding="utf-8")
        refused = self.configure("personal", self.source_one, "/home/Drive/Changed")
        self.assertEqual(refused.returncode, 75)
        self.assertIn("while PID", refused.stderr)
        (run_lock / "pid").unlink()
        run_lock.rmdir()

    def test_remote_path_rejects_empty_dot_trailing_and_dsm_managed_components(self) -> None:
        invalid_paths = (
            "/home//Drive",
            "/home/Drive/",
            "/home/../Drive",
            "/home/./Drive",
            "/home/#recycle/Drive",
            "/home/#SNAPSHOT/Drive",
            "/home/@eaDir/Drive",
            "/home/@TMP/Drive",
            "/home/@sharebin/Drive",
            "/home/@APPHOME/Drive",
            "/home/@appdata/Drive",
            "/home/@appstore/Drive",
            "/home/@apptemp/Drive",
            "/home/@appconf/Drive",
            "/home/.SynologyWorkingDirectory/Drive",
            "/home/~temporary",
            "/home/trailing.",
            "/home/trailing ",
            "/home/CON",
            "/home/com1.txt",
            "/home/bad:name",
            "/home/tab\tname",
            "/home/" + ("x" * 243),
        )
        for index, remote in enumerate(invalid_paths):
            result = self.configure(f"bad{index}", self.source_one, remote)
            self.assertEqual(result.returncode, 64, f"accepted {remote}: {result.stderr}")
        config = self.real_home / "config/config.toml"
        self.assertFalse(config.exists())

    def test_source_is_canonicalized_and_root_aliases_are_rejected(self) -> None:
        aliased_source = f"{self.source_one}/../{self.source_one.name}"
        configured = self.configure(
            "canonical", Path(aliased_source), "/home/Drive/Canonical", True
        )
        self.assertEqual(configured.returncode, 0, configured.stderr)
        config = (self.real_home / "config/config.toml").read_text(encoding="utf-8")
        self.assertIn(f'source = "{self.source_one.resolve()}"', config)
        self.assertNotIn("/../", config)

        for index, source in enumerate(("/.", "//", "/tmp/../..")):
            result = self.configure(
                f"root{index}", Path(source), f"/home/Drive/Root{index}"
            )
            self.assertEqual(result.returncode, 64, source)
            self.assertIn("resolve to the filesystem root", result.stderr)
            self.assertFalse((self.real_home / f"config/profiles.d/root{index}.toml").exists())

    def test_source_rejects_package_storage_and_its_ancestors(self) -> None:
        candidates = (
            self.fhs / "home/config",
            self.fhs / "var/state",
            self.fhs / "target/bin",
            self.real_home,
            self.real_var,
            self.real_target,
            self.root,
        )
        for index, source in enumerate(candidates):
            result = self.configure(
                f"private{index}", source, f"/home/Drive/Private{index}"
            )
            self.assertEqual(result.returncode, 64, f"accepted package path {source}")
            self.assertIn("package-owned DSM storage", result.stderr)
            self.assertFalse(
                (self.real_home / f"config/profiles.d/private{index}.toml").exists()
            )

        for index, managed_name in enumerate(("@apphome", "@APPDATA", "@appstore", "@apptemp", "@appconf")):
            managed_source = self.root / managed_name / "Source"
            managed_source.mkdir(parents=True)
            result = self.configure(
                f"managed{index}", managed_source, f"/home/Drive/Managed{index}"
            )
            self.assertEqual(result.returncode, 64, managed_name)
            self.assertIn("DSM-managed", result.stderr)

    def test_removing_nondefault_profile_preserves_the_selected_default(self) -> None:
        self.assertEqual(
            self.configure("alpha", self.source_one, "/home/Drive/Alpha").returncode,
            0,
        )
        self.assertEqual(
            self.configure("beta", self.source_one, "/home/Drive/Beta", True).returncode,
            0,
        )
        self.assertEqual(
            self.configure("gamma", self.source_two, "/home/Drive/Gamma").returncode,
            0,
        )

        removed_nondefault = self.shell(self.manager, "remove-profile", "alpha")
        self.assertEqual(removed_nondefault.returncode, 0, removed_nondefault.stderr)
        self.assertEqual(
            (self.real_home / "config/default-profile").read_text(encoding="utf-8"),
            "beta\n",
        )
        self.assertIn(
            'default-profile = "beta"',
            (self.real_home / "config/config.toml").read_text(encoding="utf-8"),
        )

        removed_default = self.shell(self.manager, "remove-profile", "beta")
        self.assertEqual(removed_default.returncode, 0, removed_default.stderr)
        self.assertEqual(
            (self.real_home / "config/default-profile").read_text(encoding="utf-8"),
            "gamma\n",
        )

    def test_removing_last_profile_disables_schedule_before_deleting_it(self) -> None:
        self.assertEqual(
            self.configure("only", self.source_one, "/home/Drive/Only", True).returncode,
            0,
        )
        password = self.root / "password"
        password.write_text("test-password\n", encoding="utf-8")
        self.assertEqual(
            self.shell(
                self.manager, "set-password", "only", "--from-file", str(password)
            ).returncode,
            0,
        )
        self.assertEqual(
            self.shell(
                self.manager,
                "enable",
                "--interval",
                "3600",
                "--allow-delete",
                "--max-total-delete",
                "12",
            ).returncode,
            0,
        )
        removed = self.shell(self.manager, "remove-profile", "only")
        self.assertEqual(removed.returncode, 0, removed.stderr)
        schedule = (self.real_home / "config/schedule.conf").read_text(encoding="utf-8")
        self.assertIn("enabled=false", schedule)
        self.assertIn("allow_delete=false", schedule)
        self.assertIn("max_total_delete=12", schedule)
        self.assertFalse((self.real_home / "config/config.toml").exists())

    def test_term_waits_for_slow_core_and_retains_run_lock(self) -> None:
        self.assertEqual(
            self.configure("personal", self.source_one, "/home/Drive/Test", True).returncode,
            0,
        )
        password = self.root / "password"
        password.write_text("test-password\n", encoding="utf-8")
        self.assertEqual(
            self.shell(
                self.manager, "set-password", "personal", "--from-file", str(password)
            ).returncode,
            0,
        )

        core_pid_file = self.root / "slow-core.pid"
        environment = self.environment.copy()
        environment.update(
            {
                "SDSYNC_TEST_HOLD": "true",
                "SDSYNC_TEST_TERM_DELAY": "2",
                "SDSYNC_TEST_CORE_PID_FILE": str(core_pid_file),
            }
        )
        process = subprocess.Popen(
            [
                "/bin/sh",
                str(self.real_target / "libexec/sdsync-run"),
                "sync",
                "personal",
                "false",
                "foreground",
                "-",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=environment,
            preexec_fn=(
                (lambda: (os.setgid(self.drop_gid), os.setuid(self.drop_uid)))
                if os.getuid() == 0
                else None
            ),
        )
        core_pid = None
        try:
            run_lock = self.real_var / "run/run.lock"
            for _ in range(100):
                if core_pid_file.is_file() and run_lock.is_dir():
                    core_pid = int(core_pid_file.read_text(encoding="utf-8").strip())
                    break
                time.sleep(0.05)
            self.assertIsNotNone(core_pid, "slow core did not start")

            os.kill(process.pid, signal.SIGTERM)
            time.sleep(0.25)
            self.assertIsNone(process.poll(), "runner exited before its core process")
            self.assertTrue(run_lock.is_dir(), "run lock disappeared during core shutdown")
            os.kill(core_pid, 0)  # type: ignore[arg-type]

            self.assertEqual(process.wait(timeout=5), 143)
            self.assertFalse(run_lock.exists())
            with self.assertRaises(ProcessLookupError):
                os.kill(core_pid, 0)  # type: ignore[arg-type]
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            if core_pid is not None:
                try:
                    os.kill(core_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_scheduled_core_launch_signal_targets_and_reaps_the_execed_core(self) -> None:
        self.assertEqual(
            self.configure("personal", self.source_one, "/home/Drive/Test", True).returncode,
            0,
        )
        password = self.root / "launch-password"
        password.write_text("test-password\n", encoding="utf-8")
        self.assertEqual(
            self.shell(
                self.manager, "set-password", "personal", "--from-file", str(password)
            ).returncode,
            0,
        )

        core = self.real_target / "bin/synology-drive-sync"
        self.write_terminable_launch_child(core, core=True)
        runner = self.real_target / "libexec/sdsync-run"
        self.instrument_launch_assignment_signal(
            runner,
            "fi",
            "core_pid=$!",
            "",
        )
        ready = self.root / "core-launch.ready"
        pid_file = self.root / "core-launch.pid"
        terminated = self.root / "core-launch.term"
        environment = {
            "SDSYNC_TEST_LAUNCH_READY": str(ready),
            "SDSYNC_TEST_LAUNCHED_PID": str(pid_file),
            "SDSYNC_TEST_TERM_OBSERVED": str(terminated),
        }
        self.assert_injected_launch_is_reaped(
            runner,
            ("sync", "personal", "false", "scheduled", "-"),
            environment,
            pid_file,
            ready,
            terminated,
            143,
        )
        self.assertFalse((self.real_var / "run/run.lock").exists())

    def test_lifecycle_stop_rejects_forged_live_pid_without_signaling_it(self) -> None:
        sleeper = subprocess.Popen(
            ["/bin/sleep", "30"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            preexec_fn=(
                (lambda: (os.setgid(self.drop_gid), os.setuid(self.drop_uid)))
                if os.getuid() == 0
                else None
            ),
        )
        try:
            run_lock = self.real_var / "run/run.lock"
            run_lock.mkdir(mode=0o700)
            (run_lock / "pid").write_text(f"{sleeper.pid}\n", encoding="utf-8")
            stopped = self.shell(self.lifecycle, "stop")
            self.assertEqual(stopped.returncode, 1, stopped.stderr)
            self.assertIn("unverified plan/sync PID", stopped.stdout)
            self.assertIsNone(sleeper.poll(), "forged PID target was signaled")
        finally:
            if sleeper.poll() is None:
                sleeper.terminate()
                sleeper.wait(timeout=5)
            run_lock = self.real_var / "run/run.lock"
            if run_lock.is_dir():
                (run_lock / "pid").unlink(missing_ok=True)
                run_lock.rmdir()

    def test_verified_stop_signal_accepts_only_a_target_that_exited_during_kill(self) -> None:
        fixture_source = (
            "#!/bin/sh\n"
            ': > "${SDSYNC_TEST_RACE_READY:?}"\n'
            'while [ ! -e "${SDSYNC_TEST_RACE_RELEASE:?}" ]; do :; done\n'
            "exit 0\n"
        )
        fixtures = {
            "controller": self.real_target / "libexec/sdsync-controller",
            "runner": self.real_target / "libexec/sdsync-run",
            "api": self.real_target / "bin/sdsync-dsm-api",
        }
        for fixture in fixtures.values():
            fixture.write_text(fixture_source, encoding="utf-8")
            fixture.chmod(0o755)

        harness = self.root / "verified-signal-race.sh"
        harness.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            '. "$SDSYNC_TEST_COMMON"\n'
            "race_service=$1\n"
            "race_mode=$2\n"
            "case $race_service in\n"
            "  controller) race_command=$controller; race_argument= ;;\n"
            "  runner) race_command=$runner; race_argument= ;;\n"
            "  api) race_command=$api_server; race_argument=--serve ;;\n"
            "  *) exit 64 ;;\n"
            "esac\n"
            "if [ -n \"$race_argument\" ]; then\n"
            '  "$race_command" "$race_argument" &\n'
            "else\n"
            '  "$race_command" &\n'
            "fi\n"
            "race_pid=$!\n"
            "race_wait=0\n"
            'while [ ! -e "$SDSYNC_TEST_RACE_READY" ]; do\n'
            '  command kill -0 "$race_pid" 2>/dev/null || exit 80\n'
            '  [ "$race_wait" -lt 200 ] || exit 81\n'
            "  sleep 0.01\n"
            "  race_wait=$((race_wait + 1))\n"
            "done\n"
            "kill() {\n"
            '  if [ "$1" = -TERM ] && [ "$2" = "$race_pid" ]; then\n'
            '    if [ "$race_mode" = gone ]; then\n'
            '      : > "$SDSYNC_TEST_RACE_RELEASE"\n'
            '      wait "$race_pid"\n'
            '      command kill "$@"\n'
            "      return $?\n"
            "    fi\n"
            "    return 1\n"
            "  fi\n"
            '  command kill "$@"\n'
            "}\n"
            'if signal_verified_service "$race_service" "$race_pid" "$race_service"; then\n'
            "  race_result=0\n"
            "else\n"
            "  race_result=$?\n"
            "fi\n"
            'if [ "$race_mode" = gone ]; then\n'
            '  [ "$race_result" -eq 0 ] || exit 82\n'
            '  pid_slot_absent "$race_pid" || exit 83\n'
            "else\n"
            '  [ "$race_result" -ne 0 ] || exit 84\n'
            '  verified_service_pid_matches "$race_service" "$race_pid" || exit 85\n'
            '  : > "$SDSYNC_TEST_RACE_RELEASE"\n'
            '  wait "$race_pid"\n'
            "fi\n",
            encoding="utf-8",
        )
        harness.chmod(0o755)

        lifecycle_source = self.lifecycle.read_text(encoding="utf-8")
        self.assertIn(
            'signal_verified_service controller "$saved_pid" controller',
            lifecycle_source,
        )
        self.assertIn(
            'signal_verified_service runner "$run_pid" plan/sync',
            lifecycle_source,
        )
        self.assertIn(
            'signal_verified_service api "$saved_api_pid" API',
            lifecycle_source,
        )

        for service in fixtures:
            for mode in ("gone", "live"):
                with self.subTest(service=service, mode=mode):
                    ready = self.root / f"{service}-{mode}.ready"
                    release = self.root / f"{service}-{mode}.release"
                    result = self.shell(
                        harness,
                        service,
                        mode,
                        extra_environment={
                            "SDSYNC_TEST_COMMON": str(self.lifecycle_dir / "common"),
                            "SDSYNC_TEST_RACE_READY": str(ready),
                            "SDSYNC_TEST_RACE_RELEASE": str(release),
                        },
                        timeout=10,
                    )
                    self.assertEqual(
                        result.returncode,
                        0,
                        (service, mode, result.stdout, result.stderr),
                    )
                    self.assertNotIn("No such process", result.stderr)
                    if mode == "live":
                        self.assertIn("target remains live", result.stdout)

    def test_package_stop_waits_for_a_manual_foreground_run(self) -> None:
        self.assertEqual(
            self.configure("personal", self.source_one, "/home/Drive/Test", True).returncode,
            0,
        )
        password = self.root / "password"
        password.write_text("test-password\n", encoding="utf-8")
        self.assertEqual(
            self.shell(
                self.manager, "set-password", "personal", "--from-file", str(password)
            ).returncode,
            0,
        )
        core_pid_file = self.root / "manual-core.pid"
        environment = self.environment.copy()
        environment.update(
            {
                "SDSYNC_TEST_HOLD": "true",
                "SDSYNC_TEST_TERM_DELAY": "2",
                "SDSYNC_TEST_CORE_PID_FILE": str(core_pid_file),
            }
        )
        process = subprocess.Popen(
            [
                "/bin/sh",
                str(self.real_target / "libexec/sdsync-run"),
                "sync",
                "personal",
                "false",
                "foreground",
                "-",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=environment,
            preexec_fn=(
                (lambda: (os.setgid(self.drop_gid), os.setuid(self.drop_uid)))
                if os.getuid() == 0
                else None
            ),
        )
        core_pid = None
        try:
            for _ in range(100):
                if core_pid_file.is_file() and (self.real_var / "run/run.lock").is_dir():
                    core_pid = int(core_pid_file.read_text(encoding="utf-8").strip())
                    break
                time.sleep(0.05)
            self.assertIsNotNone(core_pid, "manual core did not start")
            started = time.monotonic()
            stopped = self.shell(self.lifecycle, "stop", timeout=15)
            elapsed = time.monotonic() - started
            self.assertEqual(stopped.returncode, 0, stopped.stderr)
            self.assertGreaterEqual(elapsed, 1.0)
            self.assertEqual(process.wait(timeout=5), 143)
            self.assertFalse((self.real_var / "run/run.lock").exists())
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            if core_pid is not None:
                try:
                    os.kill(core_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_interval_reload_rebases_pending_deadline(self) -> None:
        self.assertEqual(
            self.configure("personal", self.source_one, "/home/Drive/Test", True).returncode,
            0,
        )
        password = self.root / "password"
        password.write_text("test-password\n", encoding="utf-8")
        self.assertEqual(
            self.shell(
                self.manager, "set-password", "personal", "--from-file", str(password)
            ).returncode,
            0,
        )
        self.assertEqual(
            self.shell(self.manager, "enable", "--interval", "3600").returncode,
            0,
        )
        self.assertEqual(self.shell(self.lifecycle, "start", timeout=15).returncode, 0)
        state_path = self.real_var / "state/controller.state"

        initial_deadline = 0
        for _ in range(100):
            if state_path.is_file():
                state = dict(
                    line.split("=", 1)
                    for line in state_path.read_text(encoding="utf-8").splitlines()
                )
                initial_deadline = int(state.get("next_run_epoch", "0"))
                if initial_deadline > int(time.time()) + 3000:
                    break
            time.sleep(0.05)
        self.assertGreater(initial_deadline, int(time.time()) + 3000)

        changed = self.shell(self.manager, "enable", "--interval", "60")
        self.assertEqual(changed.returncode, 0, changed.stderr)
        rebased = 0
        for _ in range(100):
            state = dict(
                line.split("=", 1)
                for line in state_path.read_text(encoding="utf-8").splitlines()
            )
            rebased = int(state.get("next_run_epoch", "0"))
            if 0 < rebased < initial_deadline:
                break
            time.sleep(0.05)
        self.assertLessEqual(rebased, int(time.time()) + 65)

    def test_package_stop_waits_for_an_active_scheduled_run(self) -> None:
        self.assertEqual(
            self.configure("personal", self.source_one, "/home/Drive/Test", True).returncode,
            0,
        )
        password = self.root / "password"
        password.write_text("test-password\n", encoding="utf-8")
        self.assertEqual(
            self.shell(
                self.manager, "set-password", "personal", "--from-file", str(password)
            ).returncode,
            0,
        )
        self.assertEqual(
            self.shell(
                self.manager,
                "enable",
                "--interval",
                "60",
                "--max-total-delete",
                "9",
            ).returncode,
            0,
        )

        fake_bin = self.root / "fake-bin"
        fake_bin.mkdir()
        clock = self.root / "fake-clock"
        clock.write_text("1000\n", encoding="utf-8")
        fake_date = fake_bin / "date"
        fake_date.write_text(
            "#!/bin/sh\n"
            ': "${SDSYNC_TEST_CLOCK:?}"\n'
            'IFS= read -r now < "$SDSYNC_TEST_CLOCK"\n'
            "now=$((now + 31))\n"
            'printf \'%s\\n\' "$now" > "$SDSYNC_TEST_CLOCK"\n'
            'printf \'%s\\n\' "$now"\n',
            encoding="utf-8",
        )
        fake_sleep = fake_bin / "sleep"
        fake_sleep.write_text(
            "#!/bin/sh\n"
            "case ${1:-} in 1) exec /bin/sleep 1 ;; *) exec /bin/sleep 0.05 ;; esac\n",
            encoding="utf-8",
        )
        fake_date.chmod(0o755)
        fake_sleep.chmod(0o755)
        if os.getuid() == 0:
            os.chown(clock, self.drop_uid, self.drop_gid)

        core_pid_file = self.root / "scheduled-core.pid"
        fast_environment = {
            "PATH": f"{fake_bin}:{self.environment['PATH']}",
            "SDSYNC_TEST_CLOCK": str(clock),
            "SDSYNC_TEST_HOLD": "true",
            "SDSYNC_TEST_CORE_PID_FILE": str(core_pid_file),
        }
        started = self.shell(
            self.lifecycle, "start", extra_environment=fast_environment, timeout=15
        )
        controller_log = self.real_var / "log/controller.log"
        startup_diagnostic = started.stdout + started.stderr
        if controller_log.is_file():
            startup_diagnostic += "\ncontroller.log:\n" + controller_log.read_text(
                encoding="utf-8"
            )
        self.assertEqual(started.returncode, 0, startup_diagnostic)
        core_pid = None
        try:
            for _ in range(200):
                if core_pid_file.is_file() and (self.real_var / "run/run.lock").is_dir():
                    core_pid = int(core_pid_file.read_text(encoding="utf-8").strip())
                    break
                time.sleep(0.025)
            self.assertIsNotNone(core_pid, "scheduled core did not start")
            stopped = self.shell(
                self.lifecycle,
                "stop",
                extra_environment=fast_environment,
                timeout=15,
            )
            self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)
            self.assertFalse((self.real_var / "run/run.lock").exists())
            self.assertFalse((self.real_var / "run/controller.lock").exists())
            with self.assertRaises(ProcessLookupError):
                os.kill(core_pid, 0)  # type: ignore[arg-type]
        finally:
            if core_pid is not None:
                try:
                    os.kill(core_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_aggregate_runner_launch_signal_is_forwarded_after_pid_assignment(self) -> None:
        self.assertEqual(
            self.configure("personal", self.source_one, "/home/Drive/Test", True).returncode,
            0,
        )
        password = self.root / "aggregate-launch-password"
        password.write_text("test-password\n", encoding="utf-8")
        self.assertEqual(
            self.shell(
                self.manager, "set-password", "personal", "--from-file", str(password)
            ).returncode,
            0,
        )
        self.assertEqual(
            self.shell(self.manager, "enable", "--interval", "60").returncode,
            0,
        )

        runner = self.real_target / "libexec/sdsync-run"
        self.write_terminable_launch_child(runner)
        controller = self.real_target / "libexec/sdsync-controller"
        self.instrument_launch_assignment_signal(
            controller,
            '        "$runner" sync all "$allow_delete" scheduled "$maximum_total" &',
            "        active_pid=$!",
            "        ",
        )
        ready = self.root / "aggregate-launch.ready"
        pid_file = self.root / "aggregate-launch.pid"
        terminated = self.root / "aggregate-launch.term"
        environment = self.fast_clock_environment(step=61)
        environment.update(
            {
                "SDSYNC_TEST_LAUNCH_READY": str(ready),
                "SDSYNC_TEST_LAUNCHED_PID": str(pid_file),
                "SDSYNC_TEST_TERM_OBSERVED": str(terminated),
            }
        )
        self.assert_injected_launch_is_reaped(
            controller, (), environment, pid_file, ready, terminated, 0
        )
        self.assertFalse((self.real_var / "run/controller.lock").exists())

    def test_rotating_append_checks_size_before_every_controller_entry(self) -> None:
        helper = self.root / "append-log.sh"
        helper.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            f'. "{self.real_target / "libexec/sdsync-common"}"\n'
            "ensure_layout\n"
            "printf '0123456789012345678901234567890123456789' > \"$log_root/controller.log\"\n"
            "append_rotating_log \"$log_root/controller.log\" 32 2 'scheduled entry'\n",
            encoding="utf-8",
        )
        helper.chmod(0o755)
        result = self.shell(helper)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (self.real_var / "log/controller.log").read_text(encoding="utf-8"),
            "scheduled entry\n",
        )
        self.assertEqual(
            (self.real_var / "log/controller.log.1").read_text(encoding="utf-8"),
            "0123456789012345678901234567890123456789",
        )

    def test_secret_prompt_signal_restores_tty_and_management_lock(self) -> None:
        self.assertEqual(
            self.configure("personal", self.source_one, "/home/Drive/Test", True).returncode,
            0,
        )
        master_fd, slave_fd = pty.openpty()
        initial_flags = termios.tcgetattr(slave_fd)[3]
        environment = self.environment.copy()
        process = subprocess.Popen(
            ["/bin/sh", str(self.manager), "set-password", "personal"],
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=slave_fd,
            env=environment,
            preexec_fn=(
                (lambda: (os.setgid(self.drop_gid), os.setuid(self.drop_uid)))
                if os.getuid() == 0
                else None
            ),
            close_fds=True,
        )
        output = b""
        try:
            for _ in range(100):
                readable, _, _ = select.select([master_fd], [], [], 0.05)
                if readable:
                    output += os.read(master_fd, 4096)
                    if b"DSM password:" in output:
                        break
            self.assertIn(b"DSM password:", output)
            os.kill(process.pid, signal.SIGINT)
            self.assertEqual(process.wait(timeout=5), 130)
            restored_flags = termios.tcgetattr(slave_fd)[3]
            self.assertEqual(restored_flags & termios.ECHO, initial_flags & termios.ECHO)
            self.assertFalse((self.real_var / "run/management.lock").exists())
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            os.close(master_fd)
            os.close(slave_fd)

    def test_status_and_logs_require_identity_and_reject_symlinks(self) -> None:
        if os.getuid() == 0:
            refused = self.shell(self.manager, "status", drop_identity=False)
            self.assertEqual(refused.returncode, 77)
            self.assertIn("refusing to operate as root", refused.stderr)

        marker = self.root / "outside.marker"
        marker.write_text("outside\n", encoding="utf-8")
        state = self.real_var / "state/controller.state"
        state.symlink_to(marker)
        refused_state = self.shell(self.manager, "status")
        self.assertEqual(refused_state.returncode, 73)
        self.assertNotIn("outside", refused_state.stdout)
        state.unlink()

        log = self.real_var / "log/scheduler.log"
        log.symlink_to(marker)
        refused_log = self.shell(self.manager, "logs", "10")
        self.assertEqual(refused_log.returncode, 73)
        self.assertNotIn("outside", refused_log.stdout)

    def test_start_status_term_stop_and_upgrade_uninstall_run_guard(self) -> None:
        api_socket = self.real_target / "ui/api.sock"
        stale = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        stale.bind(str(api_socket))
        stale.close()
        api_socket.chmod(0o600)
        api_pid_file = self.real_var / "run/api.pid"
        api_pid_file.write_text("2147483647\n", encoding="utf-8")
        api_pid_file.chmod(0o600)
        if os.getuid() == 0:
            os.chown(api_socket, self.drop_uid, self.drop_gid)
            os.chown(api_pid_file, self.drop_uid, self.drop_gid)

        stale_status = self.shell(self.lifecycle, "status")
        self.assertEqual(
            stale_status.returncode,
            1,
            (
                stale_status.stdout,
                stale_status.stderr,
                api_socket.lstat(),
                api_pid_file.lstat(),
                (self.real_target / "ui").stat(),
            ),
        )
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, (started.stdout, started.stderr))
        self.assertEqual(self.shell(self.lifecycle, "status").returncode, 0)
        self.assertTrue(stat.S_ISSOCK(api_socket.stat().st_mode))
        self.assertEqual(api_socket.stat().st_mode & 0o7777, 0o660)
        self.assertEqual(api_socket.stat().st_uid, self.drop_uid)
        self.assertEqual(api_socket.stat().st_gid, self.drop_gid)
        self.assertEqual(api_pid_file.stat().st_mode & 0o7777, 0o600)
        stopped = self.shell(self.lifecycle, "stop", timeout=15)
        self.assertEqual(stopped.returncode, 0, stopped.stderr)
        self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)
        self.assertFalse(api_socket.exists())
        self.assertFalse(api_pid_file.exists())

        run_lock = self.real_var / "run/run.lock"
        run_lock.mkdir(mode=0o700)
        (run_lock / "pid").write_text(f"{os.getpid()}\n", encoding="utf-8")
        self.assertEqual(self.shell(self.lifecycle_dir / "preupgrade").returncode, 1)
        self.assertEqual(self.shell(self.lifecycle_dir / "preuninst").returncode, 1)
        (run_lock / "pid").unlink()
        run_lock.rmdir()

    def test_lifecycle_rejects_untracked_socket_and_unsafe_pid_artifacts(self) -> None:
        api_socket = self.real_target / "ui/api.sock"
        outside = self.root / "outside-api-marker"
        outside.write_text("keep\n", encoding="utf-8")
        api_socket.symlink_to(outside)
        self.assertEqual(self.shell(self.lifecycle, "status").returncode, 2)
        refused_socket = self.shell(self.lifecycle, "start")
        self.assertEqual(refused_socket.returncode, 1, refused_socket.stderr)
        self.assertIn("inconsistent lifecycle state", refused_socket.stdout)
        self.assertTrue(api_socket.is_symlink())
        self.assertEqual(outside.read_text(encoding="utf-8"), "keep\n")
        api_socket.unlink()

        api_pid_file = self.real_var / "run/api.pid"
        api_pid_file.symlink_to(outside)
        self.assertEqual(self.shell(self.lifecycle, "status").returncode, 2)
        refused_pid = self.shell(self.lifecycle, "start")
        self.assertEqual(refused_pid.returncode, 1, refused_pid.stderr)
        self.assertEqual(outside.read_text(encoding="utf-8"), "keep\n")
        api_pid_file.unlink()

        hardlink_source = self.real_var / "run/api-hardlink-source"
        hardlink_source.write_text("2147483647\n", encoding="utf-8")
        hardlink_source.chmod(0o600)
        if os.getuid() == 0:
            os.chown(hardlink_source, self.drop_uid, self.drop_gid)
        os.link(hardlink_source, api_pid_file)
        self.assertEqual(self.shell(self.lifecycle, "status").returncode, 2)
        self.assertEqual(self.shell(self.lifecycle, "start").returncode, 1)
        api_pid_file.unlink()
        hardlink_source.unlink()

        wrong_mode = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        wrong_mode.bind(str(api_socket))
        wrong_mode.close()
        api_socket.chmod(0o700)
        if os.getuid() == 0:
            os.chown(api_socket, self.drop_uid, self.drop_gid)
        self.assertEqual(self.shell(self.lifecycle, "status").returncode, 2)
        self.assertEqual(self.shell(self.lifecycle, "start").returncode, 1)
        self.assertEqual(self.shell(self.lifecycle, "stop").returncode, 1)
        self.assertTrue(stat.S_ISSOCK(api_socket.lstat().st_mode))
        api_socket.unlink()

        ui_directory = self.real_target / "ui"
        ui_directory.chmod(0o2755)
        try:
            self.assertEqual(self.shell(self.lifecycle, "prestart").returncode, 150)
            self.assertEqual(self.shell(self.lifecycle, "start").returncode, 1)
        finally:
            ui_directory.chmod(0o755)

    def test_lifecycle_rejects_wrong_group_stale_socket(self) -> None:
        if os.getuid() != 0:
            self.skipTest("wrong socket group requires a privileged test runner")
        api_socket = self.real_target / "ui/api.sock"
        wrong_group = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        wrong_group.bind(str(api_socket))
        wrong_group.close()
        api_socket.chmod(0o660)
        wrong_gid = 0 if self.drop_gid != 0 else 1
        os.chown(api_socket, self.drop_uid, wrong_gid)
        api_pid_file = self.real_var / "run/api.pid"
        api_pid_file.write_text("2147483647\n", encoding="utf-8")
        api_pid_file.chmod(0o600)
        os.chown(api_pid_file, self.drop_uid, self.drop_gid)
        try:
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 2)
            refused = self.shell(self.lifecycle, "start")
            self.assertEqual(refused.returncode, 1, refused.stderr)
            self.assertTrue(stat.S_ISSOCK(api_socket.lstat().st_mode))
            self.assertEqual(self.shell(self.lifecycle, "stop").returncode, 1)
            self.assertTrue(stat.S_ISSOCK(api_socket.lstat().st_mode))
        finally:
            api_socket.unlink(missing_ok=True)
            api_pid_file.unlink(missing_ok=True)

    def test_start_never_unlinks_a_live_listener_with_a_stale_pid(self) -> None:
        api_socket = self.real_target / "ui/api.sock"
        api_pid_file = self.real_var / "run/api.pid"
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(api_socket))
        listener.listen(4)
        api_socket.chmod(0o660)
        api_pid_file.write_text("2147483647\n", encoding="utf-8")
        api_pid_file.chmod(0o600)
        if os.getuid() == 0:
            os.chown(api_socket, self.drop_uid, self.drop_gid)
            os.chown(api_pid_file, self.drop_uid, self.drop_gid)
        original = api_socket.lstat()
        try:
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 1)
            refused = self.shell(self.lifecycle, "start", timeout=15)
            self.assertEqual(refused.returncode, 1, (refused.stdout, refused.stderr))
            self.assertIn("API service failed to start", refused.stdout)
            current = api_socket.lstat()
            self.assertEqual((current.st_dev, current.st_ino), (original.st_dev, original.st_ino))
            self.assertFalse(api_pid_file.exists())
            refused_stop = self.shell(self.lifecycle, "stop")
            self.assertEqual(refused_stop.returncode, 1, refused_stop.stderr)
            current = api_socket.lstat()
            self.assertEqual((current.st_dev, current.st_ino), (original.st_dev, original.st_ino))
            client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            client.settimeout(1)
            client.connect(str(api_socket))
            client.close()
        finally:
            listener.close()
            api_socket.unlink(missing_ok=True)
            api_pid_file.unlink(missing_ok=True)

    def test_lifecycle_never_treats_same_binary_consumer_as_api_server(self) -> None:
        request = self.root / "consumer-request.json"
        response = self.root / "consumer-response.json"
        request.write_text("{}\n", encoding="utf-8")
        environment = self.environment.copy()
        environment["SDSYNC_TEST_HOLD_CONSUMER"] = "true"
        consumer = subprocess.Popen(
            [str(self.real_target / "bin/sdsync-dsm-api"), "--consume-job", str(request), str(response)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=environment,
            preexec_fn=(
                (lambda: (os.setgroups([]), os.setgid(self.drop_gid), os.setuid(self.drop_uid)))
                if os.getuid() == 0
                else None
            ),
        )
        api_pid_file = self.real_var / "run/api.pid"
        try:
            time.sleep(0.1)
            self.assertIsNone(consumer.poll(), "consumer fixture did not stay live")
            api_pid_file.write_text(f"{consumer.pid}\n", encoding="utf-8")
            api_pid_file.chmod(0o600)
            if os.getuid() == 0:
                os.chown(api_pid_file, self.drop_uid, self.drop_gid)
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 2)
            refused = self.shell(self.lifecycle, "stop")
            self.assertEqual(refused.returncode, 1, refused.stderr)
            self.assertIn("unverified API PID", refused.stdout)
            self.assertIsNone(consumer.poll(), "non-serve same-binary process was signaled")
        finally:
            api_pid_file.unlink(missing_ok=True)
            if consumer.poll() is None:
                consumer.terminate()
                consumer.wait(timeout=5)

    def test_failed_controller_start_rolls_back_api_service(self) -> None:
        controller = self.real_target / "libexec/sdsync-controller"
        controller.chmod(0o644)
        try:
            failed = self.shell(self.lifecycle, "start", timeout=15)
            self.assertEqual(failed.returncode, 1, failed.stderr)
            self.assertIn("controller failed to start", failed.stdout)
            self.assertFalse((self.real_var / "run/api.pid").exists())
            self.assertFalse((self.real_target / "ui/api.sock").exists())
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)
        finally:
            controller.chmod(0o755)

    def test_interrupted_start_cannot_leave_a_live_untracked_api(self) -> None:
        environment = self.environment.copy()
        environment["SDSYNC_TEST_API_START_DELAY"] = "1"
        process = subprocess.Popen(
            ["/bin/sh", str(self.lifecycle), "start"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=environment,
            preexec_fn=(
                (lambda: (os.setgroups([]), os.setgid(self.drop_gid), os.setuid(self.drop_uid)))
                if os.getuid() == 0
                else None
            ),
        )
        api_pid_file = self.real_var / "run/api.pid"
        try:
            for _ in range(100):
                if api_pid_file.is_file():
                    break
                if process.poll() is not None:
                    break
                time.sleep(0.02)
            self.assertTrue(api_pid_file.is_file(), "API child never published its PID")
            process.terminate()
            stdout, stderr = process.communicate(timeout=15)
            self.assertNotEqual(process.returncode, 0, (stdout, stderr))
            self.assertFalse(api_pid_file.exists())
            self.assertFalse((self.real_target / "ui/api.sock").exists())
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)
            restarted = self.shell(self.lifecycle, "start", timeout=15)
            self.assertEqual(restarted.returncode, 0, restarted.stderr)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)

    def test_uninstall_cleanup_is_bounded_to_package_data(self) -> None:
        self.assertEqual(self.configure("personal", self.source_one, "/home/Drive/Test", True).returncode, 0)
        (self.real_home / "secrets/personal.password").write_text("secret\n", encoding="utf-8")
        outside = self.root / "outside.marker"
        outside.write_text("keep\n", encoding="utf-8")
        result = self.shell(
            self.lifecycle_dir / "postuninst",
            extra_environment={"SYNOPKG_PKG_STATUS": "UNINSTALL"},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(outside.is_file())
        self.assertTrue(self.source_one.is_dir())
        self.assertFalse((self.real_home / "config").exists())
        self.assertFalse((self.real_home / "secrets").exists())
        self.assertFalse((self.real_var / "run").exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
