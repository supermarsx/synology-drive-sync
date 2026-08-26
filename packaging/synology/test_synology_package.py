#!/usr/bin/env python3
"""Builder, archive, and executable DSM lifecycle regression tests."""

from __future__ import annotations

import copy
import io
import json
import os
import re
import select
import shlex
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


def repack_payload_member(
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
            if member.name == member_name:
                content = member_payload
                member.size = len(content)
            rebuilt.addfile(member, io.BytesIO(content) if content is not None else None)
    return rebuilt_payload.getvalue()


class BuilderTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="sdsync-spk-test-")
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def test_security_documentation_tracks_the_enforced_bridge_queue_and_audit_contract(self) -> None:
        text = (REPOSITORY / "docs/dsm/security.md").read_text(encoding="utf-8")
        normalized = " ".join(text.split())
        for required in (
            "The CGI invokes DSM's root-owned `authenticate.cgi`",
            "The server independently executes DSM's root-owned",
            "stable pre-acceptance code `csrf_rejected`",
            "`security-policy`, `client-event`, and `action`",
            "`policy_version=1`",
            "at most `N` active request-plus-processing jobs",
            "at most `N` retained terminal",
            "worst case `2N`",
            "300 through 86400 seconds",
            "`audit_pending` is pinned",
            "32-hex client request ID",
            "65 through 255 safe ASCII bytes",
            "incomplete active final record is durably truncated",
            "malformed rotated history remain fail closed",
            "`SO_PEERCRED` identifies the shared DSM web tier",
        ):
            self.assertIn(required, normalized)
        selector = (REPOSITORY / "docs/release-selector.md").read_text(encoding="utf-8")
        self.assertIn("lifecycle-equivalent 26.7-26.10", selector)
        self.assertNotIn("byte-equivalent 26.7-26.10", selector)

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
            self.assertIn(b'auto_upgrade_from="26.7-1"', info)
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
                self.assertEqual(set(ui_config), {"SynologyDriveSync.js"})
                native_applications = ui_config["SynologyDriveSync.js"]
                self.assertEqual(set(native_applications), {validate_spk.APP_ID})
                native_application = native_applications[validate_spk.APP_ID]
                self.assertEqual(native_application["type"], "app")
                self.assertEqual(native_application["appWindow"], validate_spk.APP_ID)
                self.assertEqual(native_application["depend"], [])
                self.assertNotIn("url", native_application)
                self.assertEqual(ui_entrypoint, "ui/SynologyDriveSync.js")
                self.assertIn(ui_entrypoint, package_members)
                self.assertTrue(package.getmember(ui_entrypoint).isfile())
                self.assertEqual(package.getmember(ui_entrypoint).mode, 0o644)
                self.assertIn("ui/style.css", package_members)
                self.assertEqual(package.getmember("ui/style.css").mode, 0o644)
                for legacy in ("ui/index.html", "ui/app.js", "ui/app.css"):
                    self.assertNotIn(legacy, package_members)
                native_script = package.extractfile(ui_entrypoint).read()  # type: ignore[union-attr]
                native_style = package.extractfile("ui/style.css").read()  # type: ignore[union-attr]
                validate_spk.validate_native_bundle(native_script, native_style)
                self.assertNotIn(b"/usr/syno/bin/synonotify", common)
                self.assertIn(b"/usr/syno/bin/synodsmnotify", common)
                self.assertNotIn("ui/texts/enu/mails", package_members)
                self.assertEqual(package.getmember("bin/sdsync-dsm-api").mode, 0o755)
                self.assertEqual(package.getmember("ui/api.cgi").mode, 0o755)
                self.assertFalse(
                    any(member.mode & 0o6000 for member in package.getmembers())
                )

    def test_validator_rejects_legacy_and_invalid_native_appwindow_configs(self) -> None:
        source = json.loads((HERE / "ui-src/app.config").read_text(encoding="utf-8"))
        application = copy.deepcopy(source[validate_spk.APP_ID])
        application["depend"] = []
        installed = {
            "SynologyDriveSync.js": {validate_spk.APP_ID: application}
        }
        validate_spk.validate_ui_config(json.dumps(installed).encode("utf-8"))

        cases: list[tuple[str, dict[str, object], str]] = []
        legacy = {".url": {"com.supermarsx.SynologyDriveSync": {"type": "url"}}}
        cases.append(("legacy-url-wrapper", legacy, "one reviewed native"))

        wrong_module = copy.deepcopy(installed)
        wrong_module["app.js"] = wrong_module.pop("SynologyDriveSync.js")
        cases.append(("legacy-module-name", wrong_module, "one reviewed native"))

        wrong_class = copy.deepcopy(installed)
        wrong_class["SynologyDriveSync.js"]["com.supermarsx.SynologyDriveSync"] = (
            wrong_class["SynologyDriveSync.js"].pop(validate_spk.APP_ID)
        )
        cases.append(("legacy-class", wrong_class, "native AppWindow class"))

        for name, key, value, pattern in (
            ("url-type", "type", "url", "property 'type'"),
            ("wrong-window", "appWindow", "Legacy.Window", "property 'appWindow'"),
            ("legacy-url", "url", "index.html", "unreviewed DSM application property"),
            ("nonempty-depend", "depend", ["unreviewed.js"], "dependency list"),
        ):
            config = copy.deepcopy(installed)
            config["SynologyDriveSync.js"][validate_spk.APP_ID][key] = value
            cases.append((name, config, pattern))

        missing_depend = copy.deepcopy(installed)
        del missing_depend["SynologyDriveSync.js"][validate_spk.APP_ID]["depend"]
        cases.append(("missing-depend", missing_depend, "dependency list"))

        for name, config, pattern in cases:
            with self.subTest(name=name), self.assertRaisesRegex(
                validate_spk.ValidationError, pattern
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

    def test_dsm_ui_notices_are_exact_and_tampering_fails_closed(self) -> None:
        artifact = self.build("x86_64", 62)
        expected = (
            HERE / "licenses/DSM_UI_THIRD_PARTY_LICENSES.txt"
        ).read_bytes()
        for marker in (
            b"vue-loader 15.10.1",
            b"Copyright (c) 2015-present Yuxi (Evan) You",
            b"webpack 5.91.0",
            b"Copyright JS Foundation and other contributors",
            b"DSM supplies the global Vue runtime",
        ):
            self.assertIn(marker, expected)

        with tarfile.open(artifact, "r:") as outer:
            outer_notice = outer.extractfile(
                "LICENSES/DSM_UI_THIRD_PARTY_LICENSES.txt"
            ).read()  # type: ignore[union-attr]
            package_payload = outer.extractfile(
                "package.tgz"
            ).read()  # type: ignore[union-attr]
        self.assertEqual(outer_notice, expected)
        with tarfile.open(fileobj=io.BytesIO(package_payload), mode="r:gz") as inner:
            inner_notice = inner.extractfile(
                "share/licenses/DSM_UI_THIRD_PARTY_LICENSES.txt"
            ).read()  # type: ignore[union-attr]
        self.assertEqual(inner_notice, expected)

        outer_tampered = self.root / "outer-ui-notice-tampered" / artifact.name
        outer_tampered.parent.mkdir()
        repack_outer(
            artifact,
            outer_tampered,
            payload_overrides={
                "LICENSES/DSM_UI_THIRD_PARTY_LICENSES.txt": b"tampered\n"
            },
        )
        inner_tampered = self.root / "inner-ui-notice-tampered" / artifact.name
        inner_tampered.parent.mkdir()
        repack_outer(
            artifact,
            inner_tampered,
            payload_overrides={
                "package.tgz": repack_payload_member(
                    package_payload,
                    "share/licenses/DSM_UI_THIRD_PARTY_LICENSES.txt",
                    b"tampered\n",
                )
            },
        )

        for candidate, marker in (
            (outer_tampered, "outer DSM UI third-party notices"),
            (inner_tampered, "package.tgz DSM UI third-party notices"),
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
                "wrong-upgrade-floor",
                original_info.replace(
                    b'auto_upgrade_from="26.7-1"', b'auto_upgrade_from="26.6-1"', 1
                ),
                "fixed field 'auto_upgrade_from' must be '26.7-1'",
            ),
            (
                "missing-upgrade-floor",
                original_info.replace(b'auto_upgrade_from="26.7-1"\n', b"", 1),
                "INFO is missing fields: ['auto_upgrade_from']",
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

    def test_info_checksum_is_exact_package_tgz_md5_and_rejects_tamper(self) -> None:
        import hashlib

        first = self.build("x86_64", 62, "checksum-one")
        second = self.build("x86_64", 62, "checksum-two")
        self.assertEqual(first.read_bytes(), second.read_bytes())
        with tarfile.open(first, "r:") as archive:
            original_info = archive.extractfile("INFO").read()  # type: ignore[union-attr]
            package_payload = archive.extractfile("package.tgz").read()  # type: ignore[union-attr]

        info = validate_spk.parse_info(original_info)
        expected_checksum = hashlib.md5(
            package_payload, usedforsecurity=False
        ).hexdigest()
        self.assertEqual(info["checksum"], expected_checksum)
        self.assertRegex(info["checksum"], r"^[0-9a-f]{32}$")
        checksum_line = f'checksum="{expected_checksum}"'.encode()

        tampered_payload = bytearray(package_payload)
        self.assertEqual(bytes(tampered_payload[:3]), b"\x1f\x8b\x08")
        tampered_payload[9] ^= 1
        with tarfile.open(fileobj=io.BytesIO(tampered_payload), mode="r:gz") as inner:
            self.assertTrue(
                inner.getmembers(),
                "gzip-header tamper must leave the inner tar readable",
            )

        cases = (
            (
                "missing-checksum",
                original_info.replace(checksum_line + b"\n", b"", 1),
                package_payload,
                "INFO is missing fields: ['checksum']",
            ),
            (
                "malformed-checksum",
                original_info.replace(
                    checksum_line,
                    b'checksum="AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"',
                    1,
                ),
                package_payload,
                "32 lowercase hexadecimal MD5 characters",
            ),
            (
                "mismatched-checksum",
                original_info.replace(
                    checksum_line,
                    b'checksum="00000000000000000000000000000000"',
                    1,
                ),
                package_payload,
                "does not match exact package.tgz MD5",
            ),
            (
                "tampered-package-tgz",
                original_info,
                bytes(tampered_payload),
                "does not match exact package.tgz MD5",
            ),
        )
        for name, info_payload, payload, marker in cases:
            candidate = self.root / name / first.name
            candidate.parent.mkdir()
            repack_outer(
                first,
                candidate,
                info_payload=info_payload,
                payload_overrides={"package.tgz": payload},
            )
            result = subprocess.run(
                [sys.executable, str(HERE / "validate_spk.py"), str(candidate)],
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0, name)
            self.assertIn(marker, result.stderr, name)


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
        # Most manager tests exercise the public direct-CLI contract without
        # launching long-lived services. Open only their fixture admission;
        # lifecycle/transition tests explicitly close it again and runner
        # admission additionally requires exact live readiness.
        opened = self.executable(
            self.real_target / "bin/sdsync-dsm-api", "--service-admission", "open"
        )
        self.assertEqual(opened.returncode, 0, opened.stderr)

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
            "import ctypes\n"
            "import fcntl\n"
            "import json\n"
            "import os\n"
            "from pathlib import Path\n"
            "import signal\n"
            "import socket\n"
            "import stat\n"
            "import subprocess\n"
            "import sys\n"
            "import time\n"
            "\n"
            "def migrate_mock_security_policy(status_only=False):\n"
            "    policy = Path(os.environ['SYNOPKG_PKGHOME']) / 'config/security.conf'\n"
            "    try:\n"
            "        metadata = os.lstat(policy)\n"
            "    except FileNotFoundError:\n"
            "        return False\n"
            "    if (not stat.S_ISREG(metadata.st_mode) or stat.S_IMODE(metadata.st_mode) != 0o600\n"
            "            or metadata.st_uid != os.getuid() or metadata.st_nlink != 1):\n"
            "        raise SystemExit(73)\n"
            "    original = policy.read_bytes()\n"
            "    if len(original) > 8192:\n"
            "        raise SystemExit(73)\n"
            "    try:\n"
            "        text = original.decode('utf-8')\n"
            "    except UnicodeDecodeError:\n"
            "        raise SystemExit(73)\n"
            "    expected = [\n"
            "        'require_https', 'allow_interface_changes', 'allow_profile_changes',\n"
            "        'allow_secret_changes', 'allow_routine_changes', 'allow_notification_changes',\n"
            "        'allow_operational_actions', 'allow_http_targets', 'allow_invalid_tls',\n"
            "        'allow_destructive_sync', 'allow_doctor_write_test', 'allow_remote_logging',\n"
            "        'allow_empty_source', 'csrf_lifetime_seconds', 'result_retention_seconds',\n"
            "        'max_outstanding_jobs', 'audit_log_level', 'bridge_log_level',\n"
            "        'authentication_log_level', 'security_log_level', 'configuration_log_level',\n"
            "        'secrets_log_level', 'routines_log_level', 'operations_log_level',\n"
            "        'notifications_log_level', 'sync_log_level', 'controller_log_level',\n"
            "        'scheduler_log_level',\n"
            "    ]\n"
            "    fields = {}\n"
            "    for line in text.splitlines():\n"
            "        if not line or line.count('=') != 1:\n"
            "            raise SystemExit(73)\n"
            "        key, value = line.split('=', 1)\n"
            "        if key in fields:\n"
            "            raise SystemExit(73)\n"
            "        fields[key] = value\n"
            "    version = fields.pop('policy_version', None)\n"
            "    if set(fields) != set(expected) or len(fields) != 28:\n"
            "        raise SystemExit(73)\n"
            "    boolean_keys = expected[:13]\n"
            "    if any(fields[key] not in ('true', 'false') for key in boolean_keys):\n"
            "        raise SystemExit(73)\n"
            "    numeric_limits = {\n"
            "        'csrf_lifetime_seconds': (60, 900),\n"
            "        'result_retention_seconds': (300, 86400),\n"
            "        'max_outstanding_jobs': (1, 256),\n"
            "    }\n"
            "    for key, (minimum, maximum) in numeric_limits.items():\n"
            "        value = fields[key]\n"
            "        if not value.isascii() or not value.isdigit() or (len(value) > 1 and value[0] == '0'):\n"
            "            raise SystemExit(73)\n"
            "        if not minimum <= int(value) <= maximum:\n"
            "            raise SystemExit(73)\n"
            "    levels = {'off', 'trace', 'debug', 'info', 'warn', 'error'}\n"
            "    if any(fields[key] not in levels for key in expected[16:]):\n"
            "        raise SystemExit(73)\n"
            "    if version is not None:\n"
            "        if version != '1':\n"
            "            raise SystemExit(73)\n"
            "        return False\n"
            "    if status_only:\n"
            "        return True\n"
            "    migrated = b'policy_version=1\\n' + original\n"
            "    temporary = policy.with_name(f'.security.conf.migrate.{os.getpid()}')\n"
            "    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL\n"
            "    flags |= getattr(os, 'O_NOFOLLOW', 0) | getattr(os, 'O_CLOEXEC', 0)\n"
            "    descriptor = os.open(temporary, flags, 0o600)\n"
            "    try:\n"
            "        with os.fdopen(descriptor, 'wb', closefd=False) as stream:\n"
            "            stream.write(migrated)\n"
            "            stream.flush()\n"
            "        os.fsync(descriptor)\n"
            "    finally:\n"
            "        os.close(descriptor)\n"
            "    os.replace(temporary, policy)\n"
            "    directory = os.open(policy.parent, os.O_RDONLY | getattr(os, 'O_DIRECTORY', 0))\n"
            "    try:\n"
            "        os.fsync(directory)\n"
            "    finally:\n"
            "        os.close(directory)\n"
            "    return True\n"
            "\n"
            "def cleanup_mock_stale_api_socket():\n"
            "    root = Path(os.environ['SYNOPKG_PKGVAR'])\n"
            "    pid_path = root / 'run/api.pid'\n"
            "    socket_path = Path(os.environ['SDSYNC_TEST_API_SOCKET'])\n"
            "    pid_before = None\n"
            "    if pid_path.exists() or pid_path.is_symlink():\n"
            "        pid_before = os.lstat(pid_path)\n"
            "        if (not stat.S_ISREG(pid_before.st_mode) or pid_path.is_symlink() or\n"
            "                stat.S_IMODE(pid_before.st_mode) != 0o600 or\n"
            "                pid_before.st_uid != os.getuid() or pid_before.st_nlink != 1):\n"
            "            raise SystemExit(73)\n"
            "        raw_pid = pid_path.read_text(encoding='ascii')\n"
            "        if not raw_pid.endswith('\\n') or not raw_pid[:-1].isdigit() or int(raw_pid[:-1]) <= 1:\n"
            "            raise SystemExit(73)\n"
            "        try:\n"
            "            os.kill(int(raw_pid[:-1]), 0)\n"
            "        except ProcessLookupError:\n"
            "            pass\n"
            "        except OSError:\n"
            "            raise SystemExit(73)\n"
            "        else:\n"
            "            raise SystemExit(75)\n"
            "    if socket_path.exists() or socket_path.is_symlink():\n"
            "        socket_before = os.lstat(socket_path)\n"
            "        socket_mode = stat.S_IMODE(socket_before.st_mode)\n"
            "        if (not stat.S_ISSOCK(socket_before.st_mode) or socket_path.is_symlink() or\n"
            "                socket_before.st_uid != os.getuid() or socket_before.st_nlink != 1 or\n"
            "                socket_mode not in (0o600, 0o660) or\n"
            "                (socket_mode == 0o660 and socket_before.st_gid != os.getgid())):\n"
            "            raise SystemExit(73)\n"
            "        probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)\n"
            "        probe.settimeout(0.25)\n"
            "        try:\n"
            "            probe.connect(str(socket_path))\n"
            "        except ConnectionRefusedError:\n"
            "            pass\n"
            "        except FileNotFoundError:\n"
            "            socket_before = None\n"
            "        except OSError:\n"
            "            raise SystemExit(73)\n"
            "        else:\n"
            "            raise SystemExit(75)\n"
            "        finally:\n"
            "            probe.close()\n"
            "        if socket_before is not None:\n"
            "            socket_after = os.lstat(socket_path)\n"
            "            if (socket_before.st_dev, socket_before.st_ino) != (socket_after.st_dev, socket_after.st_ino):\n"
            "                raise SystemExit(73)\n"
            "            socket_path.unlink()\n"
            "    if pid_before is not None:\n"
            "        pid_after = os.lstat(pid_path)\n"
            "        if (pid_before.st_dev, pid_before.st_ino) != (pid_after.st_dev, pid_after.st_ino):\n"
            "            raise SystemExit(73)\n"
            "        pid_path.unlink()\n"
            "\n"
            "def append_mock_audit(record, state):\n"
            "    root = Path(os.environ['SYNOPKG_PKGVAR'])\n"
            "    log_root = root / 'log'\n"
            "    log_root.mkdir(mode=0o700, parents=True, exist_ok=True)\n"
            "    epoch = int(time.time())\n"
            "    category = {\n"
            "        'set-password': 'secrets', 'remove-password': 'secrets',\n"
            "        'set-totp': 'secrets', 'remove-totp': 'secrets',\n"
            "        'set-remote-log-token': 'secrets',\n"
            "        'remove-remote-log-token': 'secrets',\n"
            "        'routine': 'routines', 'remove-routine': 'routines',\n"
            "        'security-policy': 'security',\n"
            "        'doctor': 'operations', 'plan': 'operations', 'run': 'operations',\n"
            "    }.get(record['operation'], 'configuration')\n"
            "    level = 'error' if state == 'failed' else ('warn' if state == 'outcome_unknown' else 'info')\n"
            "    payload = {\n"
            "        'epoch': epoch, 'level': level, 'configured_level': 'info',\n"
            "        'subject_level': 'info', 'mandatory': True, 'category': 'audit',\n"
            "        'subject_category': category, 'operation': record['operation'],\n"
            "        'state': state, 'transaction': record['transaction'],\n"
            "        'origin': record['origin'], 'actor': record['actor'],\n"
            "        'actor_uid': record['actor_uid'], 'profile': record['profile'],\n"
            "    }\n"
            "    client_request_id = record.get('client_request_id')\n"
            "    if client_request_id is not None:\n"
            "        payload['client_request_id'] = client_request_id\n"
            "    audit = log_root / 'audit.log'\n"
            "    existing = [] if not audit.exists() else [json.loads(line) for line in audit.read_text(encoding='utf-8').splitlines() if line]\n"
            "    matches = [item for item in existing if item.get('transaction') == record['transaction'] and item.get('state') == state]\n"
            "    immutable_keys = ('operation', 'state', 'transaction', 'origin', 'actor', 'actor_uid', 'profile', 'client_request_id')\n"
            "    if matches and any(any(item.get(key) != payload.get(key) for key in immutable_keys) for item in matches):\n"
            "        raise SystemExit(73)\n"
            "    if not matches:\n"
            "        with audit.open('a', encoding='utf-8') as stream:\n"
            "            stream.write(json.dumps(payload, separators=(',', ':')) + '\\n')\n"
            "        audit.chmod(0o600)\n"
            "    activity_state = 'unavailable' if state == 'outcome_unknown' else state\n"
            "    activity = log_root / 'activity.log'\n"
            "    message = f\"Module {record['operation']} {state} [{record['transaction']}]\"\n"
            "    if client_request_id is not None:\n"
            "        message += f' request_id={client_request_id}'\n"
            "    activity_line = f\"{epoch}|audit.{state}|{record['profile']}|{activity_state}|audit|{level}|{record['actor_uid']}|{record['actor']}|{message}\\n\"\n"
            "    prior = activity.read_text(encoding='utf-8').splitlines() if activity.exists() else []\n"
            "    if activity_line.rstrip('\\n') not in prior:\n"
            "        with activity.open('a', encoding='utf-8') as stream:\n"
            "            stream.write(activity_line)\n"
            "        activity.chmod(0o600)\n"
            "    subject_level = {'requested': 'debug', 'succeeded': 'info', 'failed': 'error', 'outcome_unknown': 'warn'}[state]\n"
            "    defaults = {'audit': 'info', 'bridge': 'info', 'authentication': 'warn', 'security': 'warn',\n"
            "                'configuration': 'info', 'secrets': 'info', 'routines': 'info', 'operations': 'info',\n"
            "                'notifications': 'warn', 'sync': 'info', 'controller': 'info', 'scheduler': 'info'}\n"
            "    policy = Path(os.environ['SYNOPKG_PKGHOME']) / 'config/security.conf'\n"
            "    configured = defaults[category]\n"
            "    if policy.is_file() and not policy.is_symlink():\n"
            "        settings = dict(line.split('=', 1) for line in policy.read_text(encoding='utf-8').splitlines() if '=' in line)\n"
            "        configured = settings.get(f'{category}_log_level', configured)\n"
            "    rank = {'trace': 0, 'debug': 1, 'info': 2, 'warn': 3, 'error': 4}\n"
            "    if configured != 'off' and rank[subject_level] >= rank[configured]:\n"
            "        subject_code = f'module.{state}'\n"
            "        subject_line = f\"{epoch}|{subject_code}|{record['profile']}|{activity_state}|{category}|{subject_level}|{message}\\n\"\n"
            "        prior = activity.read_text(encoding='utf-8').splitlines()\n"
            "        if subject_line.rstrip('\\n') not in prior:\n"
            "            with activity.open('a', encoding='utf-8') as stream:\n"
            "                stream.write(subject_line)\n"
            "            activity.chmod(0o600)\n"
            "\n"
            "def repair_mock_log_tail(kind):\n"
            "    if kind not in ('audit', 'activity'):\n"
            "        raise SystemExit(64)\n"
            "    target = Path(os.environ['SYNOPKG_PKGVAR']) / 'log' / f'{kind}.log'\n"
            "    if not target.exists():\n"
            "        print('clean')\n"
            "        return\n"
            "    metadata = target.stat()\n"
            "    if (not target.is_file() or target.is_symlink() or metadata.st_uid != os.getuid() or\n"
            "            stat.S_IMODE(metadata.st_mode) != 0o600 or metadata.st_nlink != 1):\n"
            "        raise SystemExit(73)\n"
            "    def validate(line):\n"
            "        text = line.decode('utf-8')\n"
            "        if kind == 'audit':\n"
            "            record = json.loads(text)\n"
            "            if not isinstance(record, dict) or record.get('category') != 'audit' or record.get('mandatory') is not True:\n"
            "                raise ValueError('invalid audit record')\n"
            "        else:\n"
            "            fields = text.split('|')\n"
            "            if len(fields) not in (5, 7, 9) or not fields[0].isdigit():\n"
            "                raise ValueError('invalid activity record')\n"
            "    raw = target.read_bytes()\n"
            "    repaired = False\n"
            "    if raw and not raw.endswith(b'\\n'):\n"
            "        tail_start = raw.rfind(b'\\n') + 1\n"
            "        try:\n"
            "            validate(raw[tail_start:])\n"
            "            raw += b'\\n'\n"
            "        except Exception:\n"
            "            raw = raw[:tail_start]\n"
            "        target.write_bytes(raw)\n"
            "        target.chmod(0o600)\n"
            "        repaired = True\n"
            "    try:\n"
            "        for line in raw.splitlines():\n"
            "            if line:\n"
            "                validate(line)\n"
            "    except Exception:\n"
            "        raise SystemExit(73)\n"
            "    print('repaired' if repaired else 'clean')\n"
            "\n"
            "def handle_runtime_markers():\n"
            "    arguments = sys.argv[1:]\n"
            "    if not arguments or arguments[0] not in ('--package-transition', '--service-admission', '--failed-start-child'):\n"
            "        return False\n"
            "    runtime = Path(os.environ['SYNOPKG_PKGVAR']) / 'run'\n"
            "    runtime.mkdir(mode=0o700, parents=True, exist_ok=True)\n"
            "    def read_marker(path, limit):\n"
            "        try:\n"
            "            metadata = os.lstat(path)\n"
            "        except FileNotFoundError:\n"
            "            return None\n"
            "        if (not stat.S_ISREG(metadata.st_mode) or stat.S_IMODE(metadata.st_mode) != 0o600\n"
            "                or metadata.st_uid != os.getuid() or metadata.st_nlink != 1 or metadata.st_size > limit):\n"
            "            raise SystemExit(73)\n"
            "        return path.read_bytes()\n"
            "    def publish(path, expected):\n"
            "        actual = read_marker(path, 112)\n"
            "        if actual is not None:\n"
            "            if actual != expected:\n"
            "                raise SystemExit(73)\n"
            "            return\n"
            "        temporary = path.with_name(f'.{path.name}.{os.getpid()}.tmp')\n"
            "        descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, 'O_NOFOLLOW', 0), 0o600)\n"
            "        try:\n"
            "            os.write(descriptor, expected)\n"
            "            os.fsync(descriptor)\n"
            "        finally:\n"
            "            os.close(descriptor)\n"
            "        os.replace(temporary, path)\n"
            "    def clear(path, allowed):\n"
            "        actual = read_marker(path, 112)\n"
            "        if actual is None:\n"
            "            return\n"
            "        if actual not in allowed:\n"
            "            raise SystemExit(73)\n"
            "        path.unlink()\n"
            "    if arguments[0] == '--package-transition':\n"
            "        path = runtime / 'package.transition'\n"
            "        if arguments == ['--package-transition', 'status']:\n"
            "            value = read_marker(path, 16)\n"
            "            if value not in (None, b'upgrade\\n', b'uninstall\\n'):\n"
            "                raise SystemExit(73)\n"
            "            print('open' if value is None else value.decode('ascii').strip())\n"
            "        elif len(arguments) == 3 and arguments[:2] == ['--package-transition', 'prepare'] and arguments[2] in ('upgrade', 'uninstall'):\n"
            "            publish(path, (arguments[2] + '\\n').encode('ascii'))\n"
            "        elif arguments == ['--package-transition', 'clear']:\n"
            "            clear(path, (b'upgrade\\n', b'uninstall\\n'))\n"
            "        else:\n"
            "            raise SystemExit(64)\n"
            "        return True\n"
            "    if arguments[0] == '--service-admission':\n"
            "        path = runtime / 'service.closed'\n"
            "        if arguments == ['--service-admission', 'status']:\n"
            "            value = read_marker(path, 16)\n"
            "            if value not in (None, b'closed\\n'):\n"
            "                raise SystemExit(73)\n"
            "            print('open' if value is None else 'closed')\n"
            "        elif arguments == ['--service-admission', 'close']:\n"
            "            publish(path, b'closed\\n')\n"
            "        elif arguments == ['--service-admission', 'open']:\n"
            "            clear(path, (b'closed\\n',))\n"
            "        else:\n"
            "            raise SystemExit(64)\n"
            "        return True\n"
            "    if len(arguments) < 3 or arguments[2] not in ('api', 'controller'):\n"
            "        raise SystemExit(64)\n"
            "    kind = arguments[2]\n"
            "    path = runtime / f'failed-start.{kind}'\n"
            "    if arguments[1] == 'status' and len(arguments) == 3:\n"
            "        value = read_marker(path, 96)\n"
            "        if value is None:\n"
            "            print('absent')\n"
            "            return True\n"
            "        try:\n"
            "            fields = value.decode('ascii').split('\\n')\n"
            "        except UnicodeDecodeError:\n"
            "            raise SystemExit(73)\n"
            "        if (len(fields) != 5 or fields[0] != kind or not fields[1].isdigit() or int(fields[1]) <= 1\n"
            "                or not fields[2].isdigit() or int(fields[2]) <= 0 or len(fields[3]) != 36 or fields[4] != ''):\n"
            "            raise SystemExit(73)\n"
            "        print(f'present {fields[1]} {fields[2]} {fields[3]}')\n"
            "        return True\n"
            "    if arguments[1] in ('record', 'clear') and len(arguments) == 6:\n"
            "        pid, start, boot = arguments[3:]\n"
            "        if (not pid.isdigit() or int(pid) <= 1 or not start.isdigit() or int(start) <= 0\n"
            "                or len(boot) != 36):\n"
            "            raise SystemExit(64)\n"
            "        expected = f'{kind}\\n{pid}\\n{start}\\n{boot}\\n'.encode('ascii')\n"
            "        if arguments[1] == 'record':\n"
            "            publish(path, expected)\n"
            "        else:\n"
            "            clear(path, (expected,))\n"
            "        return True\n"
            "    raise SystemExit(64)\n"
            "\n"
            "def dispatch_mock_audit_transaction(arguments):\n"
            "    if len(arguments) == 3 and arguments[:2] == ['--audit-transaction', 'repair-log-tail']:\n"
            "        repair_mock_log_tail(arguments[2])\n"
            "        return True\n"
            "    if len(arguments) == 2 and arguments[0] == '--audit-transaction' and arguments[1] == 'reconcile':\n"
            "        failure_marker = os.environ.get('SDSYNC_TEST_AUDIT_RECONCILE_FAILURE_MARKER')\n"
            "        if failure_marker and Path(failure_marker).exists():\n"
            "            raise SystemExit(73)\n"
            "        outbox = Path(os.environ['SYNOPKG_PKGVAR']) / 'state/audit-outbox'\n"
            "        if outbox.is_dir():\n"
            "            for target in sorted(outbox.glob('*.mock-event')):\n"
            "                record = json.loads(target.read_text(encoding='utf-8'))\n"
            "                terminal = record.get('terminal_state')\n"
            "                if terminal in ('succeeded', 'failed', 'outcome_unknown'):\n"
            "                    policy = Path(os.environ['SYNOPKG_PKGHOME']) / 'config/security.conf'\n"
            "                    if record.get('operation') != 'security-policy' and (policy.is_symlink() or\n"
            "                            (policy.exists() and (lambda lines: len(lines) != 29 or lines[0] != 'policy_version=1')(policy.read_text(encoding='utf-8').splitlines()))):\n"
            "                        raise SystemExit(73)\n"
            "                    append_mock_audit(record, 'requested')\n"
            "                    append_mock_audit(record, terminal)\n"
            "                    target.unlink()\n"
            "        return True\n"
            "    if len(arguments) == 3 and arguments[:2] == ['--audit-transaction', 'create']:\n"
            "        print(f\"{arguments[2]}-{time.time_ns():x}-{os.getpid():x}\")\n"
            "        return True\n"
            "    if len(arguments) == 9 and arguments[:2] == ['--audit-transaction', 'verify']:\n"
            "        return True\n"
            "    if len(arguments) == 8 and arguments[:2] == ['--audit-transaction', 'validate']:\n"
            "        return True\n"
            "    root = Path(os.environ['SYNOPKG_PKGVAR'])\n"
            "    outbox = root / 'state/audit-outbox'\n"
            "    outbox.mkdir(mode=0o700, parents=True, exist_ok=True)\n"
            "    if len(arguments) == 8 and arguments[:2] == ['--audit-transaction', 'begin']:\n"
            "        operation, profile, actor, actor_uid, origin, transaction = arguments[2:]\n"
            "        record = {'operation': operation, 'profile': profile, 'actor': actor,\n"
            "                  'actor_uid': int(actor_uid), 'origin': origin, 'transaction': transaction}\n"
            "        target = outbox / f\"{transaction}.mock-event\"\n"
            "        if target.exists():\n"
            "            raise SystemExit(73)\n"
            "        target.write_text(json.dumps(record, separators=(',', ':')), encoding='utf-8')\n"
            "        target.chmod(0o600)\n"
            "        append_mock_audit(record, 'requested')\n"
            "        return True\n"
            "    if len(arguments) == 4 and arguments[:2] == ['--audit-transaction', 'complete']:\n"
            "        transaction, state = arguments[2:]\n"
            "        target = outbox / f\"{transaction}.mock-event\"\n"
            "        if not target.is_file():\n"
            "            raise SystemExit(73)\n"
            "        record = json.loads(target.read_text(encoding='utf-8'))\n"
            "        complete_ready = os.environ.get('SDSYNC_TEST_AUDIT_COMPLETE_WRITE_READY')\n"
            "        complete_release = os.environ.get('SDSYNC_TEST_AUDIT_COMPLETE_WRITE_RELEASE')\n"
            "        if complete_ready:\n"
            "            if not complete_release:\n"
            "                raise SystemExit(64)\n"
            "            reconcile_arm = os.environ.get('SDSYNC_TEST_AUDIT_RECONCILE_LOCK_ARM')\n"
            "            if not reconcile_arm:\n"
            "                raise SystemExit(64)\n"
            "            Path(reconcile_arm).write_text('reconcile-armed\\n', encoding='ascii')\n"
            "            target.write_bytes(b'')\n"
            "            target.chmod(0o600)\n"
            "            Path(complete_ready).write_text('audit-write-paused\\n', encoding='ascii')\n"
            "            deadline = time.monotonic() + 10\n"
            "            while not Path(complete_release).exists() and time.monotonic() < deadline:\n"
            "                time.sleep(0.01)\n"
            "            if not Path(complete_release).exists():\n"
            "                target.write_text(json.dumps(record, separators=(',', ':')), encoding='utf-8')\n"
            "                target.chmod(0o600)\n"
            "                raise SystemExit(75)\n"
            "        record['terminal_state'] = state\n"
            "        target.write_text(json.dumps(record, separators=(',', ':')), encoding='utf-8')\n"
            "        target.chmod(0o600)\n"
            "        append_failure = os.environ.get('SDSYNC_TEST_AUDIT_APPEND_FAILURE_MARKER')\n"
            "        if append_failure and Path(append_failure).exists():\n"
            "            raise SystemExit(75)\n"
            "        append_mock_audit(record, state)\n"
            "        target.unlink()\n"
            "        return True\n"
            "    if len(arguments) == 3 and arguments[:2] == ['--audit-transaction', 'execute']:\n"
            "        target = outbox / f\"{arguments[2]}.mock-event\"\n"
            "        if not target.is_file():\n"
            "            raise SystemExit(73)\n"
            "        return True\n"
            "    return False\n"
            "\n"
            "def handle_mock_audit_transaction():\n"
            "    arguments = sys.argv[1:]\n"
            "    if not arguments or arguments[0] != '--audit-transaction':\n"
            "        return False\n"
            "    if len(arguments) < 2 or arguments[1] not in ('reconcile', 'begin', 'execute', 'complete'):\n"
            "        return dispatch_mock_audit_transaction(arguments)\n"
            "    lock_path = Path(os.environ['SYNOPKG_PKGVAR']) / 'run/audit-outbox.flock'\n"
            "    descriptor = os.open(lock_path, os.O_RDWR | os.O_CREAT | getattr(os, 'O_CLOEXEC', 0), 0o600)\n"
            "    try:\n"
            "        if arguments == ['--audit-transaction', 'reconcile']:\n"
            "            reconcile_attempt = os.environ.get('SDSYNC_TEST_AUDIT_RECONCILE_LOCK_ATTEMPT')\n"
            "            reconcile_arm = os.environ.get('SDSYNC_TEST_AUDIT_RECONCILE_LOCK_ARM')\n"
            "            if reconcile_attempt and reconcile_arm and Path(reconcile_arm).exists():\n"
            "                Path(reconcile_attempt).write_text('reconcile-lock-attempt\\n', encoding='ascii')\n"
            "        fcntl.flock(descriptor, fcntl.LOCK_EX)\n"
            "        return dispatch_mock_audit_transaction(arguments)\n"
            "    finally:\n"
            "        os.close(descriptor)\n"
            "\n"
            "if handle_runtime_markers():\n"
            "    raise SystemExit(0)\n"
            "\n"
            "if handle_mock_audit_transaction():\n"
            "    raise SystemExit(0)\n"
            "\n"
            "if sys.argv[1:] == ['--security-policy-migration-status']:\n"
            "    print('required' if migrate_mock_security_policy(True) else 'unchanged')\n"
            "    raise SystemExit(0)\n"
            "\n"
            "if sys.argv[1:] == ['--migrate-security-policy']:\n"
            "    print('migrated' if migrate_mock_security_policy() else 'unchanged')\n"
            "    raise SystemExit(0)\n"
            "\n"
            "if sys.argv[1:] == ['--cleanup-stale-api-socket']:\n"
            "    cleanup_mock_stale_api_socket()\n"
            "    raise SystemExit(0)\n"
            "\n"
            "if len(sys.argv) >= 7 and sys.argv[1] == '--exec-supervised-core' and sys.argv[5] == '--':\n"
            "    expected_parent = int(sys.argv[2])\n"
            "    expected_start = sys.argv[3]\n"
            "    expected_boot = sys.argv[4]\n"
            "    def supervised_core_start(pid):\n"
            "        return Path(f'/proc/{pid}/stat').read_text(encoding='ascii').rsplit(') ', 1)[1].split()[19]\n"
            "    if (os.getppid() != expected_parent or supervised_core_start(expected_parent) != expected_start\n"
            "            or Path('/proc/sys/kernel/random/boot_id').read_text(encoding='ascii').strip() != expected_boot):\n"
            "        raise SystemExit(73)\n"
            "    core_libc = ctypes.CDLL(None, use_errno=True)\n"
            "    if core_libc.prctl(1, signal.SIGKILL, 0, 0, 0) != 0 or os.getppid() != expected_parent:\n"
            "        raise SystemExit(73)\n"
            "    os.execv(sys.argv[6], sys.argv[6:])\n"
            "\n"
            "if len(sys.argv) == 5 and sys.argv[1] == '--exec-supervised-controller':\n"
            "    expected_parent = int(sys.argv[2])\n"
            "    expected_start = sys.argv[3]\n"
            "    expected_boot = sys.argv[4]\n"
            "    def supervised_process_start(pid):\n"
            "        return Path(f'/proc/{pid}/stat').read_text(encoding='ascii').rsplit(') ', 1)[1].split()[19]\n"
            "    if (os.getppid() != expected_parent or supervised_process_start(expected_parent) != expected_start\n"
            "            or Path('/proc/sys/kernel/random/boot_id').read_text(encoding='ascii').strip() != expected_boot):\n"
            "        raise SystemExit(73)\n"
            "    libc = ctypes.CDLL(None, use_errno=True)\n"
            "    if libc.prctl(1, signal.SIGUSR1, 0, 0, 0) != 0 or os.getppid() != expected_parent:\n"
            "        raise SystemExit(73)\n"
            "    controller = str(Path(os.environ['SYNOPKG_PKGDEST']) / 'libexec/sdsync-controller')\n"
            "    os.execv(controller, [controller])\n"
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
            "serve_supervised = len(sys.argv) == 5 and sys.argv[1] == '--serve-supervised'\n"
            "if sys.argv[1:] == ['--serve'] or serve_supervised:\n"
            "    if serve_supervised:\n"
            "        expected_parent = int(sys.argv[2])\n"
            "        expected_parent_start = sys.argv[3]\n"
            "        expected_boot = sys.argv[4]\n"
            "        def process_start(pid):\n"
            "            return Path(f'/proc/{pid}/stat').read_text(encoding='ascii').rsplit(') ', 1)[1].split()[19]\n"
            "        current_boot = Path('/proc/sys/kernel/random/boot_id').read_text(encoding='ascii').strip()\n"
            "        if (os.getppid() != expected_parent or process_start(expected_parent) != expected_parent_start\n"
            "                or current_boot != expected_boot):\n"
            "            raise SystemExit(73)\n"
            "        api_libc = ctypes.CDLL(None, use_errno=True)\n"
            "        if api_libc.prctl(1, signal.SIGKILL, 0, 0, 0) != 0 or os.getppid() != expected_parent:\n"
            "            raise SystemExit(73)\n"
            "        term_observed = os.environ.get('SDSYNC_TEST_API_TERM_OBSERVED')\n"
            "        if term_observed:\n"
            "            def observe_early_term(_signum, _frame):\n"
            "                Path(term_observed).write_text('term\\n', encoding='ascii')\n"
            "            signal.signal(signal.SIGTERM, observe_early_term)\n"
            "        pre_pid_ready = os.environ.get('SDSYNC_TEST_API_PRE_PID_READY')\n"
            "        pre_pid_release = os.environ.get('SDSYNC_TEST_API_PRE_PID_RELEASE')\n"
            "        if pre_pid_ready and pre_pid_release:\n"
            "            Path(pre_pid_ready).write_text(f'{os.getpid()}\\n', encoding='ascii')\n"
            "            while not Path(pre_pid_release).exists():\n"
            "                if os.getppid() != expected_parent:\n"
            "                    raise SystemExit(73)\n"
            "                time.sleep(0.01)\n"
            "        pid_path = Path(os.environ['SYNOPKG_PKGVAR']) / 'run/api.pid'\n"
            "        descriptor = os.open(pid_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)\n"
            "        try:\n"
            "            os.write(descriptor, f'{os.getpid()}\\n'.encode('ascii'))\n"
            "            os.fsync(descriptor)\n"
            "        finally:\n"
            "            os.close(descriptor)\n"
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
            "    if serve_supervised:\n"
            "        if os.getppid() != expected_parent or process_start(expected_parent) != expected_parent_start:\n"
            "            server.close()\n"
            "            raise SystemExit(73)\n"
            "        identity = f'{os.getpid()}\\n{process_start(os.getpid())}\\n{current_boot}\\n'\n"
            "        bound_path = Path(os.environ['SYNOPKG_PKGVAR']) / 'run/api.bound'\n"
            "        bound_path.write_text(identity, encoding='ascii')\n"
            "        bound_path.chmod(0o600)\n"
            "        lease_path = Path(os.environ['SYNOPKG_PKGVAR']) / 'run/controller.starting'\n"
            "        prepared_lease = f'{expected_parent}\\n{expected_parent_start}\\n{expected_boot}\\n'\n"
            "        committed_lease = prepared_lease + 'committed\\n'\n"
            "        while True:\n"
            "            if os.getppid() != expected_parent or process_start(expected_parent) != expected_parent_start:\n"
            "                server.close()\n"
            "                raise SystemExit(73)\n"
            "            try:\n"
            "                lease_metadata = os.lstat(lease_path)\n"
            "                lease = lease_path.read_text(encoding='ascii')\n"
            "            except (FileNotFoundError, UnicodeError):\n"
            "                server.close()\n"
            "                raise SystemExit(73)\n"
            "            if (not stat.S_ISREG(lease_metadata.st_mode) or stat.S_IMODE(lease_metadata.st_mode) != 0o600\n"
            "                    or lease_metadata.st_uid != os.getuid() or lease_metadata.st_nlink != 1):\n"
            "                server.close()\n"
            "                raise SystemExit(73)\n"
            "            if lease == committed_lease:\n"
            "                break\n"
            "            if lease != prepared_lease:\n"
            "                server.close()\n"
            "                raise SystemExit(73)\n"
            "            time.sleep(0.02)\n"
            "        if api_libc.prctl(1, 0, 0, 0, 0) != 0 or os.getppid() != expected_parent:\n"
            "            server.close()\n"
            "            raise SystemExit(73)\n"
            "        if lease_path.read_text(encoding='ascii') != committed_lease:\n"
            "            server.close()\n"
            "            raise SystemExit(73)\n"
            "        post_commit_ready = os.environ.get('SDSYNC_TEST_API_POST_COMMIT_READY')\n"
            "        post_commit_release = os.environ.get('SDSYNC_TEST_API_POST_COMMIT_RELEASE')\n"
            "        if post_commit_ready:\n"
            "            signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
            "            Path(post_commit_ready).write_text(f'{os.getpid()}\\n', encoding='ascii')\n"
            "            while not (post_commit_release and Path(post_commit_release).exists()):\n"
            "                time.sleep(0.02)\n"
            "        bound_path.unlink()\n"
            "        ready_path = Path(os.environ['SYNOPKG_PKGVAR']) / 'run/api.ready'\n"
            "        ready_path.write_text(identity, encoding='ascii')\n"
            "        ready_path.chmod(0o600)\n"
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

    def executable(
        self, executable: Path, *arguments: str,
        extra_environment: dict[str, str] | None = None, timeout: int = 20,
    ) -> subprocess.CompletedProcess[str]:
        environment = self.environment.copy()
        if extra_environment:
            environment.update(extra_environment)
        return subprocess.run(
            [str(executable), *arguments],
            capture_output=True,
            text=True,
            env=environment,
            timeout=timeout,
            check=False,
            preexec_fn=(
                (lambda: (os.setgroups([]), os.setgid(self.drop_gid), os.setuid(self.drop_uid)))
                if os.getuid() == 0
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

    def write_controller_publication_barrier(
        self, ready: Path, release: Path, process_pid: Path
    ) -> None:
        controller = self.real_target / "libexec/sdsync-controller"
        controller.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            "trap 'exit 73' USR1\n"
            '. "$SYNOPKG_PKGDEST/libexec/sdsync-common"\n'
            "require_package_identity\n"
            "ensure_layout\n"
            "controller_start_lease_matches_parent\n"
            'acquire_private_process_lock "$controller_lock"\n'
            "controller_start_lease_matches_parent\n"
            "shutdown=false\n"
            "cleanup_controller_barrier() {\n"
            '  remove_own_controller_ready 2>/dev/null || true\n'
            '  current_pid=$(read_pid "$controller_pid_file" 2>/dev/null || true)\n'
            '  [ "$current_pid" != "$$" ] || rm -f "$controller_pid_file"\n'
            '  release_private_process_lock "$controller_lock" 2>/dev/null || true\n'
            "}\n"
            "request_controller_barrier_stop() { shutdown=true; }\n"
            "trap cleanup_controller_barrier 0\n"
            "trap request_controller_barrier_stop TERM INT\n"
            f"printf '%s\\n' \"$$\" > {shlex.quote(str(process_pid))}\n"
            f": > {shlex.quote(str(ready))}\n"
            f"while [ ! -e {shlex.quote(str(release))} ] && [ \"$shutdown\" = false ]; do /bin/sleep 0.05; done\n"
            '[ "$shutdown" = false ] || exit 143\n'
            'controller_pid_temp=$controller_pid_file.tmp.$$\n'
            'printf \'%s\\n\' "$$" > "$controller_pid_temp"\n'
            'finish_private_file "$controller_pid_temp"\n'
            'mv -f "$controller_pid_temp" "$controller_pid_file"\n'
            "wait_for_controller_start_commit\n"
            "trap '' USR1\n"
            "controller_start_lease_committed_matches_parent\n"
            'publish_controller_ready\n'
            'while [ "$shutdown" = false ]; do /bin/sleep 0.1; done\n'
            "exit 0\n",
            encoding="utf-8",
        )
        controller.chmod(0o755)
        if os.getuid() == 0:
            os.chown(controller, self.drop_uid, self.drop_gid)

    def instrument_controller_prelock_barrier(
        self, ready: Path, release: Path, process_pid: Path
    ) -> None:
        controller = self.real_target / "libexec/sdsync-controller"
        source = controller.read_text(encoding="utf-8")
        needle = "acquire_controller_lock\ncontroller_start_lease_matches_parent || {"
        self.assertEqual(source.count(needle), 1)
        replacement = (
            f"printf '%s\\n' \"$$\" > {shlex.quote(str(process_pid))}\n"
            f": > {shlex.quote(str(ready))}\n"
            f"while [ ! -e {shlex.quote(str(release))} ]; do /bin/sleep 0.05; done\n"
            "acquire_controller_lock\ncontroller_start_lease_matches_parent || {"
        )
        controller.write_text(source.replace(needle, replacement), encoding="utf-8")
        controller.chmod(0o755)
        if os.getuid() == 0:
            os.chown(controller, self.drop_uid, self.drop_gid)

    def set_lifecycle_wait_limit(self, occurrence: int, limit: int) -> None:
        source = self.lifecycle.read_text(encoding="utf-8")
        needle = 'while [ "$count" -lt "$startup_timeout" ]; do'
        offset = -1
        for _ in range(occurrence):
            offset = source.find(needle, offset + 1)
            self.assertNotEqual(offset, -1, "lifecycle startup wait site changed")
        replacement = f'while [ "$count" -lt {limit} ]; do'
        self.lifecycle.write_text(
            source[:offset] + replacement + source[offset + len(needle) :],
            encoding="utf-8",
        )

    def instrument_failed_start_cleanup_pause(self, ready: Path, release: Path) -> None:
        source = self.lifecycle.read_text(encoding="utf-8")
        needle = "    remove_own_controller_start_lease >/dev/null 2>&1 || true\n"
        self.assertEqual(source.count(needle), 1)
        replacement = (
            needle
            + f"    : > {shlex.quote(str(ready))}\n"
            + f"    while [ ! -e {shlex.quote(str(release))} ]; do /bin/sleep 0.05; done\n"
        )
        self.lifecycle.write_text(source.replace(needle, replacement), encoding="utf-8")

    def instrument_shell_pause_before(
        self, script: Path, needle: str, ready: Path, release: Path
    ) -> None:
        source = script.read_text(encoding="utf-8")
        self.assertEqual(source.count(needle), 1, f"barrier site changed in {script}")
        barrier = (
            f"        : > {shlex.quote(str(ready))}\n"
            f"        while [ ! -e {shlex.quote(str(release))} ]; do /bin/sleep 0.05; done\n"
        )
        script.write_text(source.replace(needle, barrier + needle), encoding="utf-8")

    def set_failed_start_term_limit(self, limit: int) -> None:
        source = self.lifecycle.read_text(encoding="utf-8")
        needle = 'while [ "$cleanup_count" -lt 10 ]; do'
        self.assertEqual(source.count(needle), 1)
        self.lifecycle.write_text(
            source.replace(needle, f'while [ "$cleanup_count" -lt {limit} ]; do'),
            encoding="utf-8",
        )

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
            ready_deadline = time.monotonic() + 15
            while not ready_file.is_file() and time.monotonic() < ready_deadline:
                if process.poll() is not None:
                    break
                time.sleep(0.01)
            if pid_file.is_file():
                child_pid = int(pid_file.read_text(encoding="utf-8").strip())
            if not ready_file.is_file():
                if process.poll() is None:
                    self.fail("instrumented child did not reach launch window")
                stdout, stderr = process.communicate(timeout=5)
                self.fail(
                    "instrumented child exited before launch window "
                    f"(status {process.returncode}): {stdout}{stderr}"
                )
            try:
                exit_code = process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.terminate()
                process.wait(timeout=5)
                self.fail("parent did not close the launch-assignment signal window")
            stdout, stderr = process.communicate()
            self.assertEqual(exit_code, expected_exit, stdout + stderr)
            terminal_deadline = time.monotonic() + 5
            while time.monotonic() < terminal_deadline:
                child_gone = child_pid is not None
                if child_pid is not None:
                    try:
                        os.kill(child_pid, 0)
                    except ProcessLookupError:
                        child_gone = True
                    else:
                        child_gone = False
                if term_file.is_file() and child_gone:
                    break
                time.sleep(0.01)
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

    def security_policy_options(self, **overrides: object) -> list[str]:
        values: dict[str, object] = {
            "require_https": False,
            "allow_interface_changes": True,
            "allow_profile_changes": True,
            "allow_secret_changes": True,
            "allow_routine_changes": True,
            "allow_notification_changes": True,
            "allow_operational_actions": True,
            "allow_http_targets": True,
            "allow_invalid_tls": True,
            "allow_destructive_sync": True,
            "allow_doctor_write_test": True,
            "allow_remote_logging": True,
            "allow_empty_source": True,
            "csrf_lifetime": 300,
            "result_retention": 3600,
            "max_outstanding_jobs": 256,
            "audit_log_level": "info",
            "bridge_log_level": "info",
            "authentication_log_level": "warn",
            "security_log_level": "warn",
            "configuration_log_level": "info",
            "secrets_log_level": "info",
            "routines_log_level": "info",
            "operations_log_level": "info",
            "notifications_log_level": "warn",
            "sync_log_level": "info",
            "controller_log_level": "info",
            "scheduler_log_level": "info",
        }
        unknown = set(overrides) - set(values)
        self.assertFalse(unknown, f"unknown security-policy test override: {unknown}")
        values.update(overrides)
        options: list[str] = []
        for key, value in values.items():
            options.extend(
                [
                    f"--{key.replace('_', '-')}",
                    str(value).lower() if isinstance(value, bool) else str(value),
                ]
            )
        return options

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
        self.assertIn("audit", {entry["source"] for entry in payload["logs"]})
        rendered = json.dumps(payload)
        self.assertNotIn("\x1b", rendered)
        self.assertNotIn(str(self.real_home), rendered)
        self.assertNotIn(".password", rendered)
        self.assertNotIn("a-secret-that-must-never-appear", rendered)
        self.assertLess(len(logs.stdout), 300_000)
        correlated_request_id = "d" * 32
        activity_log = self.real_var / "log/activity.log"
        with activity_log.open("a", encoding="utf-8") as stream:
            stream.write(
                "10000|audit.requested|logprofile|requested|audit|info|"
                f"{self.drop_uid}|admin|Module configure-profile requested "
                f"[bridge-correlation] request_id={correlated_request_id}\n"
            )
        activity_log.chmod(0o600)
        activity, activity_payload = self.api("activity", "--lines", "10")
        self.assertEqual(activity.returncode, 0, activity.stderr)
        self.assertEqual(activity_payload["schema"], "sdsync.dsm-activity.v1")
        self.assertTrue(
            any(
                event["category"] == "audit" and event["code"].startswith("audit.")
                for event in activity_payload["events"]
            )
        )
        self.assertTrue(
            all("client_request_id" in event for event in activity_payload["events"])
        )
        self.assertTrue(
            any(
                event["client_request_id"] == correlated_request_id
                and correlated_request_id in event["message"]
                for event in activity_payload["events"]
            )
        )

        run_state = self.real_var / "state/run.state"
        run_state.write_text("state=running\nstate=failed\n", encoding="utf-8")
        corrupt, corrupt_payload = self.api("snapshot")
        self.assertEqual(corrupt.returncode, 73)
        self.assertIn(corrupt_payload["code"], {"corrupt_state", "unsafe_state"})

    def test_bridge_audit_correlation_is_exact_durable_and_activity_visible(self) -> None:
        emitter = self.root / "emit-bridge-audit.sh"
        emitter.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            f". {shlex.quote(str(self.real_target / 'libexec/sdsync-common'))}\n"
            "SDSYNC_DSM_AUDIT_ACTOR=${4:-admin}\n"
            f"SDSYNC_DSM_AUDIT_ACTOR_UID={self.drop_uid}\n"
            "SDSYNC_DSM_AUDIT_ORIGIN=bridge\n"
            "SDSYNC_DSM_AUDIT_TRANSACTION=$2\n"
            "SDSYNC_DSM_CLIENT_REQUEST_ID=$1\n"
            "export SDSYNC_DSM_AUDIT_ACTOR SDSYNC_DSM_AUDIT_ACTOR_UID \\\n"
            "    SDSYNC_DSM_AUDIT_ORIGIN SDSYNC_DSM_AUDIT_TRANSACTION \\\n"
            "    SDSYNC_DSM_CLIENT_REQUEST_ID\n"
            "append_audit_event configure-profile \"$3\" personal\n",
            encoding="utf-8",
        )
        emitter.chmod(0o755)
        transaction = "bridge-correlation-" + ("a" * 48)
        client_request_id = "c" * 32
        for state in ("requested", "succeeded"):
            emitted = self.shell(
                emitter,
                client_request_id,
                transaction,
                state,
            )
            self.assertEqual(emitted.returncode, 0, (state, emitted.stderr))

        records = [
            json.loads(line)
            for line in (self.real_var / "log/audit.log")
            .read_text(encoding="utf-8")
            .splitlines()
            if line.strip()
        ]
        correlated = [
            record for record in records if record.get("transaction") == transaction
        ]
        self.assertEqual(
            [record["state"] for record in correlated],
            ["requested", "succeeded"],
        )
        self.assertTrue(
            all(
                record.get("client_request_id") == client_request_id
                for record in correlated
            )
        )

        activity, payload = self.api("activity", "--lines", "100")
        self.assertEqual(activity.returncode, 0, activity.stderr)
        correlated_events = [
            event
            for event in payload["events"]
            if event.get("client_request_id") == client_request_id
        ]
        self.assertEqual(
            {event["state"] for event in correlated_events},
            {"requested", "succeeded"},
        )
        self.assertTrue(
            all(client_request_id in event["message"] for event in correlated_events)
        )

        mismatch = self.shell(
            emitter,
            "d" * 32,
            transaction,
            "requested",
        )
        self.assertEqual(mismatch.returncode, 73, mismatch.stderr)
        for invalid in ("e" * 31, "e" * 33):
            rejected = self.shell(emitter, invalid, transaction + "x", "requested")
            self.assertEqual(rejected.returncode, 64, rejected.stderr)

        directory_actor = "DOMAIN\\" + ("a" * 96) + "@directory.example.test"
        self.assertGreater(len(directory_actor), 64)
        long_actor = self.shell(
            emitter,
            "f" * 32,
            transaction + "-directory",
            "requested",
            directory_actor,
        )
        self.assertEqual(long_actor.returncode, 0, long_actor.stderr)
        exact_utf8_actor = "𐐀" * 64
        self.assertEqual(len(exact_utf8_actor.encode("utf-8")), 256)
        exact_utf8 = self.shell(
            emitter,
            "a" * 32,
            transaction + "-utf8",
            "requested",
            exact_utf8_actor,
        )
        self.assertEqual(exact_utf8.returncode, 0, exact_utf8.stderr)
        for invalid_actor in (exact_utf8_actor + "a", "DOMAIN|administrator"):
            rejected_actor = self.shell(
                emitter,
                "b" * 32,
                transaction + "-invalid-actor",
                "requested",
                invalid_actor,
            )
            self.assertEqual(rejected_actor.returncode, 64, rejected_actor.stderr)

    def test_audit_tail_recovery_repairs_only_the_active_incomplete_record(self) -> None:
        emitter = self.root / "emit-tail-audit.sh"
        emitter.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            f". {shlex.quote(str(self.real_target / 'libexec/sdsync-common'))}\n"
            "SDSYNC_DSM_AUDIT_ACTOR=admin\n"
            f"SDSYNC_DSM_AUDIT_ACTOR_UID={self.drop_uid}\n"
            "SDSYNC_DSM_AUDIT_ORIGIN=bridge\n"
            "SDSYNC_DSM_AUDIT_TRANSACTION=$2\n"
            "SDSYNC_DSM_CLIENT_REQUEST_ID=$1\n"
            "export SDSYNC_DSM_AUDIT_ACTOR SDSYNC_DSM_AUDIT_ACTOR_UID \\\n"
            "    SDSYNC_DSM_AUDIT_ORIGIN SDSYNC_DSM_AUDIT_TRANSACTION \\\n"
            "    SDSYNC_DSM_CLIENT_REQUEST_ID\n"
            "append_audit_event configure-profile \"$3\" personal\n",
            encoding="utf-8",
        )
        emitter.chmod(0o755)
        rejection_emitter = self.root / "emit-rejected-audit.sh"
        rejection_emitter.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            f". {shlex.quote(str(self.real_target / 'libexec/sdsync-common'))}\n"
            "SDSYNC_DSM_AUDIT_ACTOR=admin\n"
            f"SDSYNC_DSM_AUDIT_ACTOR_UID={self.drop_uid}\n"
            "SDSYNC_DSM_AUDIT_ORIGIN=bridge\n"
            "SDSYNC_DSM_AUDIT_TRANSACTION=rejected-before-normal-audit\n"
            "unset SDSYNC_DSM_CLIENT_REQUEST_ID\n"
            "export SDSYNC_DSM_AUDIT_ACTOR SDSYNC_DSM_AUDIT_ACTOR_UID \\\n"
            "    SDSYNC_DSM_AUDIT_ORIGIN SDSYNC_DSM_AUDIT_TRANSACTION\n"
            "append_audit_event rejected-post failed all\n",
            encoding="utf-8",
        )
        rejection_emitter.chmod(0o755)
        audit_log = self.real_var / "log/audit.log"
        activity_log = self.real_var / "log/activity.log"
        client_request_id = "e" * 32

        rejected = self.shell(rejection_emitter)
        self.assertEqual(rejected.returncode, 0, rejected.stderr)

        first_transaction = "tail-first-" + ("a" * 48)
        requested = self.shell(
            emitter, client_request_id, first_transaction, "requested"
        )
        self.assertEqual(requested.returncode, 0, requested.stderr)
        with audit_log.open("ab") as stream:
            stream.write(b'{"partial"')
        with activity_log.open("ab") as stream:
            stream.write(b"partial activity")
        succeeded = self.shell(
            emitter, client_request_id, first_transaction, "succeeded"
        )
        self.assertEqual(succeeded.returncode, 0, succeeded.stderr)
        audit_bytes = audit_log.read_bytes()
        activity_bytes = activity_log.read_bytes()
        self.assertTrue(audit_bytes.endswith(b"\n"))
        self.assertTrue(activity_bytes.endswith(b"\n"))
        self.assertNotIn(b'partial"', audit_bytes)
        self.assertNotIn(b"partial activity", activity_bytes)
        records = [json.loads(line) for line in audit_bytes.splitlines()]
        self.assertEqual(
            [
                record["state"]
                for record in records
                if record["transaction"] == first_transaction
            ],
            ["requested", "succeeded"],
        )
        self.assertIn(
            "Recovered incomplete active audit and activity log tails",
            activity_bytes.decode("utf-8"),
        )

        preserved = dict(records[0])
        preserved["transaction"] = "tail-preserved-" + ("b" * 48)
        with audit_log.open("ab") as stream:
            stream.write(json.dumps(preserved, separators=(",", ":")).encode("utf-8"))
        with activity_log.open("ab") as stream:
            stream.write(
                b"10002|run.succeeded|none|succeeded|operations|info|complete activity tail"
            )
        second_transaction = "tail-second-" + ("c" * 48)
        second = self.shell(
            emitter, client_request_id, second_transaction, "requested"
        )
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertTrue(audit_log.read_bytes().endswith(b"\n"))
        self.assertTrue(activity_log.read_bytes().endswith(b"\n"))
        self.assertTrue(
            any(
                json.loads(line).get("transaction") == preserved["transaction"]
                for line in audit_log.read_bytes().splitlines()
            )
        )
        self.assertIn("complete activity tail", activity_log.read_text(encoding="utf-8"))

        with activity_log.open("ab") as stream:
            stream.write(b"partial activity only")
        third = self.shell(
            emitter,
            client_request_id,
            "tail-third-" + ("d" * 48),
            "requested",
        )
        self.assertEqual(third.returncode, 0, third.stderr)
        self.assertIn(
            "Recovered incomplete active activity log tail",
            activity_log.read_text(encoding="utf-8"),
        )

        before = audit_log.read_bytes()
        with audit_log.open("ab") as stream:
            stream.write(b'{"broken":true}\n')
        malformed = self.shell(
            emitter,
            client_request_id,
            "tail-malformed-" + ("f" * 48),
            "requested",
        )
        self.assertEqual(malformed.returncode, 73, malformed.stderr)
        self.assertEqual(audit_log.read_bytes(), before + b'{"broken":true}\n')

    def test_security_policy_is_complete_private_canonical_and_snapshot_exact(self) -> None:
        configured, configured_payload = self.api(
            "security-policy",
            *self.security_policy_options(
                require_https=True,
                allow_http_targets=False,
                allow_empty_source=False,
                result_retention=7200,
                max_outstanding_jobs=17,
                audit_log_level="debug",
            ),
        )
        self.assertEqual(configured.returncode, 0, configured.stderr)
        self.assertTrue(configured_payload["ok"])
        policy_file = self.real_home / "config/security.conf"
        policy_text = policy_file.read_text(encoding="utf-8")
        self.assertEqual(len(policy_text.splitlines()), 29)
        self.assertEqual(policy_text.splitlines()[0], "policy_version=1")
        self.assertEqual(stat.S_IMODE(policy_file.stat().st_mode), 0o600)

        snapshot, snapshot_payload = self.api("snapshot")
        self.assertEqual(snapshot.returncode, 0, snapshot.stderr)
        policy = snapshot_payload["security_policy"]
        self.assertEqual(policy["policy_version"], 1)
        self.assertTrue(policy["require_https"])
        self.assertFalse(policy["allow_http_targets"])
        self.assertFalse(policy["allow_empty_source"])
        self.assertEqual(policy["result_retention_seconds"], 7200)
        self.assertEqual(policy["max_outstanding_jobs"], 17)
        self.assertEqual(policy["log_levels"]["audit"], "debug")

        partial = self.shell(
            self.manager,
            "configure-security-policy",
            "--require-https",
            "false",
        )
        self.assertNotEqual(partial.returncode, 0)
        self.assertEqual(policy_file.read_text(encoding="utf-8"), policy_text)

        leading_zero = self.shell(
            self.manager,
            "configure-security-policy",
            *self.security_policy_options(csrf_lifetime="060"),
        )
        self.assertNotEqual(leading_zero.returncode, 0)
        self.assertEqual(policy_file.read_text(encoding="utf-8"), policy_text)

        duplicate = self.shell(
            self.manager,
            "configure-security-policy",
            *self.security_policy_options(),
            "--require-https",
            "true",
        )
        self.assertNotEqual(duplicate.returncode, 0)
        self.assertEqual(policy_file.read_text(encoding="utf-8"), policy_text)

        for noncanonical in (
            policy_text.replace("\n", "\r\n").encode("utf-8"),
            policy_text.replace(
                "allow_empty_source=false", "allow_empty_source=fa\rlse"
            ).encode("utf-8"),
            policy_text.replace(
                "allow_empty_source=false", "allow_empty_source=false\0"
            ).encode("utf-8"),
            policy_text.replace(
                "allow_empty_source=false", "allow_empty_source=fa\0lse"
            ).encode("utf-8"),
            policy_text.replace(
                "allow_empty_source", "allow_empty\0_source"
            ).encode("utf-8"),
            policy_text.rstrip("\n").encode("utf-8"),
        ):
            policy_file.write_bytes(noncanonical)
            noncanonical_snapshot, noncanonical_payload = self.api("snapshot")
            self.assertEqual(noncanonical_snapshot.returncode, 73)
            self.assertIn(
                noncanonical_payload["code"], {"corrupt_state", "unsafe_state"}
            )
        policy_file.write_text(policy_text, encoding="utf-8")
        restored_snapshot, _ = self.api("snapshot")
        self.assertEqual(restored_snapshot.returncode, 0, restored_snapshot.stderr)

        policy_hardlink = self.root / "security-policy-hardlink"
        os.link(policy_file, policy_hardlink)
        linked, linked_payload = self.api("snapshot")
        self.assertEqual(linked.returncode, 73)
        self.assertIn(linked_payload["code"], {"corrupt_state", "unsafe_state"})
        policy_hardlink.unlink()

        for unsafe_mode in (0o1600, 0o2600, 0o4600):
            policy_file.chmod(unsafe_mode)
            special, special_payload = self.api("snapshot")
            self.assertEqual(special.returncode, 73)
            self.assertIn(
                special_payload["code"], {"corrupt_state", "unsafe_state"}
            )
        policy_file.chmod(0o600)

        policy_file.chmod(0o640)
        corrupt, corrupt_payload = self.api("snapshot")
        self.assertEqual(corrupt.returncode, 73)
        self.assertIn(corrupt_payload["code"], {"corrupt_state", "unsafe_state"})

    def test_postupgrade_migrates_only_the_exact_legacy_security_policy(self) -> None:
        saved = self.shell(
            self.manager,
            "configure-security-policy",
            *self.security_policy_options(),
        )
        self.assertEqual(saved.returncode, 0, saved.stderr)
        policy = self.real_home / "config/security.conf"
        versioned = policy.read_text(encoding="utf-8")
        legacy = versioned.removeprefix("policy_version=1\n")
        self.assertEqual(len(legacy.splitlines()), 28)
        policy.write_text(legacy, encoding="utf-8")
        policy.chmod(0o600)
        if os.getuid() == 0:
            os.chown(policy, self.drop_uid, self.drop_gid)

        migrated = self.shell(self.lifecycle_dir / "postupgrade")
        self.assertEqual(migrated.returncode, 0, migrated.stderr)
        self.assertEqual(policy.read_text(encoding="utf-8"), versioned)
        audit = self.real_var / "log/audit.log"
        migration_audit = [
            json.loads(line)
            for line in audit.read_text(encoding="utf-8").splitlines()
            if line
            and json.loads(line).get("operation") == "security-policy"
            and json.loads(line).get("actor") == "package-upgrade"
        ]
        self.assertEqual([record["state"] for record in migration_audit], ["requested", "succeeded"])
        self.assertEqual(len({record["transaction"] for record in migration_audit}), 1)
        self.assertTrue(all(record["origin"] == "controller" for record in migration_audit))
        self.assertTrue(all(record["actor_uid"] == self.drop_uid for record in migration_audit))
        activity = self.real_var / "log/activity.log"
        migration_activity = [
            line.split("|", 8)
            for line in activity.read_text(encoding="utf-8").splitlines()
            if "|security.policy_changed|" in line and "|package-upgrade|" in line
        ]
        self.assertEqual(len(migration_activity), 1)
        self.assertEqual(migration_activity[0][2:8], ["all", "changed", "security", "warn", str(self.drop_uid), "package-upgrade"])
        migrated_inode = policy.stat().st_ino
        repeated = self.shell(self.lifecycle_dir / "postupgrade")
        self.assertEqual(repeated.returncode, 0, repeated.stderr)
        self.assertEqual(policy.stat().st_ino, migrated_inode, "v1 migration must be idempotent")
        repeated_audit = [
            json.loads(line)
            for line in audit.read_text(encoding="utf-8").splitlines()
            if line
            and json.loads(line).get("operation") == "security-policy"
            and json.loads(line).get("actor") == "package-upgrade"
        ]
        self.assertEqual(repeated_audit, migration_audit, "unchanged policy is not a mutation")

        invalid_documents = (
            "broken\n",
            legacy.rsplit("\n", 2)[0] + "\n",
            versioned.replace("policy_version=1", "policy_version=2", 1),
        )
        for invalid in invalid_documents:
            policy.write_text(invalid, encoding="utf-8")
            policy.chmod(0o600)
            if os.getuid() == 0:
                os.chown(policy, self.drop_uid, self.drop_gid)
            rejected = self.shell(self.lifecycle_dir / "postupgrade")
            self.assertNotEqual(rejected.returncode, 0)
            self.assertEqual(policy.read_text(encoding="utf-8"), invalid)

        policy.unlink()
        symlink_target = self.root / "security-policy-symlink-target"
        symlink_target.write_text(legacy, encoding="utf-8")
        os.symlink(symlink_target, policy)
        symlink_rejected = self.shell(self.lifecycle_dir / "postupgrade")
        self.assertNotEqual(symlink_rejected.returncode, 0)
        self.assertEqual(symlink_target.read_text(encoding="utf-8"), legacy)
        policy.unlink()

        policy.write_text(legacy, encoding="utf-8")
        policy.chmod(0o600)
        if os.getuid() == 0:
            os.chown(policy, self.drop_uid, self.drop_gid)
        extra_link = self.root / "security-policy-extra-link"
        os.link(policy, extra_link)
        hardlink_rejected = self.shell(self.lifecycle_dir / "postupgrade")
        self.assertNotEqual(hardlink_rejected.returncode, 0)
        self.assertEqual(policy.read_text(encoding="utf-8"), legacy)

    def test_postinst_audits_only_the_initial_disabled_schedule_commit(self) -> None:
        schedule = self.real_home / "config/schedule.conf"
        audit = self.real_var / "log/audit.log"
        activity = self.real_var / "log/activity.log"
        records = [
            json.loads(line)
            for line in audit.read_text(encoding="utf-8").splitlines()
            if line and json.loads(line).get("actor") == "package-install"
        ]
        self.assertEqual([record["operation"] for record in records], ["schedule", "schedule"])
        self.assertEqual([record["state"] for record in records], ["requested", "succeeded"])
        self.assertEqual(len({record["transaction"] for record in records}), 1)
        self.assertTrue(all(record["actor_uid"] == self.drop_uid for record in records))
        install_activity = [
            line.split("|", 8)
            for line in activity.read_text(encoding="utf-8").splitlines()
            if "|package-install|" in line
        ]
        self.assertEqual(len(install_activity), 3)
        self.assertTrue(any(fields[1] == "configuration.changed" for fields in install_activity))
        schedule_inode = schedule.stat().st_ino
        before_audit = audit.read_text(encoding="utf-8")
        before_activity = activity.read_text(encoding="utf-8")
        repeated = self.shell(self.lifecycle_dir / "postinst")
        self.assertEqual(repeated.returncode, 0, repeated.stdout + repeated.stderr)
        self.assertEqual(schedule.stat().st_ino, schedule_inode)
        self.assertEqual(audit.read_text(encoding="utf-8"), before_audit)
        self.assertEqual(activity.read_text(encoding="utf-8"), before_activity)

    def test_runner_enforces_saved_risk_ceilings_and_rejects_dangling_policy(self) -> None:
        configured = self.shell(
            self.manager,
            "configure-profile", "--name", "risky", "--source", str(self.source_one),
            "--url", "http://files.example.test/", "--username", "risk-bot",
            "--remote", "/home/Drive/Risky", "--delete", "--max-delete", "10",
            "--allow-http", "--allow-empty-source", "true",
            "--danger-accept-invalid-certs", "true",
            "--remote-log-url", "https://logs.example.test/ingest",
            "--remote-log-mode", "best-effort", "--default",
        )
        self.assertEqual(configured.returncode, 0, configured.stderr)
        password = self.shell(self.manager, "set-password", "risky", input_text="test-password\n")
        self.assertEqual(password.returncode, 0, password.stderr)
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)

        blocked_cases = (
            ("allow_destructive_sync", "destructive sync"),
            ("allow_http_targets", "HTTP targets"),
            ("allow_invalid_tls", "invalid TLS"),
            ("allow_remote_logging", "remote logging"),
            ("allow_empty_source", "empty-source"),
        )
        for policy_key, message in blocked_cases:
            result, _ = self.api(
                "security-policy",
                *self.security_policy_options(**{policy_key: False}),
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.capture.write_text("", encoding="utf-8")
            blocked = self.shell(self.manager, "plan", "risky")
            self.assertEqual(blocked.returncode, 77, (policy_key, blocked.stderr))
            self.assertIn(message, blocked.stderr)
            self.assertEqual(self.capture.read_text(encoding="utf-8"), "")

        security_policy = self.real_home / "config/security.conf"
        saved_policy = security_policy.read_bytes()
        security_policy.unlink()
        os.symlink(self.root / "missing-security-policy", security_policy)
        self.capture.write_text("", encoding="utf-8")
        runner = self.real_target / "libexec/sdsync-run"
        dangling = self.shell(
            runner, "plan", "risky", "false", "foreground", "-",
            extra_environment={"SDSYNC_DSM_AUDIT_WRAPPED": "true"},
        )
        self.assertEqual(dangling.returncode, 73, dangling.stderr)
        self.assertIn("security policy is not a private complete document", dangling.stderr)
        self.assertEqual(self.capture.read_text(encoding="utf-8"), "")
        security_policy.unlink()
        security_policy.write_bytes(saved_policy)
        security_policy.chmod(0o600)
        if os.getuid() == 0:
            os.chown(security_policy, self.drop_uid, self.drop_gid)
        self.assertEqual(self.shell(self.lifecycle, "stop", timeout=15).returncode, 0)

    def test_saved_security_policy_applies_to_direct_cli_with_recovery_exception(self) -> None:
        self.assertEqual(
            self.configure("personal", self.source_one, "/home/Drive/Personal", True).returncode,
            0,
        )
        self.assertEqual(
            self.shell(self.manager, "set-password", "personal", input_text="test-password\n").returncode,
            0,
        )

        def save_policy(**overrides: object) -> None:
            saved = self.shell(
                self.manager,
                "configure-security-policy",
                *self.security_policy_options(**overrides),
            )
            self.assertEqual(saved.returncode, 0, saved.stderr)

        direct_gate_cases: tuple[tuple[str, tuple[str, ...], str | None], ...] = (
            (
                "allow_profile_changes",
                (
                    "configure-profile", "--name", "blocked-profile", "--source", str(self.source_two),
                    "--url", "https://files.example.test/", "--username", "blocked-bot",
                    "--remote", "/home/Drive/Blocked",
                ),
                None,
            ),
            ("allow_secret_changes", ("set-totp", "personal"), "JBSWY3DPEHPK3PXP\n"),
            (
                "allow_routine_changes",
                (
                    "configure-routine", "--profile", "personal", "--enabled", "true",
                    "--action", "plan", "--mode", "interval", "--interval", "300",
                    "--weekdays", "1,2,3,4,5,6,7", "--window-start", "00:00",
                    "--window-end", "23:59", "--debounce-seconds", "5",
                    "--retry-attempts", "1", "--retry-backoff", "30",
                    "--poll-seconds", "30", "--allow-delete", "false",
                    "--max-total-delete", "100",
                ),
                None,
            ),
            (
                "allow_notification_changes",
                (
                    "configure-alerts", "--enabled", "true", "--on-success", "false",
                    "--on-failure", "true", "--failure-threshold", "1", "--cooldown", "300",
                ),
                None,
            ),
            ("allow_operational_actions", ("plan", "personal"), None),
        )
        for key, arguments, input_text in direct_gate_cases:
            save_policy(**{key: False})
            audit_log = self.real_var / "log/audit.log"
            before_records = len(audit_log.read_text(encoding="utf-8").splitlines())
            self.capture.write_text("", encoding="utf-8")
            blocked = self.shell(self.manager, *arguments, input_text=input_text)
            self.assertEqual(blocked.returncode, 77, (key, blocked.stderr))
            new_records = [
                json.loads(line)
                for line in audit_log.read_text(encoding="utf-8").splitlines()[before_records:]
                if line
            ]
            expected_operation = {
                "configure-profile": "configure-profile",
                "set-totp": "set-totp",
                "configure-routine": "routine",
                "configure-alerts": "alert-policy",
                "plan": "plan",
            }[arguments[0]]
            attempted = [
                record
                for record in new_records
                if record["operation"] == expected_operation
            ]
            self.assertEqual(
                [record["state"] for record in attempted],
                ["requested", "failed"],
                (key, new_records),
            )
            self.assertEqual(len({record["transaction"] for record in attempted}), 1)
            if key == "allow_operational_actions":
                self.assertEqual(self.capture.read_text(encoding="utf-8"), "")
            save_policy()

        risk_cases: tuple[tuple[str, tuple[str, ...]], ...] = (
            ("allow_http_targets", ("--url", "http://files.example.test/", "--allow-http")),
            ("allow_invalid_tls", ("--danger-accept-invalid-certs", "true")),
            ("allow_destructive_sync", ("--delete", "--max-delete", "5")),
            ("allow_remote_logging", ("--remote-log-url", "https://logs.example.test/ingest")),
            ("allow_empty_source", ("--allow-empty-source", "true")),
        )
        for index, (key, extra) in enumerate(risk_cases):
            save_policy(**{key: False})
            name = f"risk-{index}"
            url = "https://files.example.test/"
            base = [
                "configure-profile", "--name", name, "--source", str(self.source_two),
                "--url", url, "--username", "risk-bot", "--remote", f"/home/Drive/{name}",
            ]
            if extra[:1] == ("--url",):
                base[base.index("--url") + 1] = extra[1]
                extra = extra[2:]
            blocked = self.shell(self.manager, *base, *extra)
            self.assertEqual(blocked.returncode, 77, (key, blocked.stderr))
            self.assertFalse((self.real_home / f"config/profiles/{name}.toml").exists())
            save_policy()

        save_policy(allow_profile_changes=False, allow_secret_changes=False)
        recovered = self.shell(
            self.manager,
            "configure-security-policy",
            *self.security_policy_options(),
        )
        self.assertEqual(recovered.returncode, 0, recovered.stderr)
        self.assertEqual(self.configure("after-recovery", self.source_two, "/home/Drive/Recovery").returncode, 0)

    def test_complete_security_policy_recovery_audits_without_trusting_broken_policy(self) -> None:
        policy = self.real_home / "config/security.conf"
        audit = self.real_var / "log/audit.log"
        activity = self.real_var / "log/activity.log"

        def install_private_bytes(contents: bytes) -> None:
            if policy.exists() or policy.is_symlink():
                policy.unlink()
            policy.write_bytes(contents)
            policy.chmod(0o600)
            if os.getuid() == 0:
                os.chown(policy, self.drop_uid, self.drop_gid)

        initial = self.shell(
            self.manager,
            "configure-security-policy",
            *self.security_policy_options(),
        )
        self.assertEqual(initial.returncode, 0, initial.stderr)
        pending_transactions: list[str] = []

        for index, broken_kind in enumerate(("corrupt", "incomplete", "dangling")):
            pending_transaction = f"pending-before-policy-recovery-{index}"
            pending_transactions.append(pending_transaction)
            outbox = self.real_var / "state/audit-outbox"
            outbox.mkdir(mode=0o700, parents=True, exist_ok=True)
            pending = outbox / f"{pending_transaction}.mock-event"
            pending.write_text(
                json.dumps(
                    {
                        "operation": "configure-profile",
                        "profile": f"pending-{index}",
                        "actor": "package-manager",
                        "actor_uid": self.drop_uid,
                        "origin": "manager",
                        "transaction": pending_transaction,
                        "terminal_state": "succeeded",
                    },
                    separators=(",", ":"),
                ),
                encoding="utf-8",
            )
            pending.chmod(0o600)
            if os.getuid() == 0:
                os.chown(outbox, self.drop_uid, self.drop_gid)
                os.chown(pending, self.drop_uid, self.drop_gid)
            if broken_kind == "corrupt":
                install_private_bytes(b"broken\n")
            elif broken_kind == "incomplete":
                install_private_bytes(b"require_https=true\n")
            else:
                if policy.exists() or policy.is_symlink():
                    policy.unlink()
                os.symlink(self.root / "missing-security-policy", policy)

            blocked_name = f"blocked-broken-policy-{index}"
            blocked = self.configure(
                blocked_name,
                self.source_two,
                f"/home/Drive/{blocked_name}",
            )
            self.assertEqual(blocked.returncode, 73, (broken_kind, blocked.stderr))
            self.assertFalse(
                (self.real_home / f"config/profiles/{blocked_name}.toml").exists()
            )

            recovered = self.shell(
                self.manager,
                "configure-security-policy",
                *self.security_policy_options(),
            )
            self.assertEqual(recovered.returncode, 0, (broken_kind, recovered.stderr))
            self.assertTrue(policy.is_file())
            self.assertFalse(policy.is_symlink())
            self.assertEqual(stat.S_IMODE(policy.stat().st_mode), 0o600)
            self.assertFalse(pending.exists())

        audit_records = [
            json.loads(line)
            for line in audit.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
        recovery_records = [
            record
            for record in audit_records
            if record.get("operation") == "security-policy"
            and record.get("actor") == "package-manager"
        ]
        self.assertGreaterEqual(
            sum(record.get("state") == "requested" for record in recovery_records),
            4,
        )
        self.assertGreaterEqual(
            sum(record.get("state") == "succeeded" for record in recovery_records),
            4,
        )
        for transaction in pending_transactions:
            pending_records = [
                record
                for record in audit_records
                if record.get("transaction") == transaction
            ]
            self.assertEqual(
                [record.get("state") for record in pending_records],
                ["requested", "succeeded"],
            )
        activity_text = activity.read_text(encoding="utf-8")
        self.assertIn("|audit.requested|all|requested|audit|", activity_text)
        self.assertIn("|audit.succeeded|all|succeeded|audit|", activity_text)

    def test_stopped_service_allows_only_complete_security_policy_recovery(self) -> None:
        policy = self.real_home / "config/security.conf"
        api = self.real_target / "bin/sdsync-dsm-api"

        def install_broken(kind: str) -> tuple[str, bytes | str]:
            if policy.exists() or policy.is_symlink():
                policy.unlink()
            if kind == "dangling":
                target = self.root / "missing-stopped-policy"
                policy.symlink_to(target)
                return ("link", os.readlink(policy))
            contents = b"broken\n" if kind == "corrupt" else b"require_https=true\n"
            policy.write_bytes(contents)
            policy.chmod(0o600)
            if os.getuid() == 0:
                os.chown(policy, self.drop_uid, self.drop_gid)
            return ("file", contents)

        for broken_kind in ("corrupt", "incomplete", "dangling"):
            closed = self.executable(api, "--service-admission", "close")
            self.assertEqual(closed.returncode, 0, closed.stderr)
            original_kind, original_value = install_broken(broken_kind)
            incomplete_options = self.security_policy_options()[:-2]
            incomplete = self.shell(
                self.manager, "configure-security-policy", *incomplete_options
            )
            self.assertEqual(incomplete.returncode, 64, (broken_kind, incomplete.stderr))
            if original_kind == "link":
                self.assertTrue(policy.is_symlink())
                self.assertEqual(os.readlink(policy), original_value)
            else:
                self.assertFalse(policy.is_symlink())
                self.assertEqual(policy.read_bytes(), original_value)

            recovered = self.shell(
                self.manager,
                "configure-security-policy",
                *self.security_policy_options(),
            )
            self.assertEqual(recovered.returncode, 0, (broken_kind, recovered.stderr))
            self.assertTrue(policy.is_file())
            self.assertFalse(policy.is_symlink())
            started = self.shell(self.lifecycle, "start", timeout=15)
            self.assertEqual(started.returncode, 0, (broken_kind, started.stdout + started.stderr))
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 0)
            stopped = self.shell(self.lifecycle, "stop", timeout=15)
            self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)

    def test_reserved_bridge_environment_cannot_bypass_policy_or_suppress_manager_audit(self) -> None:
        saved = self.shell(
            self.manager,
            "configure-security-policy",
            *self.security_policy_options(allow_profile_changes=False),
        )
        self.assertEqual(saved.returncode, 0, saved.stderr)
        forged = {
            "SDSYNC_DSM_TERMINAL_AUDIT_OWNER": "consumer",
            "SDSYNC_DSM_POLICY_JOB_PATH": str(
                self.real_var / "control/processing" / f"{'a' * 48}.json"
            ),
            "SDSYNC_DSM_AUDIT_WRAPPED": "true",
            "SDSYNC_DSM_AUDIT_REQUESTED": "true",
            "SDSYNC_DSM_AUDIT_ACTOR": "forged-administrator",
            "SDSYNC_DSM_AUDIT_ORIGIN": "bridge",
            "SDSYNC_DSM_AUDIT_TRANSACTION": "f" * 48,
            "SDSYNC_DSM_AUDIT_PROFILE": "all",
            "SDSYNC_DSM_CLIENT_REQUEST_ID": "e" * 32,
        }
        blocked = self.shell(
            self.manager,
            "configure-profile",
            "--name",
            "reserved-bypass",
            "--source",
            str(self.source_two),
            "--url",
            "https://files.example.test/",
            "--username",
            "reserved-bot",
            "--remote",
            "/home/Drive/Reserved",
            extra_environment=forged,
        )
        self.assertEqual(blocked.returncode, 77, blocked.stderr)
        self.assertFalse(
            (self.real_home / "config/profiles/reserved-bypass.toml").exists()
        )

        recovered = self.shell(
            self.manager,
            "configure-security-policy",
            *self.security_policy_options(),
        )
        self.assertEqual(recovered.returncode, 0, recovered.stderr)
        audit_log = self.real_var / "log/audit.log"
        before = len(audit_log.read_text(encoding="utf-8").splitlines())
        applied = self.shell(
            self.manager,
            "configure-profile",
            "--name",
            "reserved-audited",
            "--source",
            str(self.source_two),
            "--url",
            "https://files.example.test/",
            "--username",
            "reserved-bot",
            "--remote",
            "/home/Drive/Reserved",
            extra_environment=forged,
        )
        self.assertEqual(applied.returncode, 0, applied.stderr)
        records = [
            json.loads(line)
            for line in audit_log.read_text(encoding="utf-8").splitlines()[before:]
            if line.strip()
        ]
        profile_records = [
            record
            for record in records
            if record.get("operation") == "configure-profile"
            and record.get("profile") == "reserved-audited"
        ]
        self.assertEqual(
            {record.get("state") for record in profile_records},
            {"requested", "succeeded"},
        )
        self.assertTrue(profile_records)
        self.assertTrue(
            all(record.get("actor") == "package-manager" for record in profile_records)
        )
        self.assertTrue(
            all(record.get("client_request_id") is None for record in profile_records)
        )
        self.assertTrue(all(record.get("origin") == "manager" for record in profile_records))
        self.assertTrue(all(record.get("transaction") != "f" * 48 for record in profile_records))

        password = self.shell(
            self.manager,
            "set-password",
            "reserved-audited",
            input_text="runner-password\n",
        )
        self.assertEqual(password.returncode, 0, password.stderr)
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
        runner_audit_start = len(audit_log.read_text(encoding="utf-8").splitlines())
        plan = self.shell(
            self.manager,
            "plan",
            "reserved-audited",
            extra_environment=forged,
        )
        self.assertEqual(plan.returncode, 0, plan.stderr)
        plan_records = [
            json.loads(line)
            for line in audit_log.read_text(encoding="utf-8").splitlines()[runner_audit_start:]
            if line.strip()
        ]
        self.assertEqual(
            {record.get("state") for record in plan_records if record.get("operation") == "plan"},
            {"requested", "succeeded"},
        )
        self.assertTrue(
            all(
                record.get("actor") == "package-runner"
                and record.get("origin") == "manager"
                and record.get("transaction") != "f" * 48
                for record in plan_records
                if record.get("operation") == "plan"
            )
        )

        direct_scheduled = self.shell(
            self.real_target / "libexec/sdsync-run",
            "plan",
            "reserved-audited",
            "false",
            "scheduled",
            "-",
            extra_environment=forged,
        )
        self.assertEqual(direct_scheduled.returncode, 77, direct_scheduled.stderr)
        scheduled_records = [
            json.loads(line)
            for line in audit_log.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
        self.assertTrue(
            any(
                record.get("operation") == "plan"
                and record.get("state") == "failed"
                and record.get("actor") == "package-runner"
                and record.get("origin") == "manager"
                for record in scheduled_records
            )
        )

    def test_missing_package_var_root_fails_closed_without_a_second_state_layout(self) -> None:
        missing_var = self.root / "missing-package-var"
        result = self.shell(
            self.manager,
            "configure-profile",
            "--name",
            "missing-var",
            "--source",
            str(self.source_two),
            "--url",
            "https://files.example.test/",
            "--username",
            "missing-var-bot",
            "--remote",
            "/home/Drive/MissingVar",
            extra_environment={"SYNOPKG_PKGVAR": str(missing_var)},
        )
        self.assertEqual(result.returncode, 73, result.stderr)
        self.assertFalse(missing_var.exists())

    def test_all_security_categories_emit_real_safe_events_and_off_keeps_minimum_audit(self) -> None:
        trace_levels = {
            f"{category}_log_level": "trace"
            for category in (
                "audit",
                "bridge",
                "authentication",
                "security",
                "configuration",
                "secrets",
                "routines",
                "operations",
                "notifications",
                "sync",
                "controller",
                "scheduler",
            )
        }
        policy_result, _ = self.api(
            "security-policy", *self.security_policy_options(**trace_levels)
        )
        self.assertEqual(policy_result.returncode, 0, policy_result.stderr)
        self.assertEqual(
            self.configure("personal", self.source_one, "/home/Drive/Test", True).returncode,
            0,
        )
        secret_literal = "category-test-secret-that-must-never-appear"
        secret_result, _ = self.api(
            "set-secret",
            "--profile",
            "personal",
            "--kind",
            "password",
            "--mode",
            "replace",
            input_text=secret_literal + "\n",
        )
        self.assertEqual(secret_result.returncode, 0, secret_result.stderr)
        routine_result, _ = self.api(
            "routine",
            "--profile",
            "personal",
            "--enabled",
            "true",
            "--action",
            "sync",
            "--mode",
            "interval",
            "--interval",
            "300",
            "--weekdays",
            "1,2,3,4,5,6,7",
            "--time-window-start",
            "00:00",
            "--time-window-end",
            "23:59",
            "--debounce-seconds",
            "5",
            "--retry-count",
            "1",
            "--retry-backoff-seconds",
            "30",
            "--poll-seconds",
            "30",
            "--allow-delete",
            "false",
            "--max-total-delete",
            "100",
        )
        self.assertEqual(routine_result.returncode, 0, routine_result.stderr)
        alert_result, _ = self.api(
            "alert-policy",
            "--enabled",
            "true",
            "--on-success",
            "false",
            "--on-failure",
            "true",
            "--failure-threshold",
            "2",
            "--cooldown",
            "600",
        )
        self.assertEqual(alert_result.returncode, 0, alert_result.stderr)
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stderr)
        doctor_result = self.shell(self.manager, "doctor", "personal")
        self.assertEqual(doctor_result.returncode, 0, doctor_result.stderr)
        run_result = self.shell(self.manager, "run", "personal")
        self.assertEqual(run_result.returncode, 0, run_result.stderr)
        notification_result, _ = self.api(
            "client-event", "--event", "session-notifications"
        )
        self.assertEqual(notification_result.returncode, 0, notification_result.stderr)

        before_bridge, before_bridge_payload = self.api("activity", "--lines", "1000")
        self.assertEqual(before_bridge.returncode, 0, before_bridge.stderr)
        before_bridge_categories = {
            event["category"] for event in before_bridge_payload["events"]
        }
        self.assertNotIn("authentication", before_bridge_categories)
        self.assertNotIn("bridge", before_bridge_categories)
        forged_bridge_record = self.shell(
            self.manager,
            "api",
            "audit-event",
            "--operation",
            "configure-profile",
            "--state",
            "requested",
            "--profile",
            "personal",
            extra_environment={
                "SDSYNC_DSM_AUDIT_ACTOR": "admin",
                "SDSYNC_DSM_AUDIT_ORIGIN": "bridge",
                "SDSYNC_DSM_AUDIT_TRANSACTION": "a" * 48,
            },
        )
        self.assertEqual(forged_bridge_record.returncode, 77, forged_bridge_record.stderr)
        self.assertEqual(
            self.shell(self.manager, "enable", "--interval", "60").returncode,
            0,
        )
        stopped = self.shell(self.lifecycle, "stop", timeout=15)
        self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)
        clock = self.fast_clock_environment(step=61)
        started = self.shell(self.lifecycle, "start", extra_environment=clock, timeout=15)
        self.assertEqual(started.returncode, 0, started.stderr)

        controller_log = self.real_var / "log/controller.log"
        scheduler_log = self.real_var / "log/scheduler.log"
        for _ in range(300):
            if controller_log.is_file() and scheduler_log.is_file():
                if "scheduled_run" in controller_log.read_text(encoding="utf-8"):
                    break
            time.sleep(0.02)
        else:
            self.fail("structured controller/scheduler events were not emitted")

        activity, activity_payload = self.api("activity", "--lines", "1000")
        self.assertEqual(activity.returncode, 0, activity.stderr)
        categories = {event["category"] for event in activity_payload["events"]}
        self.assertTrue(
            {
                "audit",
                "security",
                "configuration",
                "secrets",
                "routines",
                "operations",
                "notifications",
                "sync",
            }.issubset(categories),
            categories,
        )
        self.assertNotIn(secret_literal, activity.stdout)

        controller_lines = controller_log.read_text(encoding="utf-8").splitlines()
        self.assertTrue(
            all(line.startswith("{") for line in controller_lines if line),
            controller_lines,
        )
        controller_records = [
            json.loads(line)
            for line in controller_lines
            if line
        ]
        scheduler_records = [
            json.loads(line)
            for line in scheduler_log.read_text(encoding="utf-8").splitlines()
            if line
        ]
        self.assertTrue(
            any(record["category"] == "controller" for record in controller_records)
        )
        self.assertTrue(
            any(record["category"] == "scheduler" for record in scheduler_records)
        )
        stopped = self.shell(self.lifecycle, "stop", timeout=15)
        self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)

        audit_log = self.real_var / "log/audit.log"
        audit_records = [
            json.loads(line)
            for line in audit_log.read_text(encoding="utf-8").splitlines()
            if line
        ]
        self.assertTrue(all(record["mandatory"] is True for record in audit_records))
        self.assertTrue(
            any(
                record["operation"] == "configure-profile"
                and record["profile"] == "personal"
                for record in audit_records
            )
        )
        self.assertNotIn(secret_literal, audit_log.read_text(encoding="utf-8"))

        off_levels = {
            f"{category}_log_level": "off"
            for category in (
                "audit",
                "bridge",
                "authentication",
                "security",
                "configuration",
                "secrets",
                "routines",
                "operations",
                "notifications",
                "sync",
                "controller",
                "scheduler",
            )
        }
        off_result, _ = self.api(
            "security-policy", *self.security_policy_options(**off_levels)
        )
        self.assertEqual(off_result.returncode, 0, off_result.stderr)
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
        baseline_audit = len(audit_log.read_text(encoding="utf-8").splitlines())
        activity_log = self.real_var / "log/activity.log"
        baseline_activity = len(activity_log.read_text(encoding="utf-8").splitlines())
        default_result = self.shell(self.manager, "set-default", "personal")
        self.assertEqual(default_result.returncode, 0, default_result.stderr)
        self.assertEqual(
            len(audit_log.read_text(encoding="utf-8").splitlines()),
            baseline_audit + 2,
        )
        self.assertEqual(
            len(activity_log.read_text(encoding="utf-8").splitlines()),
            baseline_activity + 2,
        )

    def test_log_thresholds_suppress_writers_and_return_requested_matching_errors(self) -> None:
        self.assertEqual(
            self.configure("threshold", self.source_one, "/home/Drive/Threshold", True).returncode,
            0,
        )
        password = self.shell(
            self.manager, "set-password", "threshold", input_text="threshold-password\n"
        )
        self.assertEqual(password.returncode, 0, password.stderr)
        enabled = self.shell(self.manager, "enable", "--interval", "60")
        self.assertEqual(enabled.returncode, 0, enabled.stderr)

        off_policy, _ = self.api(
            "security-policy",
            *self.security_policy_options(
                controller_log_level="off", scheduler_log_level="off"
            ),
        )
        self.assertEqual(off_policy.returncode, 0, off_policy.stderr)
        bridge_capture = self.root / "threshold-bridge-capture"
        bridge_lock = self.root / "threshold-bridge-lock"
        self.write_api_mock(queue_capture=bridge_capture, queue_lock=bridge_lock)
        job_id = "e" * 48
        request = self.real_var / f"control/requests/{job_id}.json"
        request.write_text("{}\n", encoding="utf-8")
        request.chmod(0o600)
        if os.getuid() == 0:
            os.chown(request, self.drop_uid, self.drop_gid)
        self.capture.write_text("", encoding="utf-8")
        activity_log = self.real_var / "log/activity.log"
        baseline_service_events = sum(
            "|service." in line
            for line in activity_log.read_text(encoding="utf-8").splitlines()
        )
        fast_environment = self.fast_clock_environment(step=61)
        started = self.shell(
            self.lifecycle, "start", extra_environment=fast_environment, timeout=15
        )
        self.assertEqual(started.returncode, 0, started.stderr)
        response = self.real_var / f"control/responses/{job_id}.json"
        for _ in range(300):
            if (
                response.is_file()
                and "sync --all-profiles" in self.capture.read_text(encoding="utf-8")
            ):
                break
            time.sleep(0.03)
        else:
            self.fail("controller queue or scheduled runner did not execute under off-level fixture")
        stopped = self.shell(self.lifecycle, "stop", timeout=15)
        self.assertEqual(stopped.returncode, 0, stopped.stderr)
        self.assertEqual(
            sum(
                "|service." in line
                for line in activity_log.read_text(encoding="utf-8").splitlines()
            ),
            baseline_service_events,
        )

        controller_log = self.real_var / "log/controller.log"
        scheduler_log = self.real_var / "log/scheduler.log"
        controller_text = (
            controller_log.read_text(encoding="utf-8") if controller_log.exists() else ""
        )
        scheduler_text = (
            scheduler_log.read_text(encoding="utf-8") if scheduler_log.exists() else ""
        )
        self.assertNotIn(job_id, controller_text)
        self.assertNotIn("control_request_dispatched", controller_text)
        self.assertNotIn("control_consumer_completed", controller_text)
        self.assertNotIn("run_started", scheduler_text)
        self.assertNotIn("run_finished", scheduler_text)
        self.assertNotIn("scheduled_run", scheduler_text)

        error_policy, _ = self.api(
            "security-policy",
            *self.security_policy_options(
                controller_log_level="error",
                scheduler_log_level="error",
                sync_log_level="error",
            ),
        )
        self.assertEqual(error_policy.returncode, 0, error_policy.stderr)
        for source, path in (
            ("controller", controller_log),
            ("scheduler", scheduler_log),
            ("sync", self.real_var / "log/sync.log"),
        ):
            records = [
                json.dumps(
                    {
                        "epoch": 1,
                        "level": "error",
                        "category": source,
                        "event": f"older_{source}_error",
                    },
                    separators=(",", ":"),
                )
            ]
            records.extend(
                json.dumps(
                    {
                        "epoch": index + 2,
                        "level": "trace",
                        "category": source,
                        "event": "newer_trace_noise",
                    },
                    separators=(",", ":"),
                )
                for index in range(150)
            )
            path.write_text("\n".join(records) + "\n", encoding="utf-8")
            path.chmod(0o600)
            if os.getuid() == 0:
                os.chown(path, self.drop_uid, self.drop_gid)

        logs, payload = self.api("logs", "--lines", "1")
        self.assertEqual(logs.returncode, 0, logs.stderr)
        by_source = {entry["source"]: entry["lines"] for entry in payload["logs"]}
        for source in ("controller", "scheduler", "sync"):
            self.assertEqual(len(by_source[source]), 1, (source, by_source[source]))
            self.assertIn(f"older_{source}_error", by_source[source][0])
            self.assertNotIn("newer_trace_noise", by_source[source][0])

    def test_direct_doctor_audit_uses_positional_default_and_all_scope(self) -> None:
        self.assertEqual(
            self.configure("personal", self.source_one, "/home/Drive/Personal", True).returncode,
            0,
        )
        self.assertEqual(
            self.configure("archive", self.source_two, "/home/Drive/Archive").returncode,
            0,
        )
        audit_log = self.real_var / "log/audit.log"
        audit_log.write_text("", encoding="utf-8")
        audit_log.chmod(0o600)
        if os.getuid() == 0:
            os.chown(audit_log, self.drop_uid, self.drop_gid)

        for arguments in (("archive",), (), ("--all",)):
            result = self.shell(self.manager, "doctor", *arguments)
            self.assertEqual(result.returncode, 0, result.stderr)

        records = [
            json.loads(line)
            for line in audit_log.read_text(encoding="utf-8").splitlines()
            if line
        ]
        doctor_records = [record for record in records if record["operation"] == "doctor"]
        self.assertEqual(len(doctor_records), 6)
        transactions: dict[str, list[dict[str, object]]] = {}
        for record in doctor_records:
            transactions.setdefault(str(record["transaction"]), []).append(record)
        self.assertEqual(len(transactions), 3)
        terminal_profiles = []
        for transaction_records in transactions.values():
            self.assertEqual(
                {str(record["state"]) for record in transaction_records},
                {"requested", "succeeded"},
            )
            self.assertEqual(
                len({str(record["profile"]) for record in transaction_records}),
                1,
            )
            terminal_profiles.append(str(transaction_records[0]["profile"]))
        self.assertCountEqual(terminal_profiles, ["archive", "personal", "all"])

    def test_direct_routine_enable_and_disable_use_canonical_audit_operations(self) -> None:
        self.assertEqual(
            self.configure("personal", self.source_one, "/home/Drive/Personal", True).returncode,
            0,
        )
        password = self.shell(
            self.manager,
            "set-password",
            "personal",
            input_text="test-password\n",
        )
        self.assertEqual(password.returncode, 0, password.stderr)
        audit_log = self.real_var / "log/audit.log"
        audit_log.write_text("", encoding="utf-8")
        audit_log.chmod(0o600)
        if os.getuid() == 0:
            os.chown(audit_log, self.drop_uid, self.drop_gid)

        routine = self.shell(self.manager, "configure-routine", "--profile", "personal")
        enabled = self.shell(self.manager, "enable", "--interval", "60")
        disabled = self.shell(self.manager, "disable")
        for result in (routine, enabled, disabled):
            self.assertEqual(result.returncode, 0, result.stderr)

        records = [
            json.loads(line)
            for line in audit_log.read_text(encoding="utf-8").splitlines()
            if line
        ]
        self.assertNotIn("configure-routine", {record["operation"] for record in records})
        self.assertNotIn("enable", {record["operation"] for record in records})
        self.assertNotIn("disable", {record["operation"] for record in records})
        by_transaction: dict[str, list[dict[str, object]]] = {}
        for record in records:
            by_transaction.setdefault(str(record["transaction"]), []).append(record)
        self.assertEqual(len(by_transaction), 3)
        operation_pairs = []
        for transaction_records in by_transaction.values():
            self.assertEqual(
                [record["state"] for record in transaction_records],
                ["requested", "succeeded"],
            )
            self.assertEqual(len({record["operation"] for record in transaction_records}), 1)
            operation_pairs.append(str(transaction_records[0]["operation"]))
        self.assertCountEqual(operation_pairs, ["routine", "schedule", "schedule"])

    def test_duplicate_mutation_singletons_cannot_diverge_from_audit_subject(self) -> None:
        audit_log = self.real_var / "log/audit.log"
        before_audit = audit_log.read_bytes()
        before_config = (self.real_home / "config/config.toml").read_bytes() if (
            self.real_home / "config/config.toml"
        ).exists() else None
        duplicate_profile = self.shell(
            self.manager,
            "configure-profile",
            "--name", "decoy",
            "--source", str(self.source_one),
            "--url", "https://files.example.test/",
            "--username", "tester",
            "--remote", "/home/Drive/Decoy",
            "--name", "actual",
        )
        self.assertEqual(duplicate_profile.returncode, 64, duplicate_profile.stderr)
        self.assertIn("duplicate singleton option", duplicate_profile.stderr)
        for name in ("decoy", "actual"):
            self.assertFalse((self.real_home / f"config/profiles.d/{name}.toml").exists())
        self.assertEqual(audit_log.read_bytes(), before_audit)
        config = self.real_home / "config/config.toml"
        self.assertEqual(config.read_bytes() if config.exists() else None, before_config)

        for name, source in (("decoy", self.source_one), ("actual", self.source_two)):
            configured = self.configure(name, source, f"/home/Drive/{name.title()}")
            self.assertEqual(configured.returncode, 0, configured.stderr)
        before_audit = audit_log.read_bytes()
        duplicate_routine = self.shell(
            self.manager,
            "configure-routine",
            "--profile", "decoy",
            "--profile", "actual",
        )
        self.assertEqual(duplicate_routine.returncode, 64, duplicate_routine.stderr)
        self.assertIn("duplicate singleton option", duplicate_routine.stderr)
        self.assertFalse((self.real_home / "config/routines.d/decoy.conf").exists())
        self.assertFalse((self.real_home / "config/routines.d/actual.conf").exists())
        self.assertEqual(audit_log.read_bytes(), before_audit)

        duplicate_source = self.shell(
            self.manager,
            "configure-profile",
            "--name", "unique",
            "--source", str(self.source_one),
            "--source", str(self.source_two),
            "--url", "https://files.example.test/",
            "--username", "tester",
            "--remote", "/home/Drive/Unique",
        )
        self.assertEqual(duplicate_source.returncode, 64, duplicate_source.stderr)
        self.assertFalse((self.real_home / "config/profiles.d/unique.toml").exists())

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

    @unittest.skipUnless(os.getuid() == 0, "cross-UID socket proof requires root test setup")
    def test_rootless_cross_uid_socket_accepts_web_uid_and_rejects_third_uid(self) -> None:
        package_uid, package_gid = 65530, 65530
        web_uid, web_gid = 65531, 65531
        wrong_uid = 65532
        self.root.chmod(0o755)
        socket_root = self.root / "cross-uid-socket"
        socket_root.mkdir(mode=0o755)
        os.chown(socket_root, package_uid, package_gid)
        socket_path = socket_root / "api.sock"
        ready_path = socket_root / "ready"
        observed_path = socket_root / "observed"
        server_source = (
            "import os,socket,struct\n"
            f"path={str(socket_path)!r}\n"
            f"ready={str(ready_path)!r}\n"
            f"observed={str(observed_path)!r}\n"
            "server=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)\n"
            "server.bind(path)\n"
            f"os.chown(path,-1,{web_gid})\n"
            "os.chmod(path,0o660)\n"
            "server.listen(2)\n"
            "open(ready,'w',encoding='utf-8').close()\n"
            "seen=[]\n"
            "for _ in range(2):\n"
            "  connection,_=server.accept()\n"
            "  _pid,uid,_gid=struct.unpack('3i',connection.getsockopt(socket.SOL_SOCKET,socket.SO_PEERCRED,12))\n"
            "  seen.append(uid)\n"
            f"  connection.sendall(b'1' if uid=={web_uid} else b'0')\n"
            "  connection.close()\n"
            "open(observed,'w',encoding='utf-8').write(','.join(map(str,seen)))\n"
        )

        def identity(uid: int, gid: int, groups: list[int]) -> object:
            def drop() -> None:
                os.setgroups(groups)
                os.setgid(gid)
                os.setuid(uid)

            return drop

        server = subprocess.Popen(
            [sys.executable, "-c", server_source],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            preexec_fn=identity(package_uid, package_gid, [web_gid]),
        )
        try:
            deadline = time.monotonic() + 5
            while not ready_path.exists() and time.monotonic() < deadline:
                if server.poll() is not None:
                    break
                time.sleep(0.01)
            if not ready_path.exists():
                self.fail(server.stderr.read() if server.stderr else "socket server did not start")
            if server.poll() is not None:
                self.fail(server.stderr.read() if server.stderr else "socket server exited early")
            client_source = (
                "import socket,sys\n"
                f"s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);s.connect({str(socket_path)!r})\n"
                "sys.stdout.buffer.write(s.recv(1))\n"
            )
            accepted = subprocess.run(
                [sys.executable, "-c", client_source],
                capture_output=True,
                timeout=5,
                check=False,
                preexec_fn=identity(web_uid, web_gid, [web_gid]),
            )
            rejected = subprocess.run(
                [sys.executable, "-c", client_source],
                capture_output=True,
                timeout=5,
                check=False,
                preexec_fn=identity(wrong_uid, web_gid, [web_gid]),
            )
            self.assertEqual(accepted.returncode, 0, accepted.stderr)
            self.assertEqual(accepted.stdout, b"1")
            self.assertEqual(rejected.returncode, 0, rejected.stderr)
            self.assertEqual(rejected.stdout, b"0")
            stdout, stderr = server.communicate(timeout=5)
            self.assertEqual(server.returncode, 0, stdout + stderr)
            self.assertEqual(
                observed_path.read_text(encoding="utf-8"),
                f"{web_uid},{wrong_uid}",
            )
            self.assertNotIn(
                "/proc/{pid}/exe",
                (REPOSITORY / "src/dsm_api.rs").read_text(encoding="utf-8"),
            )
        finally:
            if server.poll() is None:
                server.terminate()
                try:
                    server.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait(timeout=5)

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
            self.lifecycle, ("start",), environment, pid_file, ready, terminated, 0
        )
        stopped = self.shell(self.lifecycle, "stop", timeout=15)
        self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)
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
        initialization_deadline = time.monotonic() + 15
        while time.monotonic() < initialization_deadline:
            if (
                interval_state.is_file()
                and "state=succeeded" in interval_state.read_text(encoding="utf-8")
                and watch_state.is_file()
                and "fingerprint=none" not in watch_state.read_text(encoding="utf-8")
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
        watch_invocation = "sync --profile watch --no-delete"
        before_watch_count = self.capture.read_text(encoding="utf-8").count(
            watch_invocation
        )
        self.assertEqual(before_watch_count, 0, "realtime initialization dispatched work")
        watched_file.write_text("two\n", encoding="utf-8")
        polling_deadline = time.monotonic() + 15
        while time.monotonic() < polling_deadline:
            if (
                self.capture.read_text(encoding="utf-8").count(watch_invocation)
                > before_watch_count
            ):
                break
            time.sleep(0.03)
        else:
            self.fail("realtime polling fallback did not observe a filesystem change")
        captured = self.capture.read_text(encoding="utf-8")
        self.assertIn("sync --profile interval --no-delete", captured)
        self.assertIn("sync --profile watch --no-delete", captured)

    def test_realtime_fingerprint_stage_failure_never_dispatches_partial_state(self) -> None:
        self.assertEqual(
            self.configure("watch", self.source_two, "/home/Drive/Watch", True).returncode,
            0,
        )
        (self.source_two / "first.txt").write_text("one\n", encoding="utf-8")
        (self.source_two / "second.txt").write_text("two\n", encoding="utf-8")
        realtime, _ = self.api(
            "routine", "--profile", "watch", "--enabled", "true", "--action", "sync",
            "--mode", "realtime", "--interval", "60", "--weekdays", "1,2,3,4,5,6,7",
            "--time-window-start", "00:00", "--time-window-end", "23:59",
            "--debounce-seconds", "1", "--retry-count", "1", "--retry-backoff-seconds", "10",
            "--poll-seconds", "5", "--allow-delete", "false", "--max-total-delete", "100",
        )
        self.assertEqual(realtime.returncode, 0, realtime.stderr)
        self.capture.write_text("", encoding="utf-8")

        failing_bin = self.root / "failing-fingerprint-bin"
        failing_bin.mkdir(mode=0o700)
        failing_sort = failing_bin / "sort"
        failing_sort.write_text(
            "#!/bin/sh\n"
            "# Deliberately publish partial stdout before reporting stage failure.\n"
            "/bin/sed -n '1p' \"$1\"\n"
            "exit 69\n",
            encoding="utf-8",
        )
        failing_sort.chmod(0o755)
        if os.getuid() == 0:
            os.chown(failing_bin, self.drop_uid, self.drop_gid)
            os.chown(failing_sort, self.drop_uid, self.drop_gid)

        fast_environment = self.fast_clock_environment(step=31)
        fast_environment["PATH"] = f"{failing_bin}:{fast_environment['PATH']}"
        started = self.shell(
            self.lifecycle, "start", extra_environment=fast_environment, timeout=15
        )
        self.assertEqual(started.returncode, 0, started.stderr)
        controller_log = self.real_var / "log/controller.log"
        for _ in range(200):
            if (
                controller_log.is_file()
                and "routine_poll_unavailable" in controller_log.read_text(encoding="utf-8")
            ):
                break
            time.sleep(0.03)
        else:
            self.fail(
                "controller did not report the failed fingerprint stage: "
                + (controller_log.read_text(encoding="utf-8") if controller_log.exists() else "missing")
            )

        self.assertNotIn(
            "sync --profile watch --no-delete",
            self.capture.read_text(encoding="utf-8"),
        )
        watch_state = self.real_var / "state/routines/watch.state"
        if watch_state.exists():
            self.assertIn("fingerprint=none", watch_state.read_text(encoding="utf-8"))
        self.assertFalse(list((self.real_var / "run").glob("fingerprint.*")))

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
                    "SYNO.SDS.App.SynologyDriveSync.Instance",
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
        debug_policy = self.shell(
            self.manager,
            "configure-security-policy",
            *self.security_policy_options(controller_log_level="debug"),
        )
        self.assertEqual(debug_policy.returncode, 0, debug_policy.stderr)
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

    def test_controller_reconciles_terminal_audit_before_pruning_without_new_work(self) -> None:
        transaction = "controller-reconcile-" + ("a" * 48)
        job_id = "d" * 48
        outbox = self.real_var / "state/audit-outbox"
        pending = outbox / f"{transaction}.mock-event"
        pending_record = {
            "operation": "set-default",
            "profile": "personal",
            "actor": "package-manager",
            "actor_uid": self.drop_uid,
            "origin": "manager",
            "transaction": transaction,
            "terminal_state": "succeeded",
        }
        pending.write_text(
            json.dumps(pending_record, separators=(",", ":")),
            encoding="utf-8",
        )
        pending.chmod(0o600)
        responses = self.real_var / "control/responses"
        response = responses / f"{job_id}.json"
        response.write_text(
            '{"schema":"sdsync.dsm-response.v1","audit_pending":true}\n',
            encoding="utf-8",
        )
        response.chmod(0o600)
        if os.getuid() == 0:
            os.chown(pending, self.drop_uid, self.drop_gid)
            os.chown(response, self.drop_uid, self.drop_gid)
        expired = time.time() - 3700
        os.utime(response, (expired, expired))

        failure_marker = self.root / "audit-reconcile-fails"
        failure_marker.write_text("fail\n", encoding="utf-8")
        failed_start = self.shell(
            self.lifecycle,
            "start",
            extra_environment={
                "SDSYNC_TEST_AUDIT_RECONCILE_FAILURE_MARKER": str(failure_marker)
            },
            timeout=15,
        )
        self.assertNotEqual(failed_start.returncode, 0)
        self.assertTrue(pending.is_file())
        self.assertTrue(response.is_file(), "failed reconciliation must block pruning")

        failure_marker.unlink()
        recovered_start = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(recovered_start.returncode, 0, recovered_start.stderr)
        for _ in range(300):
            if not pending.exists() and not response.exists():
                break
            time.sleep(0.03)
        self.assertFalse(pending.exists(), "controller startup did not reconcile terminal audit")
        self.assertFalse(response.exists(), "reconciled response did not become prunable")
        audit = self.real_var / "log/audit.log"
        records = [
            json.loads(line)
            for line in audit.read_text(encoding="utf-8").splitlines()
            if line and json.loads(line).get("transaction") == transaction
        ]
        self.assertEqual([record["state"] for record in records], ["requested", "succeeded"])

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

        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
        plan = self.shell(self.manager, "plan", "personal")
        self.assertEqual(plan.returncode, 0, plan.stderr)
        captured = self.capture.read_text(encoding="utf-8")
        self.assertIn("plan --profile personal --no-delete", captured)
        conflict = self.shell(self.manager, "plan", "personal", "archive")
        self.assertEqual(conflict.returncode, 64)

    def test_new_profile_names_are_64_bytes_but_legacy_long_profiles_remain_removable(self) -> None:
        for invalid_name in ("n" * 65, "n" * 256):
            rejected = self.configure(
                invalid_name,
                self.source_one,
                "/home/Drive/RejectedLongName",
            )
            self.assertEqual(rejected.returncode, 64, rejected.stderr)
            if len(invalid_name) <= 240:
                self.assertFalse(
                    (self.real_home / f"config/profiles.d/{invalid_name}.toml").exists()
                )

        seed = "legacyseed"
        legacy = "l" * 65
        configured = self.configure(
            seed, self.source_one, "/home/Drive/LegacyLongName", True
        )
        self.assertEqual(configured.returncode, 0, configured.stderr)
        profiles = self.real_home / "config/profiles.d"
        old_fragment = profiles / f"{seed}.toml"
        new_fragment = profiles / f"{legacy}.toml"
        old_fragment.rename(new_fragment)
        new_fragment.write_text(
            new_fragment.read_text(encoding="utf-8").replace(seed, legacy),
            encoding="utf-8",
        )
        config = self.real_home / "config/config.toml"
        config.write_text(
            config.read_text(encoding="utf-8").replace(seed, legacy),
            encoding="utf-8",
        )
        default = self.real_home / "config/default-profile"
        default.write_text(f"{legacy}\n", encoding="utf-8")
        for path in (new_fragment, config, default):
            path.chmod(0o600)
            if os.getuid() == 0:
                os.chown(path, self.drop_uid, self.drop_gid)

        snapshot, payload = self.api("snapshot")
        self.assertEqual(snapshot.returncode, 0, snapshot.stderr)
        self.assertIn(legacy, {profile["name"] for profile in payload["profiles"]})

        audit_log = self.real_var / "log/audit.log"
        before = len(audit_log.read_text(encoding="utf-8").splitlines())
        removed = self.shell(self.manager, "remove-profile", legacy)
        self.assertEqual(removed.returncode, 0, removed.stderr)
        self.assertFalse(new_fragment.exists())
        records = [
            json.loads(line)
            for line in audit_log.read_text(encoding="utf-8").splitlines()[before:]
            if line
        ]
        removed_records = [
            record
            for record in records
            if record["operation"] == "remove-profile" and record["profile"] == legacy
        ]
        self.assertEqual(
            [record["state"] for record in removed_records],
            ["requested", "succeeded"],
        )

    def test_direct_commit_boundaries_report_unknown_and_reload_is_post_commit_warning(self) -> None:
        original_manager = self.manager.read_text(encoding="utf-8")
        audit_log = self.real_var / "log/audit.log"

        def install_manager(contents: str) -> None:
            self.manager.write_text(contents, encoding="utf-8")
            self.manager.chmod(0o755)
            if os.getuid() == 0:
                os.chown(self.manager, self.drop_uid, self.drop_gid)

        def new_audit_records(before: int, operation: str, profile: str) -> list[dict[str, object]]:
            records = [
                json.loads(line)
                for line in audit_log.read_text(encoding="utf-8").splitlines()[before:]
                if line
            ]
            return [
                record
                for record in records
                if record.get("operation") == operation
                and record.get("profile") == profile
            ]

        configured = self.configure(
            "stable", self.source_one, "/home/Drive/Stable", True
        )
        self.assertEqual(configured.returncode, 0, configured.stderr)
        password = self.shell(
            self.manager, "set-password", "stable", input_text="stable-password\n"
        )
        self.assertEqual(password.returncode, 0, password.stderr)

        install_manager(
            original_manager.replace(
                "reload_controller_if_running() {\n",
                "reload_controller_if_running() {\n    return 69\n",
                1,
            )
        )
        before = len(audit_log.read_text(encoding="utf-8").splitlines())
        enabled = self.shell(self.manager, "enable", "--interval", "60")
        self.assertEqual(enabled.returncode, 0, enabled.stderr)
        self.assertIn("controller could not be reloaded", enabled.stderr)
        self.assertIn(
            "enabled=true",
            (self.real_home / "config/schedule.conf").read_text(encoding="utf-8"),
        )
        self.assertEqual(
            [record["state"] for record in new_audit_records(before, "schedule", "all")],
            ["requested", "succeeded"],
        )
        install_manager(original_manager)

        configure_publish = (
            '    mv -f "$fragment_temp" "$profiles_dir/$profile_name.toml"\n'
        )
        self.assertIn(configure_publish, original_manager)
        install_manager(
            original_manager.replace(
                configure_publish,
                configure_publish + "    exit 91\n",
                1,
            )
        )
        before = len(audit_log.read_text(encoding="utf-8").splitlines())
        interrupted = self.configure(
            "partialprofile", self.source_two, "/home/Drive/PartialProfile"
        )
        self.assertEqual(interrupted.returncode, 91, interrupted.stderr)
        self.assertTrue(
            (self.real_home / "config/profiles.d/partialprofile.toml").is_file()
        )
        self.assertNotIn(
            "[profiles.partialprofile]",
            (self.real_home / "config/config.toml").read_text(encoding="utf-8"),
        )
        self.assertEqual(
            [
                record["state"]
                for record in new_audit_records(
                    before, "configure-profile", "partialprofile"
                )
            ],
            ["requested", "outcome_unknown"],
        )
        (self.real_home / "config/profiles.d/partialprofile.toml").unlink()
        install_manager(original_manager)

        removable = self.configure(
            "removeunknown", self.source_two, "/home/Drive/RemoveUnknown"
        )
        self.assertEqual(removable.returncode, 0, removable.stderr)
        removal_publish = (
            '        rm -f "$profiles_dir/$remove_name.toml" "$secret_root/$remove_name.password" "$secret_root/$remove_name.totp" \\\n'
            '            "$secret_root/$remove_name.remote-log-token" "$routines_dir/$remove_name.conf" \\\n'
            '            "$routine_state_dir/$remove_name.state" "$profile_state_dir/$remove_name.state" "$health_dir/$remove_name.state"\n'
        )
        self.assertIn(removal_publish, original_manager)
        install_manager(
            original_manager.replace(
                removal_publish,
                removal_publish + "        exit 92\n",
                1,
            )
        )
        before = len(audit_log.read_text(encoding="utf-8").splitlines())
        removed = self.shell(self.manager, "remove-profile", "removeunknown")
        self.assertEqual(removed.returncode, 92, removed.stderr)
        self.assertFalse(
            (self.real_home / "config/profiles.d/removeunknown.toml").exists()
        )
        self.assertIn(
            "[profiles.removeunknown]",
            (self.real_home / "config/config.toml").read_text(encoding="utf-8"),
        )
        self.assertEqual(
            [
                record["state"]
                for record in new_audit_records(
                    before, "remove-profile", "removeunknown"
                )
            ],
            ["requested", "outcome_unknown"],
        )
        install_manager(original_manager)

    def test_controller_reload_serializes_with_terminal_audit_write(self) -> None:
        self.assertEqual(
            self.configure("reload-race", self.source_one, "/home/Drive/Reload", True).returncode,
            0,
        )
        password = self.root / "reload-race.password"
        password.write_text("test-password\n", encoding="utf-8")
        stored = self.shell(
            self.manager, "set-password", "reload-race", "--from-file", str(password)
        )
        self.assertEqual(stored.returncode, 0, stored.stderr)
        write_ready = self.root / "audit-complete-write.ready"
        write_release = self.root / "audit-complete-write.release"
        reconcile_arm = self.root / "audit-reconcile.arm"
        reconcile_attempt = self.root / "audit-reconcile-lock.attempt"
        self.environment.update(
            {
                "SDSYNC_TEST_AUDIT_RECONCILE_LOCK_ARM": str(reconcile_arm),
                "SDSYNC_TEST_AUDIT_RECONCILE_LOCK_ATTEMPT": str(reconcile_attempt),
            }
        )
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)

        controller_pid_path = self.real_var / "run/controller.pid"
        hidden_controller_pid_path = self.real_var / "run/controller.pid.reload-test"
        controller_pid = int(controller_pid_path.read_text(encoding="ascii").strip())
        # Prevent enable's normal reload from coalescing with the exact HUP
        # injected below. Restore the PID record before readiness validation.
        controller_pid_path.rename(hidden_controller_pid_path)
        enabled = self.shell_process(
            self.manager,
            "enable",
            "--interval",
            "3600",
            extra_environment={
                "SDSYNC_TEST_AUDIT_COMPLETE_WRITE_READY": str(write_ready),
                "SDSYNC_TEST_AUDIT_COMPLETE_WRITE_RELEASE": str(write_release),
            },
        )
        try:
            deadline = time.monotonic() + 10
            while not write_ready.is_file() and enabled.poll() is None and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(write_ready.is_file(), "audit completion did not reach its locked write barrier")

            hidden_controller_pid_path.rename(controller_pid_path)
            os.kill(controller_pid, signal.SIGHUP)
            deadline = time.monotonic() + 10
            while (
                not reconcile_attempt.is_file()
                and enabled.poll() is None
                and time.monotonic() < deadline
            ):
                time.sleep(0.01)
            self.assertTrue(
                reconcile_attempt.is_file(),
                "controller reconciliation did not reach the held audit lock",
            )
            self.assertIsNone(enabled.poll(), "audit completion escaped its injected write barrier")
            status = self.shell(self.lifecycle, "status")
            self.assertEqual(status.returncode, 0, status.stdout + status.stderr)

            write_release.write_text("release\n", encoding="ascii")
            stdout, stderr = enabled.communicate(timeout=15)
            self.assertEqual(enabled.returncode, 0, stdout + stderr)
            self.capture.write_text("", encoding="utf-8")
            planned = self.shell(self.manager, "plan", "--all")
            self.assertEqual(planned.returncode, 0, planned.stderr)
            self.assertIn("plan --all-profiles", self.capture.read_text(encoding="utf-8"))
        finally:
            write_release.touch(exist_ok=True)
            if hidden_controller_pid_path.exists() and not controller_pid_path.exists():
                hidden_controller_pid_path.rename(controller_pid_path)
            if enabled.poll() is None:
                enabled.terminate()
                try:
                    enabled.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    enabled.kill()
                    enabled.wait(timeout=5)

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

        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
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
        if os.getuid() == 0:
            os.chown(management, self.drop_uid, self.drop_gid)
        management_pid = management / "pid"
        management_pid.write_text("99999999\n", encoding="utf-8")
        management_pid.chmod(0o600)
        if os.getuid() == 0:
            os.chown(management_pid, self.drop_uid, self.drop_gid)
        recovered = self.shell(self.manager, "set-password", "personal", input_text="new-password\n")
        self.assertEqual(recovered.returncode, 0, recovered.stderr)
        self.assertFalse(management.exists())

        run_lock = self.real_var / "run/run.lock"
        run_lock.mkdir(mode=0o700)
        if os.getuid() == 0:
            os.chown(run_lock, self.drop_uid, self.drop_gid)
        sleeper = subprocess.Popen(
            ["/bin/sleep", "30"],
            preexec_fn=(
                (lambda: (os.setgroups([]), os.setgid(self.drop_gid), os.setuid(self.drop_uid)))
                if os.getuid() == 0
                else None
            ),
        )
        try:
            run_start = Path(f"/proc/{sleeper.pid}/stat").read_text(encoding="utf-8").split()[21]
            boot_id = Path("/proc/sys/kernel/random/boot_id").read_text(encoding="utf-8").strip()
            run_pid = run_lock / "pid"
            run_pid.write_text(f"{sleeper.pid}\n{run_start}\n{boot_id}\n", encoding="utf-8")
            run_pid.chmod(0o600)
            if os.getuid() == 0:
                os.chown(run_pid, self.drop_uid, self.drop_gid)
            refused = self.configure("personal", self.source_one, "/home/Drive/Changed")
            self.assertEqual(refused.returncode, 75)
            self.assertIn("while PID", refused.stderr)
        finally:
            sleeper.terminate()
            sleeper.wait(timeout=5)
            (run_lock / "pid").unlink(missing_ok=True)
            run_lock.rmdir()

    def test_atomic_lock_waiter_recovers_release_overlap_and_prunes_orphans(self) -> None:
        common = self.real_target / "libexec/sdsync-common"
        source = common.read_text(encoding="utf-8")
        needle = '        if ! read_private_lock_owner "$event_log_lock/pid"; then'
        self.assertEqual(source.count(needle), 1)
        source = source.replace(
            needle,
            '        if [ -n "${SDSYNC_TEST_LOCK_WAITER_READY:-}" ]; then\n'
            '            : > "$SDSYNC_TEST_LOCK_WAITER_READY"\n'
            '            while [ ! -e "$SDSYNC_TEST_LOCK_WAITER_CONTINUE" ]; do /bin/sleep 0.01; done\n'
            '        fi\n'
            '        if ! read_private_lock_owner "$event_log_lock/pid"; then',
        )
        common.write_text(source, encoding="utf-8")

        probe = self.root / "lock-probe"
        probe.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            '. "$SYNOPKG_PKGDEST/libexec/sdsync-common"\n'
            'event_log_lock="$SDSYNC_TEST_LOCK"\n'
            "event_log_lock_held=false\n"
            'if [ "$1" = holder ]; then\n'
            '    acquire_event_log_lock\n'
            '    : > "$SDSYNC_TEST_HOLDER_READY"\n'
            '    while [ ! -e "$SDSYNC_TEST_HOLDER_RELEASE" ]; do /bin/sleep 0.01; done\n'
            '    release_event_log_lock\n'
            "else\n"
            '    acquire_event_log_lock\n'
            '    : > "$SDSYNC_TEST_WAITER_ACQUIRED"\n'
            '    release_event_log_lock\n'
            "fi\n",
            encoding="utf-8",
        )
        probe.chmod(0o755)
        if os.getuid() == 0:
            os.chown(probe, self.drop_uid, self.drop_gid)

        lock = self.real_var / "run/event-contention.lock"
        boot_id = Path("/proc/sys/kernel/random/boot_id").read_text(encoding="utf-8").strip()
        orphan_claim = Path(f"{lock}.claim.99999999.1.{boot_id}")
        orphan_recovery = Path(f"{lock}.recover.99999998.1.{boot_id}")
        orphan_claim.write_bytes(b"")
        orphan_recovery.write_text(f"99999997\n1\n{boot_id}\n", encoding="utf-8")
        for path in (orphan_claim, orphan_recovery):
            path.chmod(0o600)
            if os.getuid() == 0:
                os.chown(path, self.drop_uid, self.drop_gid)

        holder_ready = self.root / "holder.ready"
        holder_release = self.root / "holder.release"
        waiter_ready = self.root / "waiter.ready"
        waiter_continue = self.root / "waiter.continue"
        waiter_acquired = self.root / "waiter.acquired"
        environment = {
            "SDSYNC_TEST_LOCK": str(lock),
            "SDSYNC_TEST_HOLDER_READY": str(holder_ready),
            "SDSYNC_TEST_HOLDER_RELEASE": str(holder_release),
            "SDSYNC_TEST_LOCK_WAITER_READY": str(waiter_ready),
            "SDSYNC_TEST_LOCK_WAITER_CONTINUE": str(waiter_continue),
            "SDSYNC_TEST_WAITER_ACQUIRED": str(waiter_acquired),
        }
        holder = self.shell_process(probe, "holder", extra_environment=environment)
        for _ in range(300):
            if holder_ready.is_file():
                break
            self.assertIsNone(holder.poll(), "holder exited before acquiring lock")
            time.sleep(0.01)
        self.assertTrue(holder_ready.is_file())

        waiter = self.shell_process(probe, "waiter", extra_environment=environment)
        for _ in range(300):
            if waiter_ready.is_file():
                break
            self.assertIsNone(waiter.poll(), "waiter exited before observing live owner")
            time.sleep(0.01)
        self.assertTrue(waiter_ready.is_file())

        holder_release.touch()
        holder_stdout, holder_stderr = holder.communicate(timeout=5)
        self.assertEqual(holder.returncode, 0, holder_stdout + holder_stderr)
        self.assertFalse(lock.exists())
        waiter_continue.touch()
        waiter_stdout, waiter_stderr = waiter.communicate(timeout=5)
        self.assertEqual(waiter.returncode, 0, waiter_stdout + waiter_stderr)
        self.assertTrue(waiter_acquired.is_file())
        self.assertFalse(lock.exists())
        self.assertFalse(orphan_claim.exists())
        self.assertFalse(orphan_recovery.exists())

    def test_atomic_claim_publication_tolerates_live_dual_link_window(self) -> None:
        common = self.real_target / "libexec/sdsync-common"
        source = common.read_text(encoding="utf-8")
        needle = '    ln "$event_pending_path" "$event_claim_path" 2>/dev/null || { rm -f "$event_pending_path"; return 73; }'
        self.assertEqual(source.count(needle), 1)
        source = source.replace(
            needle,
            needle
            + '\n    if [ -n "${SDSYNC_TEST_CLAIM_LINKED:-}" ]; then\n'
            + '        : > "$SDSYNC_TEST_CLAIM_LINKED"\n'
            + '        while [ ! -e "$SDSYNC_TEST_CLAIM_CONTINUE" ]; do /bin/sleep 0.01; done\n'
            + "    fi",
        )
        common.write_text(source, encoding="utf-8")

        probe = self.root / "claim-probe"
        probe.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            '. "$SYNOPKG_PKGDEST/libexec/sdsync-common"\n'
            'event_log_lock="$SDSYNC_TEST_LOCK"\n'
            "event_log_lock_held=false\n"
            "acquire_event_log_lock\n"
            ': > "$SDSYNC_TEST_ACQUIRED"\n'
            "release_event_log_lock\n",
            encoding="utf-8",
        )
        probe.chmod(0o755)
        if os.getuid() == 0:
            os.chown(probe, self.drop_uid, self.drop_gid)

        lock = self.real_var / "run/claim-publication.lock"
        first_linked = self.root / "first.linked"
        first_continue = self.root / "first.continue"
        first_acquired = self.root / "first.acquired"
        second_acquired = self.root / "second.acquired"
        base_environment = {"SDSYNC_TEST_LOCK": str(lock)}
        first_environment = {
            **base_environment,
            "SDSYNC_TEST_CLAIM_LINKED": str(first_linked),
            "SDSYNC_TEST_CLAIM_CONTINUE": str(first_continue),
            "SDSYNC_TEST_ACQUIRED": str(first_acquired),
        }
        first = self.shell_process(probe, extra_environment=first_environment)
        for _ in range(300):
            if first_linked.is_file():
                break
            self.assertIsNone(first.poll(), "first contender exited before claim publication")
            time.sleep(0.01)
        self.assertTrue(first_linked.is_file())

        second = self.shell_process(
            probe,
            extra_environment={
                **base_environment,
                "SDSYNC_TEST_ACQUIRED": str(second_acquired),
            },
        )
        second_stdout, second_stderr = second.communicate(timeout=5)
        self.assertEqual(second.returncode, 0, second_stdout + second_stderr)
        self.assertTrue(second_acquired.is_file())

        first_continue.touch()
        first_stdout, first_stderr = first.communicate(timeout=5)
        self.assertEqual(first.returncode, 0, first_stdout + first_stderr)
        self.assertTrue(first_acquired.is_file())
        self.assertFalse(lock.exists())
        self.assertEqual(list((self.real_var / "run").glob("claim-publication.lock.*")), [])

    def test_live_claim_quarantine_is_retried_until_reaper_finishes(self) -> None:
        common = self.real_target / "libexec/sdsync-common"
        source = common.read_text(encoding="utf-8")
        needle = '            mv "$checked_claim_path" "$quarantine_path" 2>/dev/null || continue'
        self.assertEqual(source.count(needle), 1)
        source = source.replace(
            needle,
            needle
            + '\n            if [ -n "${SDSYNC_TEST_QUARANTINE_READY:-}" ]; then\n'
            + '                : > "$SDSYNC_TEST_QUARANTINE_READY"\n'
            + '                while [ ! -e "$SDSYNC_TEST_QUARANTINE_CONTINUE" ]; do /bin/sleep 0.01; done\n'
            + "            fi",
        )
        common.write_text(source, encoding="utf-8")

        probe = self.root / "quarantine-probe"
        probe.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            '. "$SYNOPKG_PKGDEST/libexec/sdsync-common"\n'
            'event_log_lock="$SDSYNC_TEST_LOCK"\n'
            "event_log_lock_held=false\n"
            "acquire_event_log_lock\n"
            ': > "$SDSYNC_TEST_ACQUIRED"\n'
            "release_event_log_lock\n",
            encoding="utf-8",
        )
        probe.chmod(0o755)
        if os.getuid() == 0:
            os.chown(probe, self.drop_uid, self.drop_gid)

        lock = self.real_var / "run/quarantine-contention.lock"
        boot_id = Path("/proc/sys/kernel/random/boot_id").read_text(encoding="utf-8").strip()
        orphan = Path(f"{lock}.claim.99999999.1.{boot_id}")
        orphan.write_bytes(b"")
        orphan.chmod(0o600)
        if os.getuid() == 0:
            os.chown(orphan, self.drop_uid, self.drop_gid)
        quarantine_ready = self.root / "quarantine.ready"
        quarantine_continue = self.root / "quarantine.continue"
        first_acquired = self.root / "quarantine-first.acquired"
        second_acquired = self.root / "quarantine-second.acquired"
        base_environment = {"SDSYNC_TEST_LOCK": str(lock)}
        first = self.shell_process(
            probe,
            extra_environment={
                **base_environment,
                "SDSYNC_TEST_QUARANTINE_READY": str(quarantine_ready),
                "SDSYNC_TEST_QUARANTINE_CONTINUE": str(quarantine_continue),
                "SDSYNC_TEST_ACQUIRED": str(first_acquired),
            },
        )
        for _ in range(300):
            if quarantine_ready.is_file():
                break
            self.assertIsNone(first.poll(), "reaper exited before quarantine publication")
            time.sleep(0.01)
        self.assertTrue(quarantine_ready.is_file())

        second = self.shell_process(
            probe,
            extra_environment={
                **base_environment,
                "SDSYNC_TEST_ACQUIRED": str(second_acquired),
            },
        )
        time.sleep(0.1)
        self.assertIsNone(second.poll(), "contender treated a live reaper as unsafe")
        quarantine_continue.touch()
        first_stdout, first_stderr = first.communicate(timeout=5)
        second_stdout, second_stderr = second.communicate(timeout=5)
        self.assertEqual(first.returncode, 0, first_stdout + first_stderr)
        self.assertEqual(second.returncode, 0, second_stdout + second_stderr)
        self.assertTrue(first_acquired.is_file())
        self.assertTrue(second_acquired.is_file())
        self.assertFalse(orphan.exists())
        self.assertEqual(list((self.real_var / "run").glob("quarantine-contention.lock.*")), [])

    def test_lock_identity_uses_boot_id_and_reaps_cross_boot_owner(self) -> None:
        if os.getuid() != 0:
            self.skipTest("injected boot identity must remain root-owned like procfs")
        probe = self.root / "boot-lock-probe"
        probe.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            '. "$SYNOPKG_PKGDEST/libexec/sdsync-common"\n'
            'boot_identity_path="$SDSYNC_TEST_BOOT_ID"\n'
            'if [ "$1" = status ]; then\n'
            '    status=0; private_process_lock_is_live "$SDSYNC_TEST_LOCK" || status=$?\n'
            '    exit "$status"\n'
            "fi\n"
            'acquire_private_process_lock "$SDSYNC_TEST_LOCK"\n'
            'release_private_process_lock "$SDSYNC_TEST_LOCK"\n',
            encoding="utf-8",
        )
        probe.chmod(0o755)
        os.chown(probe, self.drop_uid, self.drop_gid)
        injected_boot = self.root / "injected-boot-id"
        current_boot = "11111111-1111-4111-8111-111111111111"
        previous_boot = "22222222-2222-4222-8222-222222222222"
        injected_boot.write_text(current_boot + "\n", encoding="utf-8")
        injected_boot.chmod(0o444)
        os.chown(injected_boot, 0, 0)

        sleeper = subprocess.Popen(
            ["/bin/sleep", "30"],
            preexec_fn=lambda: (os.setgroups([]), os.setgid(self.drop_gid), os.setuid(self.drop_uid)),
        )
        lock = self.real_var / "run/boot-identity.lock"
        try:
            start = Path(f"/proc/{sleeper.pid}/stat").read_text(encoding="utf-8").split()[21]
            environment = {
                "SDSYNC_TEST_BOOT_ID": str(injected_boot),
                "SDSYNC_TEST_LOCK": str(lock),
            }

            def seed_owner(boot: str) -> None:
                lock.mkdir(mode=0o700, exist_ok=True)
                os.chown(lock, self.drop_uid, self.drop_gid)
                owner = lock / "pid"
                owner.write_text(f"{sleeper.pid}\n{start}\n{boot}\n", encoding="utf-8")
                owner.chmod(0o600)
                os.chown(owner, self.drop_uid, self.drop_gid)

            seed_owner(current_boot)
            same_boot = self.shell(probe, "status", extra_environment=environment)
            self.assertEqual(same_boot.returncode, 0, same_boot.stderr)
            (lock / "pid").unlink()
            lock.rmdir()

            seed_owner(previous_boot)
            different_boot = self.shell(probe, "acquire", extra_environment=environment, timeout=8)
            self.assertEqual(different_boot.returncode, 0, different_boot.stderr)
            self.assertFalse(lock.exists())
        finally:
            sleeper.terminate()
            sleeper.wait(timeout=5)
            if lock.is_dir():
                (lock / "pid").unlink(missing_ok=True)
                lock.rmdir()

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
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)

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
        controller = self.real_target / "libexec/sdsync-controller"
        controller.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            '. "$SYNOPKG_PKGDEST/libexec/sdsync-common"\n'
            "require_package_identity\n"
            "ensure_layout\n"
            'acquire_private_process_lock "$controller_lock"\n'
            "cleanup_test_controller() {\n"
            '  remove_own_controller_ready >/dev/null 2>&1 || true\n'
            '  saved_controller_pid=$(read_pid "$controller_pid_file" 2>/dev/null || true)\n'
            '  [ "$saved_controller_pid" != "$$" ] || rm -f "$controller_pid_file"\n'
            '  release_private_process_lock "$controller_lock" >/dev/null 2>&1 || true\n'
            "}\n"
            "trap cleanup_test_controller 0\n"
            'controller_pid_temp=$controller_pid_file.tmp.$$\n'
            'printf \'%s\\n\' "$$" > "$controller_pid_temp"\n'
            'finish_private_file "$controller_pid_temp"\n'
            'mv -f "$controller_pid_temp" "$controller_pid_file"\n'
            "publish_controller_ready\n"
            "set +e\n"
            '"$runner" "$@"\n'
            "runner_status=$?\n"
            "set -e\n"
            'exit "$runner_status"\n',
            encoding="utf-8",
        )
        controller.chmod(0o755)
        if os.getuid() == 0:
            os.chown(controller, self.drop_uid, self.drop_gid)

        api_bridge = self.real_target / "bin/sdsync-dsm-api"
        api_process = subprocess.Popen(
            [str(api_bridge), "--serve"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=self.environment.copy(),
            preexec_fn=(
                (lambda: (os.setgroups([]), os.setgid(self.drop_gid), os.setuid(self.drop_uid)))
                if os.getuid() == 0
                else None
            ),
        )
        api_pid_file = self.real_var / "run/api.pid"
        api_ready_file = self.real_var / "run/api.ready"
        api_socket = Path(self.environment["SDSYNC_TEST_API_SOCKET"])
        ready = self.root / "core-launch.ready"
        pid_file = self.root / "core-launch.pid"
        terminated = self.root / "core-launch.term"
        environment = {
            "SDSYNC_TEST_LAUNCH_READY": str(ready),
            "SDSYNC_TEST_LAUNCHED_PID": str(pid_file),
            "SDSYNC_TEST_TERM_OBSERVED": str(terminated),
        }
        try:
            for _ in range(200):
                if api_socket.exists():
                    break
                if api_process.poll() is not None:
                    self.fail("test API exited before binding its socket")
                time.sleep(0.01)
            else:
                self.fail("test API did not bind its socket")
            api_start = (
                Path(f"/proc/{api_process.pid}/stat")
                .read_text(encoding="ascii")
                .rsplit(") ", 1)[1]
                .split()[19]
            )
            boot = Path("/proc/sys/kernel/random/boot_id").read_text(encoding="ascii").strip()
            api_pid_file.write_text(f"{api_process.pid}\n", encoding="ascii")
            api_ready_file.write_text(
                f"{api_process.pid}\n{api_start}\n{boot}\n", encoding="ascii"
            )
            for path in (api_pid_file, api_ready_file):
                path.chmod(0o600)
                if os.getuid() == 0:
                    os.chown(path, self.drop_uid, self.drop_gid)
            self.assert_injected_launch_is_reaped(
                controller,
                ("sync", "personal", "false", "scheduled", "-"),
                environment,
                pid_file,
                ready,
                terminated,
                143,
            )
            self.assertFalse((self.real_var / "run/run.lock").exists())
        finally:
            if api_process.poll() is None:
                api_process.terminate()
                try:
                    api_process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    api_process.kill()
                    api_process.wait(timeout=5)
            api_pid_file.unlink(missing_ok=True)
            api_ready_file.unlink(missing_ok=True)
            api_socket.unlink(missing_ok=True)

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
            '    if [ "$race_mode" = zombie ]; then\n'
            '      : > "$SDSYNC_TEST_RACE_RELEASE"\n'
            "      zombie_wait=0\n"
            '      while [ -e "/proc/$race_pid" ]; do\n'
            "        zombie_state=$(sed -n 's/^.*) \\([^ ]\\).*/\\1/p' 2>/dev/null < \"/proc/$race_pid/stat\" || true)\n"
            '        case $zombie_state in Z|X|x) break ;; esac\n'
            '        [ "$zombie_wait" -lt 200 ] || exit 86\n'
            "        sleep 0.01\n"
            "        zombie_wait=$((zombie_wait + 1))\n"
            "      done\n"
            "      return 1\n"
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
            'if [ "$race_mode" = gone ] || [ "$race_mode" = zombie ]; then\n'
            '  [ "$race_result" -eq 0 ] || exit 82\n'
            '  pid_slot_absent "$race_pid" || exit 83\n'
            '  [ "$race_mode" != zombie ] || wait "$race_pid"\n'
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
            for mode in ("gone", "zombie", "live"):
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
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
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
            self.lifecycle, ("start",), environment, pid_file, ready, terminated, 0
        )
        stopped = self.shell(self.lifecycle, "stop", timeout=15)
        self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)
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

    def test_lifecycle_serializes_controller_lock_before_pid_publication(self) -> None:
        ready = self.root / "controller-lock.ready"
        release = self.root / "controller-lock.release"
        process_pid = self.root / "controller-lock.process-pid"
        self.write_controller_publication_barrier(ready, release, process_pid)

        def wait_for(path: Path, process: subprocess.Popen[str], label: str) -> None:
            for _ in range(250):
                if path.exists():
                    return
                if process.poll() is not None:
                    stdout, stderr = process.communicate(timeout=5)
                    self.fail(f"{label} exited before barrier: {process.returncode} {stdout} {stderr}")
                time.sleep(0.02)
            self.fail(f"timed out waiting for {label}")

        start = self.shell_process(self.lifecycle, "start")
        wait_for(ready, start, "first start")
        self.assertFalse((self.real_var / "run/controller.pid").exists())
        self.assertFalse((self.real_var / "run/controller.ready").exists())
        for lock_name in ("lifecycle.lock", "controller.lock"):
            owner = self.real_var / "run" / lock_name / "pid"
            fields = owner.read_text(encoding="utf-8").splitlines()
            self.assertEqual(len(fields), 3, lock_name)
            self.assertRegex(fields[0], r"^[1-9][0-9]*$")
            self.assertRegex(fields[1], r"^[1-9][0-9]*$")
            self.assertRegex(fields[2], r"^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$")

        status = self.shell_process(self.lifecycle, "status")
        time.sleep(0.2)
        self.assertIsNone(status.poll(), "status misclassified the lock-without-PID transition")
        release.touch()
        start_stdout, start_stderr = start.communicate(timeout=15)
        self.assertEqual(start.returncode, 0, (start_stdout, start_stderr))
        status_stdout, status_stderr = status.communicate(timeout=15)
        self.assertEqual(status.returncode, 0, (status_stdout, status_stderr))
        self.assertEqual(self.shell(self.lifecycle, "stop", timeout=15).returncode, 0)

        ready.unlink(missing_ok=True)
        release.unlink(missing_ok=True)
        process_pid.unlink(missing_ok=True)
        start = self.shell_process(self.lifecycle, "start")
        wait_for(ready, start, "second start")
        controller_process = int(process_pid.read_text(encoding="utf-8").strip())
        self.assertFalse((self.real_var / "run/controller.pid").exists())

        stop = self.shell_process(self.lifecycle, "stop")
        time.sleep(0.2)
        self.assertIsNone(stop.poll(), "stop reported success before controller PID publication")
        self.assertIsNone(start.poll(), "start exited while its publication barrier was held")
        release.touch()
        start_stdout, start_stderr = start.communicate(timeout=15)
        self.assertEqual(start.returncode, 0, (start_stdout, start_stderr))
        stop_stdout, stop_stderr = stop.communicate(timeout=15)
        self.assertEqual(stop.returncode, 0, (stop_stdout, stop_stderr))
        self.assertFalse((self.real_var / "run/controller.ready").exists())

        for _ in range(100):
            try:
                os.kill(controller_process, 0)
            except ProcessLookupError:
                break
            time.sleep(0.02)
        else:
            self.fail("controller survived a successful concurrent stop")
        self.assertFalse((self.real_target / "ui/api.sock").exists())
        self.assertFalse((self.real_var / "run/api.pid").exists())
        self.assertFalse((self.real_var / "run/controller.pid").exists())
        self.assertFalse((self.real_var / "run/controller.lock").exists())
        self.assertFalse((self.real_var / "run/lifecycle.lock").exists())
        self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)

    def test_stop_recovers_live_controller_lock_owner_after_start_sigkill_before_pid(self) -> None:
        ready = self.root / "controller-orphan.ready"
        release = self.root / "controller-orphan.release"
        process_pid = self.root / "controller-orphan.pid"
        self.write_controller_publication_barrier(ready, release, process_pid)
        start = self.shell_process(self.lifecycle, "start")
        controller_pid: int | None = None
        api_pid: int | None = None
        try:
            for _ in range(250):
                if ready.exists():
                    break
                if start.poll() is not None:
                    stdout, stderr = start.communicate(timeout=5)
                    self.fail(f"start exited before controller barrier: {start.returncode} {stdout} {stderr}")
                time.sleep(0.02)
            else:
                self.fail("timed out waiting for controller pre-PID barrier")
            controller_pid = int(process_pid.read_text(encoding="ascii").strip())
            api_pid = int((self.real_var / "run/api.pid").read_text(encoding="ascii").strip())
            owner = self.real_var / "run/controller.lock/pid"
            owner_fields = owner.read_text(encoding="ascii").splitlines()
            self.assertEqual(int(owner_fields[0]), controller_pid)
            self.assertEqual(len(owner_fields), 3)
            self.assertFalse((self.real_var / "run/controller.pid").exists())
            self.assertFalse((self.real_var / "run/controller.ready").exists())
            self.assertTrue((self.real_var / "run/api.bound").is_file())
            self.assertFalse((self.real_var / "run/api.ready").exists())
            start.kill()
            start.communicate(timeout=5)
            self.assertEqual(start.returncode, -signal.SIGKILL)

            stopped = self.shell(self.lifecycle, "stop", timeout=15)
            self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)
            for captured in (controller_pid, api_pid):
                for _ in range(200):
                    try:
                        os.kill(captured, 0)
                    except ProcessLookupError:
                        break
                    time.sleep(0.01)
                else:
                    self.fail(f"orphaned service PID {captured} survived stop")
            for path in (
                self.real_var / "run/controller.pid",
                self.real_var / "run/controller.ready",
                self.real_var / "run/controller.lock",
                self.real_var / "run/lifecycle.lock",
                self.real_var / "run/api.pid",
                self.real_var / "run/api.bound",
                self.real_var / "run/api.ready",
                self.real_target / "ui/api.sock",
            ):
                self.assertFalse(path.exists(), path)
            self.assertFalse(release.exists())
        finally:
            if start.poll() is None:
                start.kill()
                start.communicate(timeout=5)
            for captured in (controller_pid, api_pid):
                if captured is None:
                    continue
                try:
                    os.kill(captured, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            release.touch(exist_ok=True)
            self.shell(self.lifecycle, "stop", timeout=15)

    def test_controller_parent_death_before_lock_cancels_startup_without_orphan(self) -> None:
        ready = self.root / "controller-pre-lock.ready"
        release = self.root / "controller-pre-lock.release"
        process_pid = self.root / "controller-pre-lock.pid"
        self.instrument_controller_prelock_barrier(ready, release, process_pid)
        start = self.shell_process(self.lifecycle, "start")
        controller_pid: int | None = None
        api_pid: int | None = None
        try:
            for _ in range(250):
                if ready.exists():
                    break
                if start.poll() is not None:
                    stdout, stderr = start.communicate(timeout=5)
                    self.fail(f"start exited before controller pre-lock barrier: {start.returncode} {stdout} {stderr}")
                time.sleep(0.02)
            else:
                self.fail("timed out waiting for controller pre-lock barrier")
            controller_pid = int(process_pid.read_text(encoding="ascii").strip())
            api_pid = int((self.real_var / "run/api.pid").read_text(encoding="ascii").strip())
            self.assertTrue((self.real_var / "run/controller.starting").is_file())
            self.assertFalse((self.real_var / "run/controller.lock").exists())
            self.assertFalse((self.real_var / "run/controller.pid").exists())
            self.assertFalse((self.real_var / "run/controller.ready").exists())
            start.kill()
            start.communicate(timeout=5)
            self.assertEqual(start.returncode, -signal.SIGKILL)
            stopped = self.shell(self.lifecycle, "stop", timeout=15)
            self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)
            for captured in (controller_pid, api_pid):
                for _ in range(200):
                    try:
                        os.kill(captured, 0)
                    except ProcessLookupError:
                        break
                    time.sleep(0.01)
                else:
                    self.fail(f"pre-lock service PID {captured} survived parent death and stop")
            release.touch()
            time.sleep(0.1)
            for path in (
                self.real_var / "run/controller.starting",
                self.real_var / "run/controller.lock",
                self.real_var / "run/controller.pid",
                self.real_var / "run/controller.ready",
                self.real_var / "run/api.pid",
                self.real_var / "run/api.bound",
                self.real_var / "run/api.ready",
                self.real_target / "ui/api.sock",
                self.real_var / "run/lifecycle.lock",
            ):
                self.assertFalse(path.exists(), path)
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)
        finally:
            if start.poll() is None:
                start.kill()
                start.communicate(timeout=5)
            release.touch(exist_ok=True)
            for captured in (controller_pid, api_pid):
                if captured is None:
                    continue
                try:
                    os.kill(captured, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            self.shell(self.lifecycle, "stop", timeout=15)

    def test_stop_rejects_forged_live_controller_lock_owner_without_signaling(self) -> None:
        sleeper = subprocess.Popen(
            ["/bin/sleep", "30"],
            preexec_fn=(
                (lambda: (os.setgroups([]), os.setgid(self.drop_gid), os.setuid(self.drop_uid)))
                if os.getuid() == 0
                else None
            ),
        )
        controller_lock = self.real_var / "run/controller.lock"
        owner = controller_lock / "pid"
        try:
            controller_lock.mkdir(mode=0o700)
            start = Path(f"/proc/{sleeper.pid}/stat").read_text(encoding="ascii").rsplit(") ", 1)[1].split()[19]
            boot = Path("/proc/sys/kernel/random/boot_id").read_text(encoding="ascii").strip()
            owner.write_text(f"{sleeper.pid}\n{start}\n{boot}\n", encoding="ascii")
            owner.chmod(0o600)
            if os.getuid() == 0:
                os.chown(controller_lock, self.drop_uid, self.drop_gid)
                os.chown(owner, self.drop_uid, self.drop_gid)
            original = owner.lstat()
            contents = owner.read_bytes()
            refused = self.shell(self.lifecycle, "stop", timeout=15)
            self.assertEqual(refused.returncode, 1, refused.stdout + refused.stderr)
            self.assertIn("unverified controller lock owner", refused.stdout)
            self.assertIsNone(sleeper.poll(), "forged lock owner was signaled")
            current = owner.lstat()
            self.assertEqual((current.st_dev, current.st_ino), (original.st_dev, original.st_ino))
            self.assertEqual(owner.read_bytes(), contents)
            self.assertFalse((self.real_var / "run/controller.pid").exists())
            self.assertFalse((self.real_var / "run/controller.ready").exists())
        finally:
            if sleeper.poll() is None:
                sleeper.terminate()
                sleeper.wait(timeout=5)
            owner.unlink(missing_ok=True)
            if controller_lock.exists():
                controller_lock.rmdir()

    def test_supervised_api_dies_with_start_parent_before_pid_publication(self) -> None:
        ready = self.root / "api-pre-pid.ready"
        release = self.root / "api-pre-pid.release"
        start = self.shell_process(
            self.lifecycle,
            "start",
            extra_environment={
                "SDSYNC_TEST_API_PRE_PID_READY": str(ready),
                "SDSYNC_TEST_API_PRE_PID_RELEASE": str(release),
            },
        )
        api_pid: int | None = None
        try:
            for _ in range(250):
                if ready.exists():
                    break
                if start.poll() is not None:
                    stdout, stderr = start.communicate(timeout=5)
                    self.fail(f"start exited before API barrier: {start.returncode} {stdout} {stderr}")
                time.sleep(0.02)
            else:
                self.fail("timed out waiting for API pre-PID barrier")
            api_pid = int(ready.read_text(encoding="ascii").strip())
            for path in (
                self.real_var / "run/api.pid",
                self.real_var / "run/api.bound",
                self.real_var / "run/api.ready",
                self.real_target / "ui/api.sock",
                self.real_var / "run/controller.pid",
                self.real_var / "run/controller.lock",
            ):
                self.assertFalse(path.exists(), path)
            start.kill()
            start.communicate(timeout=5)
            self.assertEqual(start.returncode, -signal.SIGKILL)
            stopped = self.shell(self.lifecycle, "stop", timeout=15)
            self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)
            release.touch()
            for _ in range(200):
                try:
                    os.kill(api_pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.01)
            else:
                self.fail("supervised API survived death of its pre-publication parent")
            time.sleep(0.1)
            for path in (
                self.real_var / "run/api.pid",
                self.real_var / "run/api.ready",
                self.real_target / "ui/api.sock",
                self.real_var / "run/lifecycle.lock",
            ):
                self.assertFalse(path.exists(), path)
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)
        finally:
            if start.poll() is None:
                start.kill()
                start.communicate(timeout=5)
            release.touch(exist_ok=True)
            if api_pid is not None:
                try:
                    os.kill(api_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_failed_start_cleanup_catches_api_publishing_after_timeout(self) -> None:
        pre_pid_ready = self.root / "api-cleanup-pre-pid.ready"
        pre_pid_release = self.root / "api-cleanup-pre-pid.release"
        cleanup_ready = self.root / "api-cleanup.ready"
        cleanup_release = self.root / "api-cleanup.release"
        self.set_lifecycle_wait_limit(1, 2)
        self.instrument_failed_start_cleanup_pause(cleanup_ready, cleanup_release)
        start = self.shell_process(
            self.lifecycle,
            "start",
            extra_environment={
                "SDSYNC_TEST_API_PRE_PID_READY": str(pre_pid_ready),
                "SDSYNC_TEST_API_PRE_PID_RELEASE": str(pre_pid_release),
            },
        )
        api_pid: int | None = None
        try:
            for _ in range(300):
                if pre_pid_ready.exists():
                    break
                time.sleep(0.02)
            else:
                self.fail("API did not reach its pre-PID cleanup barrier")
            api_pid = int(pre_pid_ready.read_text(encoding="ascii").strip())
            for _ in range(300):
                if cleanup_ready.exists():
                    break
                if start.poll() is not None:
                    self.fail("failed start exited before entering cleanup")
                time.sleep(0.02)
            else:
                self.fail("start did not enter failed-start cleanup")
            pre_pid_release.touch()
            api_pid_file = self.real_var / "run/api.pid"
            for _ in range(300):
                if api_pid_file.exists():
                    break
                time.sleep(0.01)
            else:
                self.fail("API did not publish during the cleanup pause")
            self.assertEqual(int(api_pid_file.read_text(encoding="ascii").strip()), api_pid)
            cleanup_release.touch()
            stdout, stderr = start.communicate(timeout=15)
            self.assertEqual(start.returncode, 1, stdout + stderr)
            for _ in range(300):
                try:
                    os.kill(api_pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.01)
            else:
                self.fail("API that published during cleanup survived failed start")
            for path in (
                self.real_var / "run/controller.starting",
                self.real_var / "run/api.pid",
                self.real_var / "run/api.bound",
                self.real_var / "run/api.ready",
                self.real_target / "ui/api.sock",
                self.real_var / "run/lifecycle.lock",
            ):
                self.assertFalse(path.exists(), path)
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)
        finally:
            pre_pid_release.touch(exist_ok=True)
            cleanup_release.touch(exist_ok=True)
            if start.poll() is None:
                start.kill()
                start.communicate(timeout=5)
            if api_pid is not None:
                try:
                    os.kill(api_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_failed_start_cleanup_catches_controller_exec_during_cleanup(self) -> None:
        controller_ready = self.root / "controller-cleanup-pre-lock.ready"
        controller_release = self.root / "controller-cleanup-pre-lock.release"
        controller_pid_file = self.root / "controller-cleanup-pre-lock.pid"
        cleanup_ready = self.root / "controller-cleanup.ready"
        cleanup_release = self.root / "controller-cleanup.release"
        self.instrument_controller_prelock_barrier(
            controller_ready, controller_release, controller_pid_file
        )
        self.set_lifecycle_wait_limit(2, 2)
        self.instrument_failed_start_cleanup_pause(cleanup_ready, cleanup_release)
        start = self.shell_process(self.lifecycle, "start")
        controller_pid: int | None = None
        api_pid: int | None = None
        try:
            for _ in range(300):
                if controller_ready.exists():
                    break
                if start.poll() is not None:
                    stdout, stderr = start.communicate(timeout=5)
                    self.fail(f"start exited before controller barrier: {stdout} {stderr}")
                time.sleep(0.02)
            else:
                self.fail("controller did not reach its pre-lock cleanup barrier")
            controller_pid = int(controller_pid_file.read_text(encoding="ascii").strip())
            api_pid = int((self.real_var / "run/api.pid").read_text(encoding="ascii").strip())
            for _ in range(300):
                if cleanup_ready.exists():
                    break
                time.sleep(0.02)
            else:
                self.fail("controller timeout did not enter cleanup")
            controller_release.touch()
            time.sleep(0.1)
            cleanup_release.touch()
            stdout, stderr = start.communicate(timeout=15)
            self.assertEqual(start.returncode, 1, stdout + stderr)
            for captured in (controller_pid, api_pid):
                for _ in range(300):
                    try:
                        os.kill(captured, 0)
                    except ProcessLookupError:
                        break
                    time.sleep(0.01)
                else:
                    self.fail(f"transitioning child {captured} survived failed-start cleanup")
            for path in (
                self.real_var / "run/controller.starting",
                self.real_var / "run/controller.lock",
                self.real_var / "run/controller.pid",
                self.real_var / "run/controller.ready",
                self.real_var / "run/api.pid",
                self.real_var / "run/api.bound",
                self.real_var / "run/api.ready",
                self.real_target / "ui/api.sock",
                self.real_var / "run/lifecycle.lock",
            ):
                self.assertFalse(path.exists(), path)
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)
        finally:
            controller_release.touch(exist_ok=True)
            cleanup_release.touch(exist_ok=True)
            if start.poll() is None:
                start.kill()
                start.communicate(timeout=5)
            for captured in (controller_pid, api_pid):
                if captured is None:
                    continue
                try:
                    os.kill(captured, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_failed_start_hard_stops_term_resistant_child_after_commit(self) -> None:
        post_commit_ready = self.root / "api-post-commit.ready"
        post_commit_release = self.root / "api-post-commit.release"
        self.set_lifecycle_wait_limit(3, 2)
        self.set_failed_start_term_limit(1)
        start = self.shell_process(
            self.lifecycle,
            "start",
            extra_environment={
                "SDSYNC_TEST_API_POST_COMMIT_READY": str(post_commit_ready),
                "SDSYNC_TEST_API_POST_COMMIT_RELEASE": str(post_commit_release),
            },
        )
        api_pid: int | None = None
        controller_pid: int | None = None
        try:
            for _ in range(400):
                if post_commit_ready.exists():
                    break
                if start.poll() is not None:
                    stdout, stderr = start.communicate(timeout=5)
                    self.fail(f"start exited before post-commit barrier: {stdout} {stderr}")
                time.sleep(0.02)
            else:
                self.fail("API did not reach post-commit TERM-resistant barrier")
            api_pid = int(post_commit_ready.read_text(encoding="ascii").strip())
            controller_pid = int(
                (self.real_var / "run/controller.pid").read_text(encoding="ascii").strip()
            )
            lease = (self.real_var / "run/controller.starting").read_text(encoding="ascii")
            self.assertTrue(lease.endswith("committed\n"))
            stdout, stderr = start.communicate(timeout=15)
            self.assertEqual(start.returncode, 1, stdout + stderr)
            for captured in (api_pid, controller_pid):
                for _ in range(300):
                    try:
                        os.kill(captured, 0)
                    except ProcessLookupError:
                        break
                    time.sleep(0.01)
                else:
                    self.fail(f"post-commit failed-start child {captured} survived")
            for path in (
                self.real_var / "run/controller.starting",
                self.real_var / "run/controller.lock",
                self.real_var / "run/controller.pid",
                self.real_var / "run/controller.ready",
                self.real_var / "run/api.pid",
                self.real_var / "run/api.bound",
                self.real_var / "run/api.ready",
                self.real_target / "ui/api.sock",
                self.real_var / "run/lifecycle.lock",
            ):
                self.assertFalse(path.exists(), path)
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)
        finally:
            post_commit_release.touch(exist_ok=True)
            if start.poll() is None:
                start.kill()
                start.communicate(timeout=5)
            for captured in (api_pid, controller_pid):
                if captured is None:
                    continue
                try:
                    os.kill(captured, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_failed_start_does_not_signal_mismatched_launched_identity(self) -> None:
        pre_pid_ready = self.root / "api-mismatch-pre-pid.ready"
        pre_pid_release = self.root / "api-mismatch-pre-pid.release"
        term_observed = self.root / "api-mismatch.term"
        self.set_lifecycle_wait_limit(1, 2)
        source = self.lifecycle.read_text(encoding="utf-8")
        needle = "        launched_api_boot=$api_supervisor_boot\n"
        self.assertEqual(source.count(needle), 1)
        self.lifecycle.write_text(
            source.replace(needle, needle + "        launched_api_start=1\n"),
            encoding="utf-8",
        )
        start = self.shell_process(
            self.lifecycle,
            "start",
            extra_environment={
                "SDSYNC_TEST_API_PRE_PID_READY": str(pre_pid_ready),
                "SDSYNC_TEST_API_PRE_PID_RELEASE": str(pre_pid_release),
                "SDSYNC_TEST_API_TERM_OBSERVED": str(term_observed),
            },
        )
        api_pid: int | None = None
        try:
            for _ in range(300):
                if pre_pid_ready.exists():
                    break
                time.sleep(0.02)
            else:
                self.fail("API did not reach mismatched-identity barrier")
            api_pid = int(pre_pid_ready.read_text(encoding="ascii").strip())
            stdout, stderr = start.communicate(timeout=10)
            self.assertEqual(start.returncode, 1, stdout + stderr)
            self.assertIn("live or uninspectable", stdout)
            self.assertFalse(term_observed.exists(), "mismatched PID identity was signaled")
            for _ in range(300):
                try:
                    os.kill(api_pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.01)
            else:
                self.fail("parent-death supervision did not retire mismatched test child")
            self.assertFalse((self.real_var / "run/controller.starting").exists())
            self.assertFalse((self.real_var / "run/lifecycle.lock").exists())
        finally:
            pre_pid_release.touch(exist_ok=True)
            if start.poll() is None:
                start.kill()
                start.communicate(timeout=5)
            if api_pid is not None:
                try:
                    os.kill(api_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_controller_preflight_can_exceed_ten_seconds_with_bounded_deadline(self) -> None:
        lifecycle_source = self.lifecycle.read_text(encoding="utf-8")
        self.assertIn("startup_timeout=${SDSYNC_DSM_START_TIMEOUT:-120}", lifecycle_source)
        for invalid in ("0", "601", "invalid"):
            refused = self.shell(
                self.lifecycle,
                "start",
                extra_environment={"SDSYNC_DSM_START_TIMEOUT": invalid},
            )
            self.assertEqual(refused.returncode, 1, refused.stdout + refused.stderr)
            self.assertFalse((self.real_var / "run/api.pid").exists())

        controller = self.real_target / "libexec/sdsync-controller"
        source = controller.read_text(encoding="utf-8")
        needle = (
            "reconcile_audit_outbox || {\n"
            "    controller_log_event error audit_reconcile_failed \"phase=startup\""
        )
        self.assertEqual(source.count(needle), 1)
        controller.write_text(
            source.replace(needle, "/bin/sleep 11\n" + needle),
            encoding="utf-8",
        )
        controller.chmod(0o755)
        if os.getuid() == 0:
            os.chown(controller, self.drop_uid, self.drop_gid)
        started_at = time.monotonic()
        started = self.shell(
            self.lifecycle,
            "start",
            timeout=30,
            extra_environment={"SDSYNC_DSM_START_TIMEOUT": "20"},
        )
        elapsed = time.monotonic() - started_at
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
        self.assertGreaterEqual(elapsed, 10.0)
        self.assertEqual(self.shell(self.lifecycle, "status").returncode, 0)
        stopped = self.shell(self.lifecycle, "stop", timeout=15)
        self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)
        self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)

    def test_lifecycle_serialization_lock_tampering_fails_closed(self) -> None:
        runtime = self.real_var / "run"
        lifecycle_lock = runtime / "lifecycle.lock"
        outside = self.root / "outside-lifecycle-lock"
        outside.mkdir(mode=0o700)
        lifecycle_lock.symlink_to(outside, target_is_directory=True)
        try:
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 2)
            self.assertEqual(self.shell(self.lifecycle, "start").returncode, 1)
            self.assertEqual(self.shell(self.lifecycle, "stop").returncode, 1)
            self.assertTrue(lifecycle_lock.is_symlink())
            self.assertEqual(list(outside.iterdir()), [])
        finally:
            lifecycle_lock.unlink(missing_ok=True)

        lifecycle_lock.mkdir(mode=0o700)
        malformed_owner = lifecycle_lock / "pid"
        malformed_owner.write_text("not-a-process-identity\n", encoding="utf-8")
        malformed_owner.chmod(0o600)
        if os.getuid() == 0:
            os.chown(lifecycle_lock, self.drop_uid, self.drop_gid)
            os.chown(malformed_owner, self.drop_uid, self.drop_gid)
        try:
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 2)
            self.assertEqual(self.shell(self.lifecycle, "start").returncode, 1)
            self.assertEqual(self.shell(self.lifecycle, "stop").returncode, 1)
            self.assertEqual(malformed_owner.read_text(encoding="utf-8"), "not-a-process-identity\n")
        finally:
            malformed_owner.unlink(missing_ok=True)
            lifecycle_lock.rmdir()

    def test_noop_stop_does_not_fabricate_restart_telemetry(self) -> None:
        marker = self.real_var / "state/service-stopped.marker"
        activity = self.real_var / "log/activity.log"
        baseline = [
            line for line in activity.read_text(encoding="utf-8").splitlines() if "|service." in line
        ]
        self.assertEqual(self.shell(self.lifecycle, "stop", timeout=15).returncode, 0)
        self.assertFalse(marker.exists())
        self.assertEqual(
            [line for line in activity.read_text(encoding="utf-8").splitlines() if "|service." in line],
            baseline,
        )

        self.assertEqual(self.shell(self.lifecycle, "start", timeout=15).returncode, 0)
        self.assertEqual(self.shell(self.lifecycle, "stop", timeout=15).returncode, 0)
        self.assertTrue(marker.is_file())
        before = [line for line in activity.read_text(encoding="utf-8").splitlines() if "|service." in line]
        marker_inode = marker.stat().st_ino
        self.assertEqual(self.shell(self.lifecycle, "stop", timeout=15).returncode, 0)
        after = [line for line in activity.read_text(encoding="utf-8").splitlines() if "|service." in line]
        self.assertEqual(after, before)
        self.assertEqual(marker.stat().st_ino, marker_inode)

    def test_stop_reaps_exact_controller_and_api_artifacts_after_sigkill(self) -> None:
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
        controller_pid = int(
            (self.real_var / "run/controller.pid").read_text(encoding="utf-8").strip()
        )
        api_pid = int((self.real_var / "run/api.pid").read_text(encoding="utf-8").strip())
        os.kill(controller_pid, signal.SIGKILL)
        os.kill(api_pid, signal.SIGKILL)
        for _ in range(200):
            live = []
            for pid in (controller_pid, api_pid):
                try:
                    os.kill(pid, 0)
                    live.append(pid)
                except ProcessLookupError:
                    pass
            if not live:
                break
            time.sleep(0.01)
        stopped = self.shell(self.lifecycle, "stop", timeout=15)
        self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)
        self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)
        for path in (
            self.real_var / "run/controller.pid",
            self.real_var / "run/controller.ready",
            self.real_var / "run/controller.lock",
            self.real_var / "run/api.pid",
            self.real_var / "run/api.ready",
            self.real_target / "ui/api.sock",
        ):
            self.assertFalse(path.exists(), path)

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
        controller_pid = int(
            (self.real_var / "run/controller.pid").read_text(encoding="utf-8").strip()
        )
        controller_ready = self.real_var / "run/controller.ready"
        ready_fields = controller_ready.read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(ready_fields), 3)
        self.assertEqual(int(ready_fields[0]), controller_pid)
        self.assertEqual(
            ready_fields[1], Path(f"/proc/{controller_pid}/stat").read_text(encoding="utf-8").split()[21]
        )
        self.assertEqual(
            ready_fields[2], Path("/proc/sys/kernel/random/boot_id").read_text(encoding="utf-8").strip()
        )
        self.assertEqual(controller_ready.stat().st_mode & 0o7777, 0o600)
        self.assertEqual(controller_ready.stat().st_nlink, 1)
        self.assertEqual(controller_ready.stat().st_uid, self.drop_uid)
        api_ready = self.real_var / "run/api.ready"
        api_ready_fields = api_ready.read_text(encoding="utf-8").splitlines()
        api_pid = int(api_pid_file.read_text(encoding="ascii").strip())
        self.assertEqual(int(api_ready_fields[0]), api_pid)
        self.assertEqual(len(api_ready_fields), 3)
        self.assertEqual(api_ready.stat().st_mode & 0o7777, 0o600)
        self.assertEqual(api_ready.stat().st_nlink, 1)
        self.assertEqual(api_ready.stat().st_uid, self.drop_uid)
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
        self.assertFalse(api_ready.exists())
        self.assertFalse(controller_ready.exists())
        restart_marker = self.real_var / "state/service-stopped.marker"
        self.assertTrue(restart_marker.is_file())
        restarted = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(restarted.returncode, 0, restarted.stdout + restarted.stderr)
        self.assertFalse(restart_marker.exists())
        service_events = [
            line.split("|", 8)
            for line in (self.real_var / "log/activity.log").read_text(encoding="utf-8").splitlines()
            if "|service." in line
        ]
        self.assertEqual(
            [fields[1] for fields in service_events],
            ["service.started", "service.stopped", "service.started", "service.restarted"],
        )
        for fields in service_events:
            self.assertEqual(fields[2], "none")
            self.assertEqual(fields[4], "controller")
            self.assertEqual(fields[6], str(self.drop_uid))
            self.assertEqual(fields[7], "package-service")
        self.assertEqual(self.shell(self.lifecycle, "stop", timeout=15).returncode, 0)

        run_lock = self.real_var / "run/run.lock"
        run_lock.mkdir(mode=0o700)
        (run_lock / "pid").write_text(f"{os.getpid()}\n", encoding="utf-8")
        self.assertEqual(self.shell(self.lifecycle_dir / "preupgrade").returncode, 1)
        self.assertEqual(self.shell(self.lifecycle_dir / "preuninst").returncode, 1)
        (run_lock / "pid").unlink()
        run_lock.rmdir()

    def test_transition_marker_linearizes_manager_runner_and_official_upgrade_hooks(self) -> None:
        manager_ready = self.root / "manager-before-lock.ready"
        manager_release = self.root / "manager-before-lock.release"
        self.instrument_shell_pause_before(
            self.manager,
            '    acquire_private_process_lock "$management_lock" || {\n',
            manager_ready,
            manager_release,
        )
        pending = self.shell_process(
            self.manager,
            "configure-profile", "--name", "late", "--source", str(self.source_one),
            "--url", "https://files.example.test/", "--username", "late-bot",
            "--remote", "/home/Drive/Late",
        )
        try:
            for _ in range(200):
                if manager_ready.exists():
                    break
                if pending.poll() is not None:
                    stdout, stderr = pending.communicate(timeout=5)
                    self.fail(f"manager exited before admission barrier: {stdout} {stderr}")
                time.sleep(0.02)
            else:
                self.fail("manager did not reach its pre-lock barrier")
            closed = self.shell(
                self.lifecycle,
                "stop",
                extra_environment={"SYNOPKG_PKG_STATUS": "UPGRADE"},
            )
            self.assertEqual(closed.returncode, 0, closed.stdout + closed.stderr)
            preupgrade = self.shell(self.lifecycle_dir / "preupgrade")
            self.assertEqual(preupgrade.returncode, 0, preupgrade.stdout + preupgrade.stderr)
            marker = self.real_var / "run/package.transition"
            self.assertEqual(marker.read_text(encoding="ascii"), "upgrade\n")
            manager_release.touch()
            stdout, stderr = pending.communicate(timeout=10)
            self.assertEqual(pending.returncode, 75, stdout + stderr)
            self.assertFalse((self.real_home / "config/profiles.d/late.toml").exists())
        finally:
            manager_release.touch(exist_ok=True)
            if pending.poll() is None:
                pending.kill()
                pending.communicate(timeout=5)

        old_preuninst = self.shell(
            self.lifecycle_dir / "preuninst",
            extra_environment={"SYNOPKG_PKG_STATUS": "UPGRADE"},
        )
        self.assertEqual(old_preuninst.returncode, 0, old_preuninst.stdout + old_preuninst.stderr)
        (self.real_home / "config/schedule.conf").unlink()
        new_postinst = self.shell(
            self.lifecycle_dir / "postinst",
            extra_environment={"SYNOPKG_PKG_STATUS": "UPGRADE"},
        )
        self.assertEqual(new_postinst.returncode, 0, new_postinst.stdout + new_postinst.stderr)
        self.assertEqual(marker.read_text(encoding="ascii"), "upgrade\n")
        upgrade_activity = (self.real_var / "log/activity.log").read_text(encoding="utf-8")
        self.assertIn("|package-upgrade|Package upgrade initialized missing scheduling state as disabled", upgrade_activity)
        denied = self.configure("still-closed", self.source_one, "/home/Drive/Closed")
        self.assertEqual(denied.returncode, 75, denied.stdout + denied.stderr)
        upgraded = self.shell(self.lifecycle_dir / "postupgrade")
        self.assertEqual(upgraded.returncode, 0, upgraded.stdout + upgraded.stderr)
        self.assertFalse(marker.exists())

        reopened = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(reopened.returncode, 0, reopened.stdout + reopened.stderr)
        configured = self.configure("admitted", self.source_one, "/home/Drive/Admitted", True)
        self.assertEqual(configured.returncode, 0, configured.stderr)
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
        runner = self.real_target / "libexec/sdsync-run"
        runner_ready = self.root / "runner-before-lifecycle.ready"
        runner_release = self.root / "runner-before-lifecycle.release"
        self.instrument_shell_pause_before(
            runner, "runner_lifecycle_status=0\n", runner_ready, runner_release
        )
        self.capture.write_text("", encoding="utf-8")
        pending_run = self.shell_process(self.manager, "plan", "admitted")
        try:
            for _ in range(200):
                if runner_ready.exists():
                    break
                if pending_run.poll() is not None:
                    stdout, stderr = pending_run.communicate(timeout=5)
                    self.fail(f"runner exited before admission barrier: {stdout} {stderr}")
                time.sleep(0.02)
            else:
                self.fail("runner did not reach its admission barrier")
            stopped = self.shell(self.lifecycle, "stop", timeout=15)
            self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)
            runner_release.touch()
            stdout, stderr = pending_run.communicate(timeout=10)
            self.assertEqual(pending_run.returncode, 75, stdout + stderr)
            self.assertNotIn(" plan ", f" {self.capture.read_text(encoding='utf-8')} ")
        finally:
            runner_release.touch(exist_ok=True)
            if pending_run.poll() is None:
                pending_run.kill()
                pending_run.communicate(timeout=5)

    def test_preupgrade_adopts_exact_released_helper_without_new_marker_cli(self) -> None:
        legacy_versions = ("26.7", "26.8", "26.9", "26.10")
        for legacy_tag in legacy_versions[:-1]:
            for relative in (
                "packaging/synology/package/libexec/sdsync-common",
                "packaging/synology/package/libexec/sdsync-run",
                "packaging/synology/scripts/common",
                "packaging/synology/scripts/start-stop-status",
            ):
                earlier = subprocess.run(
                    ["git", "show", f"{legacy_tag}:{relative}"],
                    cwd=REPOSITORY,
                    check=True,
                    capture_output=True,
                ).stdout
                floor = subprocess.run(
                    ["git", "show", f"26.10:{relative}"],
                    cwd=REPOSITORY,
                    check=True,
                    capture_output=True,
                ).stdout
                if relative.endswith("sdsync-common"):
                    # 26.10 changed only the DSM notification application ID;
                    # lifecycle, run-lock, and core-launch behavior stayed exact.
                    earlier = earlier.replace(
                        b"com.supermarsx.SynologyDriveSync",
                        b"SYNO.SDS.App.SynologyDriveSync.Instance",
                    )
                self.assertEqual(earlier, floor, (legacy_tag, relative))
        old_common = subprocess.run(
            ["git", "show", "26.10:packaging/synology/package/libexec/sdsync-common"],
            cwd=REPOSITORY,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        installed_common = self.real_target / "libexec/sdsync-common"
        installed_common.write_text(old_common, encoding="utf-8")
        installed_common.chmod(0o755)
        old_api = self.real_target / "bin/sdsync-dsm-api"
        old_api.write_text("#!/bin/sh\nexit 64\n", encoding="utf-8")
        old_api.chmod(0o755)
        if os.getuid() == 0:
            os.chown(installed_common, self.drop_uid, self.drop_gid)
            os.chown(old_api, self.drop_uid, self.drop_gid)

        legacy_run_lock = self.real_var / "run/run.lock"
        legacy_run_lock.mkdir(mode=0o700)
        legacy_pid = legacy_run_lock / "pid"
        legacy_pid.write_text("99999999\n", encoding="ascii")
        legacy_pid.chmod(0o600)
        if os.getuid() == 0:
            os.chown(legacy_run_lock, self.drop_uid, self.drop_gid)
            os.chown(legacy_pid, self.drop_uid, self.drop_gid)

        runtime_before = sorted(
            str(path.relative_to(self.real_var))
            for path in self.real_var.rglob("*")
        )
        for legacy_version in legacy_versions:
            adopted = self.shell(
                self.lifecycle_dir / "preupgrade",
                extra_environment={"SYNOPKG_OLD_PKGVER": f"{legacy_version}-1"},
            )
            self.assertEqual(adopted.returncode, 0, (legacy_version, adopted.stdout + adopted.stderr))
            self.assertEqual(
                sorted(str(path.relative_to(self.real_var)) for path in self.real_var.rglob("*")),
                runtime_before,
                "preupgrade must remain a read-only DSM acceptance hook",
            )
        below_floor = self.shell(
            self.lifecycle_dir / "preupgrade",
            extra_environment={"SYNOPKG_OLD_PKGVER": "26.6-1"},
        )
        self.assertNotEqual(below_floor.returncode, 0)
        marker = self.real_var / "run/package.transition"
        self.assertFalse(marker.exists())

        shutil.copy2(HERE / "package/libexec/sdsync-common", installed_common)
        installed_common.chmod(0o755)
        self.write_api_mock()
        if os.getuid() == 0:
            os.chown(installed_common, self.drop_uid, self.drop_gid)
        postinst = self.shell(
            self.lifecycle_dir / "postinst",
            extra_environment={
                "SYNOPKG_PKG_STATUS": "UPGRADE",
                "SYNOPKG_OLD_PKGVER": "26.7-1",
            },
        )
        self.assertEqual(postinst.returncode, 0, postinst.stdout + postinst.stderr)
        self.assertFalse(legacy_run_lock.exists())
        self.assertEqual(marker.read_text(encoding="ascii"), "upgrade\n")
        postupgrade = self.shell(self.lifecycle_dir / "postupgrade")
        self.assertEqual(postupgrade.returncode, 0, postupgrade.stdout + postupgrade.stderr)
        self.assertFalse(marker.exists())

    def test_preupgrade_rejects_exact_legacy_orphan_core_before_adopting_stale_lock(self) -> None:
        installed_common = self.real_target / "libexec/sdsync-common"
        old_common = subprocess.run(
            ["git", "show", "26.10:packaging/synology/package/libexec/sdsync-common"],
            cwd=REPOSITORY,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        installed_common.write_text(old_common, encoding="utf-8")
        installed_common.chmod(0o755)
        old_api = self.real_target / "bin/sdsync-dsm-api"
        old_api.write_text("#!/bin/sh\nexit 64\n", encoding="utf-8")
        old_api.chmod(0o755)
        core = self.real_target / "bin/synology-drive-sync"
        saved_core = core.read_bytes()
        saved_core_mode = stat.S_IMODE(core.stat().st_mode)
        shutil.copy2("/bin/sleep", core)
        core.chmod(0o755)
        if os.getuid() == 0:
            for path in (installed_common, old_api, core):
                os.chown(path, self.drop_uid, self.drop_gid)

        core_pid_file = self.root / "legacy-orphan-core.pid"
        legacy_runner = self.root / "legacy-runner-fixture.sh"
        legacy_runner.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            "umask 077\n"
            'mkdir "$SYNOPKG_PKGVAR/run/run.lock"\n'
            'chmod 0700 "$SYNOPKG_PKGVAR/run/run.lock"\n'
            'printf "%s\\n" "$$" > "$SYNOPKG_PKGVAR/run/run.lock/pid"\n'
            'chmod 0600 "$SYNOPKG_PKGVAR/run/run.lock/pid"\n'
            '"$SYNOPKG_PKGDEST/bin/synology-drive-sync" 120 </dev/null >/dev/null 2>&1 &\n'
            'core_pid=$!\n'
            'printf "%s\\n" "$core_pid" > "$SDSYNC_TEST_CORE_PID_FILE"\n'
            'wait "$core_pid"\n',
            encoding="utf-8",
        )
        legacy_runner.chmod(0o755)
        old_scripts = self.root / "old-lifecycle"
        old_scripts.mkdir(mode=0o700)
        for name in ("common", "start-stop-status"):
            source = subprocess.run(
                ["git", "show", f"26.10:packaging/synology/scripts/{name}"],
                cwd=REPOSITORY,
                check=True,
                capture_output=True,
                text=True,
            ).stdout
            target = old_scripts / name
            target.write_text(source, encoding="utf-8")
            target.chmod(0o755)
        if os.getuid() == 0:
            for path in (legacy_runner, old_scripts, *old_scripts.iterdir()):
                os.chown(path, self.drop_uid, self.drop_gid)

        environment = self.environment.copy()
        environment["SDSYNC_TEST_CORE_PID_FILE"] = str(core_pid_file)
        runner = subprocess.Popen(
            ["/bin/sh", str(legacy_runner)],
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
        core_pid: int | None = None
        try:
            for _ in range(300):
                if core_pid_file.is_file():
                    core_pid = int(core_pid_file.read_text(encoding="ascii").strip())
                    break
                if runner.poll() is not None:
                    stdout, stderr = runner.communicate(timeout=5)
                    self.fail(f"legacy runner exited before core publication: {stdout} {stderr}")
                time.sleep(0.02)
            else:
                self.fail("legacy runner did not publish its core PID")
            os.kill(runner.pid, signal.SIGKILL)
            runner.communicate(timeout=5)
            old_stop = self.shell(old_scripts / "start-stop-status", "stop", timeout=15)
            self.assertEqual(old_stop.returncode, 0, old_stop.stdout + old_stop.stderr)
            self.assertTrue((self.real_var / "run/run.lock").is_dir())
            blocked = self.shell(
                self.lifecycle_dir / "preupgrade",
                extra_environment={"SYNOPKG_OLD_PKGVER": "26.10-1"},
            )
            self.assertNotEqual(blocked.returncode, 0, blocked.stdout + blocked.stderr)
            os.kill(core_pid, signal.SIGKILL)
            for _ in range(300):
                if not Path(f"/proc/{core_pid}").exists():
                    break
                time.sleep(0.02)
            else:
                self.fail("legacy orphan core did not terminate")
            accepted = self.shell(
                self.lifecycle_dir / "preupgrade",
                extra_environment={"SYNOPKG_OLD_PKGVER": "26.10-1"},
            )
            self.assertEqual(accepted.returncode, 0, accepted.stdout + accepted.stderr)
        finally:
            if runner.poll() is None:
                runner.kill()
                runner.communicate(timeout=5)
            if core_pid is not None and Path(f"/proc/{core_pid}").exists():
                try:
                    os.kill(core_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                for _ in range(300):
                    if not Path(f"/proc/{core_pid}").exists():
                        break
                    time.sleep(0.02)
            core.write_bytes(saved_core)
            core.chmod(saved_core_mode)
            shutil.copy2(HERE / "package/libexec/sdsync-common", installed_common)
            installed_common.chmod(0o755)
            self.write_api_mock()
            if os.getuid() == 0:
                os.chown(core, self.drop_uid, self.drop_gid)
                os.chown(installed_common, self.drop_uid, self.drop_gid)
        adopted = self.shell(
            self.lifecycle_dir / "postinst",
            extra_environment={
                "SYNOPKG_PKG_STATUS": "UPGRADE",
                "SYNOPKG_OLD_PKGVER": "26.10-1",
            },
        )
        self.assertEqual(adopted.returncode, 0, adopted.stdout + adopted.stderr)
        self.assertFalse((self.real_var / "run/run.lock").exists())

    def test_failed_start_marker_keeps_unresolved_child_inconsistent_until_stop(self) -> None:
        ready = self.root / "failed-marker-api.ready"
        self.set_lifecycle_wait_limit(3, 2)
        self.set_failed_start_term_limit(1)
        source = self.lifecycle.read_text(encoding="utf-8")
        needle = '    kill -KILL "$hard_stop_pid" 2>/dev/null || true\n'
        self.assertEqual(source.count(needle), 1)
        self.lifecycle.write_text(
            source.replace(needle, "    return 75\n"),
            encoding="utf-8",
        )
        started = self.shell_process(
            self.lifecycle,
            "start",
            extra_environment={"SDSYNC_TEST_API_POST_COMMIT_READY": str(ready)},
        )
        api_pid: int | None = None
        try:
            for _ in range(300):
                if ready.exists():
                    api_pid = int(ready.read_text(encoding="ascii").strip())
                    break
                if started.poll() is not None:
                    stdout, stderr = started.communicate(timeout=5)
                    self.fail(f"start exited before failed-marker barrier: {stdout} {stderr}")
                time.sleep(0.02)
            else:
                self.fail("API did not reach failed-marker barrier")
            stdout, stderr = started.communicate(timeout=15)
            self.assertEqual(started.returncode, 1, stdout + stderr)
            marker = self.real_var / "run/failed-start.api"
            self.assertTrue(marker.is_file())
            self.assertIn(f"api\n{api_pid}\n", marker.read_text(encoding="ascii"))
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 2)
            stopped = self.shell(self.lifecycle, "stop", timeout=15)
            self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)
            self.assertFalse(marker.exists())
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)
        finally:
            if started.poll() is None:
                started.kill()
                started.communicate(timeout=5)
            if api_pid is not None:
                try:
                    os.kill(api_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_failed_start_exit_cleanup_ignores_repeated_terminal_signals(self) -> None:
        pre_pid_ready = self.root / "repeat-signal-api.ready"
        pre_pid_release = self.root / "repeat-signal-api.release"
        cleanup_ready = self.root / "repeat-signal-cleanup.ready"
        cleanup_release = self.root / "repeat-signal-cleanup.release"
        self.set_lifecycle_wait_limit(1, 2)
        self.instrument_failed_start_cleanup_pause(cleanup_ready, cleanup_release)
        start = self.shell_process(
            self.lifecycle,
            "start",
            extra_environment={
                "SDSYNC_TEST_API_PRE_PID_READY": str(pre_pid_ready),
                "SDSYNC_TEST_API_PRE_PID_RELEASE": str(pre_pid_release),
            },
        )
        api_pid: int | None = None
        try:
            for _ in range(300):
                if pre_pid_ready.exists():
                    api_pid = int(pre_pid_ready.read_text(encoding="ascii").strip())
                    break
                if start.poll() is not None:
                    stdout, stderr = start.communicate(timeout=5)
                    self.fail(f"start exited before API barrier: {stdout} {stderr}")
                time.sleep(0.02)
            else:
                self.fail("API did not reach repeated-signal barrier")
            for _ in range(300):
                if cleanup_ready.exists():
                    break
                if start.poll() is not None:
                    stdout, stderr = start.communicate(timeout=5)
                    self.fail(f"start exited before cleanup barrier: {stdout} {stderr}")
                time.sleep(0.02)
            else:
                self.fail("failed-start cleanup did not reach its barrier")
            os.kill(start.pid, signal.SIGTERM)
            os.kill(start.pid, signal.SIGHUP)
            time.sleep(0.1)
            self.assertIsNone(start.poll(), "second terminal signal aborted serialized EXIT cleanup")
            pre_pid_release.touch()
            cleanup_release.touch()
            stdout, stderr = start.communicate(timeout=15)
            self.assertEqual(start.returncode, 1, stdout + stderr)
            self.assertFalse((self.real_var / "run/lifecycle.lock").exists())
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)
            if api_pid is not None:
                with self.assertRaises(ProcessLookupError):
                    os.kill(api_pid, 0)
        finally:
            pre_pid_release.touch(exist_ok=True)
            cleanup_release.touch(exist_ok=True)
            if start.poll() is None:
                start.kill()
                start.communicate(timeout=5)
            if api_pid is not None:
                try:
                    os.kill(api_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_interrupted_stop_leaves_closed_ready_topology_inconsistent_until_start_repairs(self) -> None:
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
        closed_ready = self.root / "stop-closed-admission.ready"
        closed_release = self.root / "stop-closed-admission.release"
        self.instrument_shell_pause_before(
            self.lifecycle, "        failed_stop_status=0\n", closed_ready, closed_release
        )
        stopping = self.shell_process(self.lifecycle, "stop")
        try:
            for _ in range(300):
                if closed_ready.exists():
                    break
                if stopping.poll() is not None:
                    stdout, stderr = stopping.communicate(timeout=5)
                    self.fail(f"stop exited before closed-admission barrier: {stdout} {stderr}")
                time.sleep(0.02)
            else:
                self.fail("stop did not close admission before its signal barrier")
            admission = self.executable(
                self.real_target / "bin/sdsync-dsm-api", "--service-admission", "status"
            )
            self.assertEqual(admission.stdout.strip(), "closed")
            os.kill(stopping.pid, signal.SIGTERM)
            stdout, stderr = stopping.communicate(timeout=10)
            self.assertEqual(stopping.returncode, 1, stdout + stderr)
            closed_release.touch()
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 2)
            self.assertEqual(
                self.shell(
                    self.lifecycle,
                    "status",
                    extra_environment={"status_allow_closed_admission": "true"},
                ).returncode,
                2,
            )
            repaired = self.shell(self.lifecycle, "start", timeout=15)
            self.assertEqual(repaired.returncode, 0, repaired.stdout + repaired.stderr)
            self.assertIn("admission restored", repaired.stdout)
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 0)
            stopped = self.shell(self.lifecycle, "stop", timeout=15)
            self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)
        finally:
            closed_release.touch(exist_ok=True)
            if stopping.poll() is None:
                stopping.kill()
                stopping.communicate(timeout=5)

    def test_runner_sigkill_cannot_orphan_core_and_stop_retires_stale_lock(self) -> None:
        configured = self.configure("crash-runner", self.source_one, "/home/Drive/Crash", True)
        self.assertEqual(configured.returncode, 0, configured.stderr)
        password = self.shell(
            self.manager, "set-password", "crash-runner", input_text="crash-password\n"
        )
        self.assertEqual(password.returncode, 0, password.stdout + password.stderr)
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
        core_pid_file = self.root / "orphan-core.pid"
        running = self.shell_process(
            self.manager,
            "plan",
            "crash-runner",
            extra_environment={
                "SDSYNC_TEST_HOLD": "true",
                "SDSYNC_TEST_CORE_PID_FILE": str(core_pid_file),
            },
        )
        runner_pid: int | None = None
        core_pid: int | None = None
        try:
            for _ in range(400):
                run_pid_file = self.real_var / "run/run.lock/pid"
                if run_pid_file.is_file() and core_pid_file.is_file():
                    runner_pid = int(run_pid_file.read_text(encoding="ascii").splitlines()[0])
                    core_pid = int(core_pid_file.read_text(encoding="ascii").strip())
                    break
                if running.poll() is not None:
                    stdout, stderr = running.communicate(timeout=5)
                    self.fail(f"runner exited before crash barrier: {stdout} {stderr}")
                time.sleep(0.02)
            else:
                self.fail("runner/core did not publish their test identities")
            self.assertEqual(runner_pid, running.pid)
            os.kill(runner_pid, signal.SIGKILL)
            running.communicate(timeout=5)
            for _ in range(300):
                try:
                    os.kill(core_pid, 0)  # type: ignore[arg-type]
                except ProcessLookupError:
                    break
                time.sleep(0.02)
            else:
                self.fail(f"core {core_pid} survived its supervised runner")
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 2)
            self.assertNotEqual(self.shell(self.lifecycle_dir / "preupgrade").returncode, 0)
            stopped = self.shell(self.lifecycle, "stop", timeout=15)
            self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)
            self.assertFalse((self.real_var / "run/run.lock").exists())
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)
        finally:
            if running.poll() is None:
                running.kill()
                running.communicate(timeout=5)
            for captured in (runner_pid, core_pid):
                if captured is None:
                    continue
                try:
                    os.kill(captured, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_dsm_core_launches_clear_all_public_sdsync_overrides(self) -> None:
        public_names = (
            "SDSYNC_CONFIG", "SDSYNC_PROFILE", "SDSYNC_MAX_TOTAL_DELETE", "SDSYNC_URL",
            "SDSYNC_USERNAME", "SDSYNC_PASSWORD", "SDSYNC_OTP", "SDSYNC_REMOTE_LOG_TOKEN",
            "SDSYNC_PASSWORD_STDIN", "SDSYNC_PASSWORD_FILE", "SDSYNC_TOTP_SECRET_FILE",
            "SDSYNC_NO_VAULT", "SDSYNC_COMPARE", "SDSYNC_JOBS", "SDSYNC_DELETE",
            "SDSYNC_ALLOW_EMPTY_SOURCE", "SDSYNC_MAX_DELETE", "SDSYNC_RETRIES",
            "SDSYNC_TIMEOUT", "SDSYNC_MAX_RATE", "SDSYNC_CONNECT_TIMEOUT",
            "SDSYNC_CA_CERTIFICATE", "SDSYNC_ALLOW_HTTP",
            "SDSYNC_DANGER_ACCEPT_INVALID_CERTS", "SDSYNC_QUIET", "SDSYNC_LOG_LEVEL",
            "SDSYNC_LOG_FORMAT", "SDSYNC_LOG_FILE", "SDSYNC_REMOTE_LOG_URL",
            "SDSYNC_REMOTE_LOG_TOKEN_FILE", "SDSYNC_REMOTE_LOG_TOKEN_ENV",
            "SDSYNC_REMOTE_LOG_MODE", "SDSYNC_PROGRESS", "SDSYNC_OUTPUT", "SDSYNC_REMOTE",
        )
        core = self.real_target / "bin/synology-drive-sync"
        source = core.read_text(encoding="utf-8")
        needle = ': "${SDSYNC_TEST_CAPTURE:?}"\n'
        self.assertEqual(source.count(needle), 1)
        probes = "".join(
            f"printf '{name}=%s\\n' \"${{{name}-unset}}\" >> \"$SDSYNC_TEST_CAPTURE\"\n"
            for name in public_names
        )
        core.write_text(source.replace(needle, needle + probes), encoding="utf-8")
        core.chmod(0o755)
        if os.getuid() == 0:
            os.chown(core, self.drop_uid, self.drop_gid)

        configured = self.configure("env-safe", self.source_one, "/home/Drive/EnvSafe", True)
        self.assertEqual(configured.returncode, 0, configured.stderr)
        password = self.shell(
            self.manager, "set-password", "env-safe", input_text="safe-password\n"
        )
        self.assertEqual(password.returncode, 0, password.stderr)
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
        self.capture.write_text("", encoding="utf-8")
        hostile = {name: f"hostile-{index}-{name.lower()}" for index, name in enumerate(public_names)}
        hostile["SDSYNC_ALLOW_HTTP"] = "true"
        hostile["SDSYNC_DANGER_ACCEPT_INVALID_CERTS"] = "true"
        hostile["SDSYNC_ALLOW_EMPTY_SOURCE"] = "true"
        for arguments in (("plan", "env-safe"), ("run", "env-safe"), ("doctor", "env-safe")):
            result = self.shell(self.manager, *arguments, extra_environment=hostile)
            self.assertEqual(result.returncode, 0, (arguments, result.stdout + result.stderr))
        captured = self.capture.read_text(encoding="utf-8")
        for name in public_names:
            self.assertIn(f"{name}=unset\n", captured)
            self.assertNotIn(f"hostile-{public_names.index(name)}-", captured)
        self.assertIn("SDSYNC_CONFIG=unset", captured)
        runner_source = (self.real_target / "libexec/sdsync-run").read_text(encoding="utf-8")
        self.assertLess(
            runner_source.index("clear_core_cli_environment"),
            runner_source.index('if [ "$run_output" = scheduled ]'),
            "scheduled and foreground runners must share the same cleared environment",
        )
        stopped = self.shell(self.lifecycle, "stop", timeout=15)
        self.assertEqual(stopped.returncode, 0, stopped.stdout + stopped.stderr)

    def test_dsm_log_file_is_fixed_and_tampered_profiles_cannot_escape(self) -> None:
        outside = self.real_var / "state/protected.conf"
        outside.write_bytes(b"protected\n")
        outside.chmod(0o600)
        if os.getuid() == 0:
            os.chown(outside, self.drop_uid, self.drop_gid)
        before = outside.read_bytes()
        base = [
            "configure-profile", "--name", "escape", "--source", str(self.source_one),
            "--url", "https://files.example.test/", "--username", "escape-bot",
            "--remote", "/home/Drive/Escape",
        ]
        traversal = self.shell(
            self.manager, *base, "--log-file", str(self.real_var / "log/../state/protected.conf")
        )
        self.assertEqual(traversal.returncode, 64, traversal.stdout + traversal.stderr)
        self.assertEqual(outside.read_bytes(), before)
        self.assertFalse((self.real_home / "config/profiles.d/escape.toml").exists())

        sync_log = self.real_var / "log/sync.log"
        sync_log.symlink_to(outside)
        symlinked = self.shell(self.manager, *base, "--log-file", str(sync_log))
        self.assertEqual(symlinked.returncode, 64, symlinked.stdout + symlinked.stderr)
        self.assertTrue(sync_log.is_symlink())
        self.assertEqual(outside.read_bytes(), before)
        sync_log.unlink()

        configured = self.configure("fixed-log", self.source_one, "/home/Drive/Fixed", True)
        self.assertEqual(configured.returncode, 0, configured.stderr)
        fragment = self.real_home / "config/profiles.d/fixed-log.toml"
        fragment.write_text(
            fragment.read_text(encoding="utf-8").replace(
                f'log-file = "{sync_log}"',
                f'log-file = "{self.real_var / "log/../state/schedule.conf"}"',
            ),
            encoding="utf-8",
        )
        fragment.chmod(0o600)
        if os.getuid() == 0:
            os.chown(fragment, self.drop_uid, self.drop_gid)
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
        self.capture.write_text("", encoding="utf-8")
        tampered = self.shell(self.manager, "plan", "fixed-log")
        self.assertEqual(tampered.returncode, 66, tampered.stdout + tampered.stderr)
        self.assertEqual(outside.read_bytes(), before)
        self.assertNotIn(" plan ", f" {self.capture.read_text(encoding='utf-8')} ")
        self.assertEqual(self.shell(self.lifecycle, "stop", timeout=15).returncode, 0)

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
            self.assertIn("API service failed before startup commit", refused.stdout)
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
            self.assertIn("controller failed before startup commit", failed.stdout)
            self.assertFalse((self.real_var / "run/api.pid").exists())
            self.assertFalse((self.real_var / "run/controller.ready").exists())
            self.assertFalse((self.real_target / "ui/api.sock").exists())
            self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)
            service_events = [
                line.split("|", 8)
                for line in (self.real_var / "log/activity.log").read_text(encoding="utf-8").splitlines()
                if "|service.start_failed|" in line
            ]
            self.assertEqual(len(service_events), 1)
            self.assertEqual(
                service_events[0][1:8],
                ["service.start_failed", "none", "failed", "controller", "error", str(self.drop_uid), "package-service"],
            )
        finally:
            controller.chmod(0o755)

    def test_controller_policy_preflight_blocks_readiness_and_rolls_back_api(self) -> None:
        policy = self.real_home / "config/security.conf"
        policy.write_text("broken\n", encoding="utf-8")
        policy.chmod(0o600)
        if os.getuid() == 0:
            os.chown(policy, self.drop_uid, self.drop_gid)
        failed = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(failed.returncode, 1, failed.stdout + failed.stderr)
        self.assertIn("controller failed before startup commit", failed.stdout)
        for path in (
            self.real_var / "run/controller.pid",
            self.real_var / "run/controller.ready",
            self.real_var / "run/controller.lock",
            self.real_var / "run/api.pid",
            self.real_target / "ui/api.sock",
        ):
            self.assertFalse(path.exists(), path)
        self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)

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
        control = self.real_var / "control"
        for relative in (
            "csrf.key",
            "enqueue.lock",
            "requests/0000000000000001.json",
            "requests/0000000000000001.secret",
            "processing/0000000000000002.json",
            "responses/0000000000000003.json",
        ):
            target = control / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text("private bridge state\n", encoding="utf-8")
        outside = self.root / "outside.marker"
        outside.write_text("keep\n", encoding="utf-8")

        upgrade = self.shell(
            self.lifecycle_dir / "postuninst",
            extra_environment={"SYNOPKG_PKG_STATUS": "UPGRADE"},
        )
        self.assertEqual(upgrade.returncode, 0, upgrade.stderr)
        self.assertTrue(control.is_dir(), "upgrade must retain in-flight bridge state")

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
        self.assertFalse(control.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
