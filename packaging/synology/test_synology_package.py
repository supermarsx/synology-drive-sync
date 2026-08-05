#!/usr/bin/env python3
"""Builder, archive, and executable DSM lifecycle regression tests."""

from __future__ import annotations

import copy
import io
import os
import select
import signal
import shutil
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


def fake_elf(machine: int, *, interpreter: bool = False, no_headers: bool = False) -> bytes:
    size = 160
    payload = bytearray(size)
    payload[:16] = b"\x7fELF\x02\x01\x01" + b"\0" * 9
    program_count = 0 if no_headers else 1
    struct.pack_into(
        "<HHIQQQIHHHHHH",
        payload,
        16,
        2,
        machine,
        1,
        0x400000,
        64,
        0,
        0,
        64,
        56,
        program_count,
        0,
        0,
        0,
    )
    if not no_headers:
        kind = 3 if interpreter else 1
        flags = 4 if interpreter else 5
        struct.pack_into("<IIQQQQQQ", payload, 64, kind, flags, 0, 0x400000, 0, size, size, 4096)
    return bytes(payload)


def repack_outer(
    source: Path,
    destination: Path,
    *,
    info_payload: bytes | None = None,
    mode_overrides: dict[str, int] | None = None,
    type_overrides: dict[str, bytes] | None = None,
) -> None:
    mode_overrides = mode_overrides or {}
    type_overrides = type_overrides or {}
    with tarfile.open(source, "r:") as original, tarfile.open(
        destination, "w", format=tarfile.PAX_FORMAT
    ) as rebuilt:
        for original_member in original.getmembers():
            member = copy.copy(original_member)
            payload = (
                original.extractfile(original_member).read()
                if original_member.isfile()
                else None
            )
            if member.name == "INFO" and info_payload is not None:
                payload = info_payload
                member.size = len(payload)
            if member.name in mode_overrides:
                member.mode = mode_overrides[member.name]
            if member.name in type_overrides:
                member.type = type_overrides[member.name]
                member.size = 0
                payload = None
            rebuilt.addfile(member, io.BytesIO(payload) if payload is not None else None)


class BuilderTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="sdsync-spk-test-")
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def build(self, arch: str, machine: int, output: str = "out") -> Path:
        binary = self.root / f"{arch}.elf"
        binary.write_bytes(fake_elf(machine))
        binary.chmod(0o755)
        destination = self.root / output
        environment = os.environ.copy()
        environment["SOURCE_DATE_EPOCH"] = "1700000000"
        result = subprocess.run(
            [
                sys.executable,
                str(HERE / "build_spk.py"),
                "--binary", str(binary),
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

    def test_builds_and_validates_both_supported_architectures(self) -> None:
        for arch, machine in (("x86_64", 62), ("armv8", 183)):
            artifact = self.build(arch, machine, arch)
            result = subprocess.run(
                [sys.executable, str(HERE / "validate_spk.py"), "--arch", arch, str(artifact)],
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(f"({arch})", result.stdout)

    def test_output_is_reproducible_and_extractsize_is_uncompressed_content(self) -> None:
        first = self.build("x86_64", 62, "one")
        second = self.build("x86_64", 62, "two")
        self.assertEqual(first.read_bytes(), second.read_bytes())
        with tarfile.open(first, "r:") as outer:
            info = outer.extractfile("INFO").read().decode("utf-8")  # type: ignore[union-attr]
            compressed_size = outer.getmember("package.tgz").size
        extract_line = next(line for line in info.splitlines() if line.startswith('extractsize="'))
        extract_kib = int(extract_line.split('"')[1])
        self.assertGreater(extract_kib * 1024, compressed_size)
        self.assertIn('version="1.2.3-1"', info)

    def test_rejects_wrong_machine_dynamic_interpreter_and_headerless_elf(self) -> None:
        cases = (
            (fake_elf(183), "machine"),
            (fake_elf(62, interpreter=True), "interpreter"),
            (fake_elf(62, no_headers=True), "program headers"),
        )
        for index, (payload, marker) in enumerate(cases):
            binary = self.root / f"bad-{index}.elf"
            binary.write_bytes(payload)
            result = subprocess.run(
                [
                    sys.executable, str(HERE / "build_spk.py"), "--binary", str(binary),
                    "--arch", "x86_64", "--version", "1.0.0", "--output", str(self.root / "bad"),
                ],
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(marker, result.stderr)

    @unittest.skipUnless(os.name == "posix", "creating a symlink is not portable on Windows")
    def test_builder_rejects_a_symlinked_binary_before_resolving_it(self) -> None:
        binary = self.root / "real.elf"
        binary.write_bytes(fake_elf(62))
        link = self.root / "linked.elf"
        link.symlink_to(binary)
        result = subprocess.run(
            [
                sys.executable,
                str(HERE / "build_spk.py"),
                "--binary",
                str(link),
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
                "SDSYNC_DSM_STOP_TIMEOUT": "10",
            }
        )
        self.manager = self.fhs / "target/bin/sdsync-dsm"
        self.lifecycle = self.lifecycle_dir / "start-stop-status"
        self.drop_uid = os.getuid()
        self.drop_gid = os.getgid()
        if os.getuid() == 0:
            self.drop_uid = 65534
            self.drop_gid = 65534
            for path in [self.root, *self.root.rglob("*")]:
                # Model DSM: FHS links stay root-owned while their @apphome,
                # @appdata, and @appstore targets belong to the package user.
                if not path.is_symlink():
                    os.lchown(path, self.drop_uid, self.drop_gid)
        installed = self.shell(self.lifecycle_dir / "postinst")
        self.assertEqual(installed.returncode, 0, installed.stderr)

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
                (lambda: (os.setgid(self.drop_gid), os.setuid(self.drop_uid)))
                if os.getuid() == 0 and drop_identity
                else None
            ),
        )

    def configure(self, name: str, source: Path, remote: str, default: bool = False) -> subprocess.CompletedProcess[str]:
        arguments = [
            "configure-profile", "--name", name, "--source", str(source),
            "--url", "https://files.example.test/proxy/", "--username", f"{name}-bot",
            "--remote", remote,
        ]
        if default:
            arguments.append("--default")
        return self.shell(self.manager, *arguments)

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
        fake_sleep.write_text("#!/bin/sh\nexec /bin/sleep 0.05\n", encoding="utf-8")
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
        self.assertEqual(started.returncode, 0, started.stderr)
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
        started = self.shell(self.lifecycle, "start", timeout=15)
        self.assertEqual(started.returncode, 0, started.stderr)
        self.assertEqual(self.shell(self.lifecycle, "status").returncode, 0)
        stopped = self.shell(self.lifecycle, "stop", timeout=15)
        self.assertEqual(stopped.returncode, 0, stopped.stderr)
        self.assertEqual(self.shell(self.lifecycle, "status").returncode, 3)

        run_lock = self.real_var / "run/run.lock"
        run_lock.mkdir(mode=0o700)
        (run_lock / "pid").write_text(f"{os.getpid()}\n", encoding="utf-8")
        self.assertEqual(self.shell(self.lifecycle_dir / "preupgrade").returncode, 1)
        self.assertEqual(self.shell(self.lifecycle_dir / "preuninst").returncode, 1)
        (run_lock / "pid").unlink()
        run_lock.rmdir()

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
