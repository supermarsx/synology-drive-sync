#!/usr/bin/env python3
"""Deterministic static contract checks for shipped service-management assets."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def read(relative: str) -> str:
    return (ROOT / relative).read_text(encoding="utf-8")


def require(text: str, needles: list[str], label: str) -> None:
    missing = [needle for needle in needles if needle not in text]
    if missing:
        raise AssertionError(f"{label} is missing required contracts: {missing}")


def forbid(text: str, needles: list[str], label: str) -> None:
    present = [needle for needle in needles if needle in text]
    if present:
        raise AssertionError(f"{label} contains forbidden contracts: {present}")


def _parse_simple_env_file(text: str) -> dict[str, str]:
    """Parse KEY=value lines the way the shipped systemd/cron example env files use."""
    values: dict[str, str] = {}
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        name, _, value = line.partition("=")
        values[name.strip()] = value.strip()
    return values


def _validator_binary() -> Path | None:
    """Locate the compiled binary the argument-parsing checks execute against.

    CI builds it once (see .github/workflows/ci.yml, `packaging` job) and this finds it
    at the conventional cargo output path; SDSYNC_VALIDATOR_BINARY overrides that for a
    developer pointing at a different build. When both a debug and a release build are
    present -- common on a developer machine that has run plain `cargo build` at some
    point and `cargo build --release` at another -- the newer one wins so a stale build
    left over from before a fix landed can never shadow a fresh one.
    """
    override = os.environ.get("SDSYNC_VALIDATOR_BINARY")
    if override:
        path = Path(override)
        return path if path.is_file() else None
    suffix = ".exe" if os.name == "nt" else ""
    candidates = [
        ROOT / "target" / profile / f"synology-drive-sync{suffix}"
        for profile in ("debug", "release")
    ]
    existing = [candidate for candidate in candidates if candidate.is_file()]
    if not existing:
        return None
    return max(existing, key=lambda candidate: candidate.stat().st_mtime)


def _clap_conflict(result: subprocess.CompletedProcess[str]) -> bool:
    return result.returncode == 2 and "cannot be used with" in result.stderr


def _assert_no_clap_conflict(result: subprocess.CompletedProcess[str], label: str) -> None:
    """Fail loudly if the real binary rejected its argv as a clap argument conflict.

    Argument-parsing failures are always exit code 2 with a clap-authored message on
    stderr; every invocation this validator makes is otherwise complete and valid, so
    exit 2 here can only mean a `conflicts_with` is firing against an env-sourced flag
    and its documented override (see C1, .orchestration/logs/t2-a4.md).
    """
    if _clap_conflict(result):
        raise AssertionError(
            f"{label}: the real binary aborted with a clap argument-parsing conflict "
            f"before doing any work -- stderr={result.stderr!r}"
        )


def _assert_clap_conflict(result: subprocess.CompletedProcess[str], label: str) -> None:
    """Fail loudly if a conflict that must still exist stopped firing.

    The inverse of `_assert_no_clap_conflict`: `--profiles`/`--all-profiles` is not
    env-sourced on either side, so it was deliberately left as a real `conflicts_with`
    (src/cli.rs:269/275) while the env-sourced pairs lost theirs. Losing this guard
    silently (a future refactor dropping it by accident) would let a batch request
    apply one selection's credentials to the other's profiles.
    """
    if not _clap_conflict(result):
        raise AssertionError(
            f"{label}: expected the real binary to reject this argv with a clap "
            f"conflict, but it did not -- exit={result.returncode} stdout={result.stdout!r} "
            f"stderr={result.stderr!r}"
        )


def plist_values(path: Path) -> dict[str, ET.Element]:
    root = ET.parse(path).getroot()
    dictionary = root.find("dict")
    if dictionary is None:
        raise AssertionError(f"{path} has no root dict")
    children = list(dictionary)
    values: dict[str, ET.Element] = {}
    index = 0
    while index < len(children):
        key = children[index]
        if key.tag != "key" or index + 1 >= len(children):
            raise AssertionError(f"{path} has malformed key/value ordering")
        values[key.text or ""] = children[index + 1]
        index += 2
    return values


def validate_systemd() -> None:
    unit = read("packaging/systemd/synology-drive-sync.service")
    require(
        unit,
        [
            "Type=oneshot",
            "StateDirectory=synology-drive-sync",
            "Restart=no",
            "TimeoutStartSec=24h",
            "TimeoutStopSec=2m",
            "LoadCredential=dsm-password:",
            "NoNewPrivileges=yes",
            "ProtectSystem=strict",
        ],
        "systemd service",
    )
    forbid(unit, ["ConditionFileIsExecutable=", "ConditionPathExists="], "systemd service")

    wrapper = read("packaging/systemd/systemd-run.sh")
    require(
        wrapper,
        [
            "flock -n 9",
            "SDSYNC_LOCK_FILE",
            "config validate",
            "--profiles",
            "--all-profiles",
            "--max-total-delete",
            "--password-file",
            "--no-delete",
            "SDSYNC_USE_REMOTE_LOG_CREDENTIAL",
            "SDSYNC_BATCH_SHARED_CREDENTIALS",
            "batch mode is blocked by default",
        ],
        "systemd wrapper",
    )
    forbid(wrapper, ["eval ", ". \"$config\"", "--password-stdin"], "systemd wrapper")
    forbid(wrapper, ['set -- sync --config'], "systemd wrapper")


def validate_cron() -> None:
    wrapper = read("packaging/cron/run-sync.sh")
    require(
        wrapper,
        [
            'export "$line"',
            "flock -n 9",
            "SDSYNC_LOCK_FILE",
            'printf \'%s\\n\' "$$" > "$lock_file"',
            "config validate",
            "--profiles",
            "--all-profiles",
            "--max-total-delete",
            "--no-delete",
            "SDSYNC_BATCH_SHARED_CREDENTIALS",
            "batch mode is blocked by default",
        ],
        "cron wrapper",
    )
    forbid(wrapper, ['. "$config"', "eval ", "--password-stdin"], "cron wrapper")
    forbid(wrapper, ['set -- sync --config'], "cron wrapper")
    crontab = read("packaging/cron/synology-drive-sync.crontab.example")
    forbid(crontab, ["/usr/bin/flock"], "cron example")
    require(crontab, ["status=$?", '"$status" -eq 75'], "cron example")


def validate_launchd() -> None:
    path = ROOT / "packaging/launchd/io.github.supermarsx.synology-drive-sync.plist"
    values = plist_values(path)
    required_keys = {
        "Label",
        "ProgramArguments",
        "StartCalendarInterval",
        "ExitTimeOut",
        "StandardOutPath",
        "StandardErrorPath",
    }
    missing = sorted(required_keys - values.keys())
    if missing:
        raise AssertionError(f"launchd plist is missing keys: {missing}")
    arguments = [node.text or "" for node in values["ProgramArguments"] if node.tag == "string"]
    if len(arguments) < 2 or not arguments[0].endswith("/launchd-run"):
        raise AssertionError("launchd ProgramArguments must start with the packaged wrapper")
    require("\n".join(arguments), ["synology-drive-sync", "--quiet", "--log-file", "--progress"], "launchd arguments")
    forbid("\n".join(arguments), ["--password-file", "--totp-secret-file", "--password-stdin"], "launchd arguments")

    wrapper = read("packaging/launchd/launchd-run.sh")
    require(
        wrapper,
        [
            "kill -TERM",
            "/usr/bin/logger",
            "mktemp -d",
            "mkfifo -m 0600",
            'wait "$child"',
            'wait "$logger_pid"',
            "kill -0",
        ],
        "launchd wrapper",
    )


def validate_windows() -> None:
    installer = read("packaging/windows/Install-SynologyDriveSyncTask.ps1")
    require(
        installer,
        [
            "MultipleInstances IgnoreNew",
            "ConvertTo-NativeArgument",
            "config validate",
            "--profiles",
            "--all-profiles",
            "--max-total-delete",
            "--no-delete",
            "SupportsShouldProcess",
        ],
        "Task Scheduler installer",
    )
    forbid(installer, ["--password-stdin", "--password-file", "--totp-secret-file"], "Task Scheduler installer")
    manager = read("packaging/windows/Manage-SynologyDriveSyncTask.ps1")
    require(
        manager,
        [
            "Get-ScheduledTaskInfo",
            "Start-ScheduledTask",
            "Stop-ScheduledTask",
            "Enable-ScheduledTask",
            "Disable-ScheduledTask",
            "Unregister-ScheduledTask",
            "LastTaskResult",
            "SupportsShouldProcess",
        ],
        "Task Scheduler manager",
    )


def validate_docker() -> None:
    runner = read("packaging/docker/run-compose.sh")
    require(
        runner,
        [
            "lock_arguments=(-n 9)",
            "config --quiet",
            "build --pull",
            "write-test",
            "--write-test",
            "docker stop --time 120",
            "docker container inspect",
            "restart_container_id",
            "lock_arguments=(-w 10 9)",
            "identity changed during restart",
            "PIPESTATUS[0]",
            "10485760",
            "SDSYNC_RUNTIME_UID",
            "SDSYNC_RUNTIME_GID",
            "export SDSYNC_RUNTIME_UID=$runtime_uid SDSYNC_RUNTIME_GID=$runtime_gid",
        ],
        "Compose runner",
    )
    forbid(runner, ["docker rm -f", "restart: always"], "Compose runner")
    compose_model = read("compose.yaml")
    require(
        compose_model,
        ['user: "${SDSYNC_RUNTIME_UID:-10001}:${SDSYNC_RUNTIME_GID:-10001}"'],
        "Compose runtime identity",
    )
    entrypoint = read("packaging/docker/entrypoint.sh")
    require(entrypoint, ['exec "$binary" "$@"'], "container entrypoint")


def validate_docker_runner_behavior() -> None:
    """Exercise identity rejection/export without requiring a Docker daemon."""
    if os.name != "posix":
        print("skipped POSIX Docker-runner behavior checks on this host")
        return
    bash = shutil.which("bash")
    if bash is None:
        raise AssertionError("bash is required for Docker-runner behavior validation")

    runner = ROOT / "packaging/docker/run-compose.sh"
    with tempfile.TemporaryDirectory(prefix="sdsync-service-validator-") as raw_temp:
        temporary = Path(raw_temp)
        fake_bin = temporary / "bin"
        fake_bin.mkdir(mode=0o700)
        capture = temporary / "docker-environment"
        fake_docker = fake_bin / "docker"
        fake_docker.write_text(
            "#!/bin/sh\n"
            'printf \'%s:%s\\n\' "$SDSYNC_RUNTIME_UID" "$SDSYNC_RUNTIME_GID" '
            '> "$SDSYNC_TEST_CAPTURE"\n',
            encoding="utf-8",
        )
        fake_docker.chmod(0o700)
        source = temporary / "source"
        source.mkdir(mode=0o700)
        secret = temporary / "password"
        secret.write_text("test-only-secret\n", encoding="utf-8")
        secret.chmod(0o600)

        base_environment = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("SDSYNC_")
        }
        base_environment.update(
            {
                "PATH": f"{fake_bin}{os.pathsep}{base_environment.get('PATH', '')}",
                "SDSYNC_COMPOSE_DIR": str(ROOT),
                "SDSYNC_TEST_CAPTURE": str(capture),
                "SDSYNC_URL": "https://files.example.invalid",
                "SDSYNC_USERNAME": "service-validator",
                "SDSYNC_SOURCE": str(source),
                "SDSYNC_REMOTE": "/service-validator",
                "SDSYNC_PASSWORD_FILE": str(secret),
                "XDG_STATE_HOME": str(temporary / "state"),
            }
        )

        invalid_identities = [
            ("0", "1", "SDSYNC_RUNTIME_UID"),
            ("not-a-uid", "1", "SDSYNC_RUNTIME_UID"),
            ("1", "0", "SDSYNC_RUNTIME_GID"),
            ("1", "not-a-gid", "SDSYNC_RUNTIME_GID"),
        ]
        for uid, gid, expected_error in invalid_identities:
            environment = base_environment | {
                "SDSYNC_RUNTIME_UID": uid,
                "SDSYNC_RUNTIME_GID": gid,
            }
            result = subprocess.run(
                [bash, str(runner), "validate"],
                cwd=ROOT,
                env=environment,
                capture_output=True,
                text=True,
                timeout=15,
                check=False,
            )
            if result.returncode != 64 or expected_error not in result.stderr:
                raise AssertionError(
                    "Compose runner did not reject invalid runtime identity "
                    f"{uid}:{gid} with exit 64: stdout={result.stdout!r}, "
                    f"stderr={result.stderr!r}, exit={result.returncode}"
                )

        environment = base_environment | {
            "SDSYNC_RUNTIME_UID": "12345",
            "SDSYNC_RUNTIME_GID": "23456",
        }
        result = subprocess.run(
            [bash, str(runner), "validate"],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
        if result.returncode != 0:
            raise AssertionError(
                "Compose runner rejected a valid runtime identity: "
                f"stdout={result.stdout!r}, stderr={result.stderr!r}, "
                f"exit={result.returncode}"
            )
        if capture.read_text(encoding="utf-8").strip() != "12345:23456":
            raise AssertionError("Compose runner did not export its validated UID/GID to Compose")


def validate_rendered_compose_identity() -> None:
    """Ask Compose itself to prove the managed UID/GID interpolation contract."""
    docker = shutil.which("docker")
    if docker is None:
        print("skipped rendered Compose identity check because Docker CLI is unavailable")
        return
    probe = subprocess.run(
        [docker, "compose", "version"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        timeout=15,
        check=False,
    )
    if probe.returncode != 0:
        print("skipped rendered Compose identity check because Compose is unavailable")
        return

    environment = {
        key: value for key, value in os.environ.items() if not key.startswith("SDSYNC_")
    }
    environment.update(
        {
            "SDSYNC_URL": "https://files.example.invalid",
            "SDSYNC_USERNAME": "service-validator",
            "SDSYNC_SOURCE": str(ROOT),
            "SDSYNC_REMOTE": "/service-validator",
            "SDSYNC_PASSWORD_FILE": str(ROOT / "LICENSE"),
            "SDSYNC_RUNTIME_UID": "12345",
            "SDSYNC_RUNTIME_GID": "23456",
        }
    )
    rendered = subprocess.run(
        [docker, "compose", "-f", "compose.yaml", "config", "--format", "json"],
        cwd=ROOT,
        env=environment,
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
    if rendered.returncode != 0:
        raise AssertionError(
            "Compose could not render the runtime identity test model: "
            f"stdout={rendered.stdout!r}, stderr={rendered.stderr!r}"
        )
    model = json.loads(rendered.stdout)
    actual = model.get("services", {}).get("sync", {}).get("user")
    if actual != "12345:23456":
        raise AssertionError(f"rendered Compose user is {actual!r}, expected '12345:23456'")


def validate_installers_and_docs() -> None:
    shell_installer = read("packaging/install.sh")
    powershell_installer = read("packaging/install.ps1")
    for installer, label in [
        (shell_installer, "POSIX installer"),
        (powershell_installer, "PowerShell installer"),
    ]:
        require(
            installer,
            [
                "synology-drive-sync-doctor-source.1",
                "synology-drive-sync-doctor-target.1",
                "uninstall" if label == "POSIX installer" else "Uninstall",
            ],
            label,
        )

    for directory in ["systemd", "launchd", "windows", "cron", "docker"]:
        documentation = read(f"packaging/{directory}/README.md").lower()
        require(
            documentation,
            ["install", "upgrade", "uninstall", "status", "log", "batch", "lock"],
            f"{directory} documentation",
        )


def validate_negation_flag_regressions() -> None:
    """Run the real binary through the confirmed env+flag clap conflicts (C1).

    `--no-delete`, `--no-quiet`, `-v`, and `--vault` exist specifically to override an
    env-sourced or profile-sourced setting. clap's `conflicts_with` does not know that: it
    rejects the pair outright and, critically, treats SDSYNC_DELETE=false as "present" the
    same as SDSYNC_DELETE=true. A textual check on wrapper source can never see this -- it
    is a clap-level argument-parsing failure that only shows up when the real binary
    parses the real argv. This loads the actual SDSYNC_DELETE and SDSYNC_QUIET values
    shipped in packaging/cron/synology-drive-sync.env.example so the check reflects what a
    real deployment's environment looks like, not a synthetic shortcut.
    """
    binary = _validator_binary()
    if binary is None:
        print(
            "skipped negation-flag regression checks: no built binary found "
            "(set SDSYNC_VALIDATOR_BINARY or run `cargo build`)"
        )
        return

    shipped = _parse_simple_env_file(read("packaging/cron/synology-drive-sync.env.example"))
    pairs: list[tuple[str, str, list[str]]] = [
        ("SDSYNC_DELETE", shipped["SDSYNC_DELETE"], ["--no-delete"]),
        ("SDSYNC_DELETE", "true", ["--no-delete"]),
        ("SDSYNC_QUIET", shipped["SDSYNC_QUIET"], ["--no-quiet"]),
        ("SDSYNC_QUIET", shipped["SDSYNC_QUIET"], ["-v"]),
        ("SDSYNC_NO_VAULT", "true", ["--vault"]),
    ]

    with tempfile.TemporaryDirectory(prefix="sdsync-negation-check-") as raw_temp:
        temporary = Path(raw_temp)
        source = temporary / "source"
        source.mkdir()
        (source / "payload.txt").write_text("payload", encoding="utf-8")
        secret = temporary / "password"
        secret.write_text("test-only-secret\n", encoding="utf-8")

        base_environment = {
            key: value for key, value in os.environ.items() if not key.startswith("SDSYNC_")
        }

        for env_name, env_value, extra_args in pairs:
            environment = base_environment | {env_name: env_value}
            arguments = [
                "sync",
                str(source),
                "/negation-check",
                "--url",
                "https://files.example.invalid",
                "--username",
                "negation-check",
                "--password-file",
                str(secret),
                "--jobs",
                "1",
                "--connect-timeout",
                "1",
                "--timeout",
                "1",
                *extra_args,
            ]
            result = subprocess.run(
                [str(binary), *arguments],
                env=environment,
                capture_output=True,
                text=True,
                timeout=15,
                check=False,
            )
            _assert_no_clap_conflict(result, f"{env_name}={env_value} plus {' '.join(extra_args)}")


def validate_authentication_conflict_regressions() -> None:
    """Run the real binary through the four env-sourced-on-both-sides pairs found after
    C1 (t2-f1's second pass over src/cli.rs): `--password-stdin`/`--password-file` and
    `--remote-log-token-file`/`--remote-log-token-env` each carry env on BOTH sides
    (SDSYNC_PASSWORD_STDIN and SDSYNC_PASSWORD_FILE; SDSYNC_REMOTE_LOG_TOKEN_FILE and
    SDSYNC_REMOTE_LOG_TOKEN_ENV), so unlike the negation pairs above, each one broke in
    both directions. `--password-file`'s env value is the literal path shipped in
    packaging/cron/synology-drive-sync.env.example -- it does not need to exist for a
    parse-conflict check, and using it anyway keeps this anchored to what a real
    deployment's environment actually contains. The remote-log-token pair is not shipped
    uncommented anywhere, so those two use representative values instead.

    Unlike the other three pairs, both remote-log-token sides being set is NOT rejected
    post-fix: config.rs's resolver forces the env-name source to None before its
    both-set check ever runs, so the file source silently wins (only a profile setting
    both keys is rejected). This still asserts no *clap* conflict either way -- that is
    this check's whole scope -- and does not assert anything about which value wins.
    `--remote-log-url` is included so the invocation reaches that resolver instead of
    failing earlier on an unrelated "token source without a URL" configuration error.
    """
    binary = _validator_binary()
    if binary is None:
        print(
            "skipped authentication/remote-log conflict regression checks: no built "
            "binary found (set SDSYNC_VALIDATOR_BINARY or run `cargo build`)"
        )
        return

    shipped_password_file = _parse_simple_env_file(
        read("packaging/cron/synology-drive-sync.env.example")
    )["SDSYNC_PASSWORD_FILE"]

    with tempfile.TemporaryDirectory(prefix="sdsync-auth-conflict-check-") as raw_temp:
        temporary = Path(raw_temp)
        source = temporary / "source"
        source.mkdir()
        (source / "payload.txt").write_text("payload", encoding="utf-8")
        secret = temporary / "password"
        secret.write_text("test-only-secret\n", encoding="utf-8")
        token_file = temporary / "remote-log-token"
        token_file.write_text("test-only-token\n", encoding="utf-8")

        base_environment = {
            key: value for key, value in os.environ.items() if not key.startswith("SDSYNC_")
        }
        base_arguments = [
            "sync",
            str(source),
            "/auth-conflict-check",
            "--url",
            "https://files.example.invalid",
            "--username",
            "auth-conflict-check",
            "--jobs",
            "1",
            "--connect-timeout",
            "1",
            "--timeout",
            "1",
        ]

        cases: list[tuple[str, str, list[str]]] = [
            (
                "SDSYNC_PASSWORD_STDIN",
                "true",
                ["--password-file", str(secret)],
            ),
            (
                "SDSYNC_PASSWORD_FILE",
                shipped_password_file,
                ["--password-stdin"],
            ),
            (
                "SDSYNC_REMOTE_LOG_TOKEN_FILE",
                str(token_file),
                [
                    "--remote-log-token-env",
                    "SDSYNC_TEST_UNUSED_TOKEN_VAR",
                    "--remote-log-url",
                    "https://files.example.invalid/logs",
                    "--password-file",
                    str(secret),
                ],
            ),
            (
                "SDSYNC_REMOTE_LOG_TOKEN_ENV",
                "SDSYNC_TEST_UNUSED_TOKEN_VAR",
                [
                    "--remote-log-token-file",
                    str(token_file),
                    "--remote-log-url",
                    "https://files.example.invalid/logs",
                    "--password-file",
                    str(secret),
                ],
            ),
        ]

        for env_name, env_value, extra_args in cases:
            environment = base_environment | {env_name: env_value}
            result = subprocess.run(
                [str(binary), *base_arguments, *extra_args],
                env=environment,
                # A conflict-free --password-stdin would otherwise block reading a real
                # password from this process's own stdin.
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                timeout=15,
                check=False,
            )
            _assert_no_clap_conflict(result, f"{env_name}={env_value} plus {' '.join(extra_args)}")


def validate_credentials_conflict_regressions() -> None:
    """Run `credentials set-password`/`set-totp` through their own stdin/file conflicts.

    SetPasswordArgs.password_file and SetTotpArgs.totp_secret_file carry
    SDSYNC_PASSWORD_FILE/SDSYNC_TOTP_SECRET_FILE; their --password-stdin/--secret-stdin
    siblings do not carry env, so (unlike validate_authentication_conflict_regressions)
    only one direction exists per pair. SDSYNC_TOTP_SECRET_FILE is the pair that matters
    operationally: all four wrappers set it up, so a broken conflict here would make
    `credentials set-totp --secret-stdin` unusable for any operator with a wrapper's
    example env file sourced. Both cases point the env var at a file that is never
    created and read stdin from /dev/null (verified: resolution picks the CLI-explicit
    stdin side here and fails on "password/TOTP seed was empty" without ever touching
    the file), so a fixed binary proceeds past parsing straight to an empty-input
    failure instead of ever reaching the OS credential vault -- this must never create
    a real vault entry, checked explicitly below.
    """
    binary = _validator_binary()
    if binary is None:
        print(
            "skipped credentials conflict regression checks: no built binary found "
            "(set SDSYNC_VALIDATOR_BINARY or run `cargo build`)"
        )
        return

    with tempfile.TemporaryDirectory(prefix="sdsync-credentials-conflict-check-") as raw_temp:
        temporary = Path(raw_temp)
        missing_password_file = temporary / "never-created-password"
        missing_totp_file = temporary / "never-created-totp"

        base_environment = {
            key: value for key, value in os.environ.items() if not key.startswith("SDSYNC_")
        }
        # --url/--username (CredentialProfileArgs) are flattened into SetPasswordArgs and
        # SetTotpArgs directly, not into a parent shared by every `credentials` action, so
        # they must follow the action name, not precede it -- `credentials --url ...
        # set-password` is itself a clap usage error ("unexpected argument '--url'"),
        # which very nearly produced a false pass here: it also aborts before doing any
        # work, but for a completely unrelated reason.
        profile_arguments = ["--url", "https://files.example.invalid", "--username", "credentials-conflict-check"]

        cases: list[tuple[str, str, list[str], list[str]]] = [
            (
                "SDSYNC_PASSWORD_FILE",
                str(missing_password_file),
                ["set-password"],
                ["--password-stdin"],
            ),
            (
                "SDSYNC_TOTP_SECRET_FILE",
                str(missing_totp_file),
                ["set-totp"],
                ["--secret-stdin"],
            ),
        ]

        for env_name, env_value, action, extra_args in cases:
            environment = base_environment | {env_name: env_value}
            result = subprocess.run(
                [str(binary), "credentials", *action, *profile_arguments, *extra_args],
                env=environment,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                timeout=15,
                check=False,
            )
            _assert_no_clap_conflict(
                result, f"credentials {action[0]}: {env_name}={env_value} plus {' '.join(extra_args)}"
            )
            if "vault" in result.stdout.lower() or "stored" in result.stdout.lower():
                raise AssertionError(
                    "credentials conflict regression check may have written a real "
                    f"OS-vault entry: stdout={result.stdout!r}"
                )


def validate_batch_profile_conflict_still_enforced() -> None:
    """Confirm `--profiles`/`--all-profiles` still conflicts and still exits 2.

    Neither flag is env-sourced (src/cli.rs:269/275), so this `conflicts_with` was
    deliberately kept when the env-sourced ones were dropped. This is the control that
    proves the checks above distinguish "a conflict that should exist" from "no
    conflicts anywhere" -- without it, a validator that simply never flagged any exit 2
    would trivially pass even if it stopped checking anything.
    """
    binary = _validator_binary()
    if binary is None:
        print(
            "skipped --profiles/--all-profiles conflict regression check: no built "
            "binary found (set SDSYNC_VALIDATOR_BINARY or run `cargo build`)"
        )
        return

    with tempfile.TemporaryDirectory(prefix="sdsync-batch-conflict-check-") as raw_temp:
        temporary = Path(raw_temp)
        config = temporary / "config.toml"
        config.write_text(
            "\n".join(
                [
                    '[profiles.alpha]',
                    f'source = "{(temporary / "alpha").as_posix()}"',
                    'remote = "/alpha"',
                    'url = "https://files.example.invalid"',
                    'username = "alpha"',
                    'no-vault = true',
                    "",
                    '[profiles.beta]',
                    f'source = "{(temporary / "beta").as_posix()}"',
                    'remote = "/beta"',
                    'url = "https://files.example.invalid"',
                    'username = "beta"',
                    'no-vault = true',
                    "",
                ]
            ),
            encoding="utf-8",
        )
        (temporary / "alpha").mkdir()
        (temporary / "beta").mkdir()
        secret = temporary / "password"
        secret.write_text("test-only-secret\n", encoding="utf-8")

        environment = {
            key: value for key, value in os.environ.items() if not key.startswith("SDSYNC_")
        }
        result = subprocess.run(
            [
                str(binary),
                "--config",
                str(config),
                "sync",
                "--password-file",
                str(secret),
                "--profiles",
                "alpha,beta",
                "--all-profiles",
            ],
            env=environment,
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
        _assert_clap_conflict(result, "--profiles plus --all-profiles")


def validate_cron_wrapper_argument_parsing() -> None:
    """Execute the real cron wrapper's env-to-argv construction against the real binary.

    packaging/cron/synology-drive-sync.env.example ships SDSYNC_DELETE=false and
    SDSYNC_QUIET=true; packaging/cron/run-sync.sh always adds --no-delete when
    SDSYNC_DELETE is not true. Only the paths that must exist in a sandbox are overridden
    (executable, source, password file, state root, URL); every other shipped line is fed
    to the wrapper unmodified so this reflects what a real deployment actually ships.
    """
    if os.name != "posix":
        print("skipped cron wrapper argument-parsing check on this host (requires POSIX sh)")
        return
    binary = _validator_binary()
    if binary is None:
        print("skipped cron wrapper argument-parsing check: no built binary found")
        return

    wrapper = ROOT / "packaging/cron/run-sync.sh"
    shipped = _parse_simple_env_file(read("packaging/cron/synology-drive-sync.env.example"))

    with tempfile.TemporaryDirectory(prefix="sdsync-cron-argparse-") as raw_temp:
        temporary = Path(raw_temp)
        fixture_binary = temporary / "synology-drive-sync"
        shutil.copy2(binary, fixture_binary)
        fixture_binary.chmod(0o755)

        source = temporary / "source"
        source.mkdir(mode=0o700)
        (source / "payload.txt").write_text("payload", encoding="utf-8")

        password = temporary / "password"
        password.write_text("test-only-secret\n", encoding="utf-8")
        password.chmod(0o600)

        state = temporary / "state"
        config_dir = temporary / "config"
        config_dir.mkdir(mode=0o700)

        merged = dict(shipped)
        merged.update(
            {
                "SDSYNC_EXECUTABLE": str(fixture_binary),
                "SDSYNC_SOURCE": str(source),
                "SDSYNC_PASSWORD_FILE": str(password),
                "SDSYNC_URL": "https://files.example.invalid",
                "SDSYNC_LOCK_FILE": str(state / "synology-drive-sync" / "service.lock"),
            }
        )
        env_file = config_dir / "cron.env"
        env_file.write_text(
            "".join(f"{name}={value}\n" for name, value in merged.items()), encoding="utf-8"
        )
        env_file.chmod(0o600)

        environment = {
            key: value for key, value in os.environ.items() if not key.startswith("SDSYNC_")
        }
        environment["XDG_STATE_HOME"] = str(state)
        environment["HOME"] = str(temporary)

        result = subprocess.run(
            ["/bin/sh", str(wrapper), str(env_file)],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
            timeout=20,
            check=False,
        )
        _assert_no_clap_conflict(result, "cron wrapper's shipped example env file")


def validate_synology_wrapper_argument_parsing() -> None:
    """Execute the real sdsync-run wrapper's --no-delete construction against the real
    binary, with an ambient SDSYNC_DELETE in the environment.

    packaging/synology/package/libexec/sdsync-run always adds --no-delete for a
    non-delete run; an ambient SDSYNC_DELETE in the package's environment would hit the
    same clap conflict as the systemd/cron wrappers. sdsync-run and sdsync-dsm resolve
    their package paths from SYNOPKG_PKGDEST/SYNOPKG_PKGHOME/SYNOPKG_PKGVAR, so this points
    those at sandbox directories rather than a real DSM installation and uses the real
    sdsync-dsm manager -- the same tool a DSM user runs -- to build a real profile and
    stored password.
    """
    if os.name != "posix":
        print("skipped Synology wrapper argument-parsing check on this host (requires POSIX sh)")
        return
    binary = _validator_binary()
    if binary is None:
        print("skipped Synology wrapper argument-parsing check: no built binary found")
        return

    manager = ROOT / "packaging/synology/package/bin/sdsync-dsm"
    runner = ROOT / "packaging/synology/package/libexec/sdsync-run"

    with tempfile.TemporaryDirectory(prefix="sdsync-dsm-argparse-") as raw_temp:
        temporary = Path(raw_temp)
        package_home = temporary / "home"
        package_target = temporary / "target"
        package_var = temporary / "var"
        for path in (package_home, package_var):
            path.mkdir(mode=0o700)
        (package_target / "bin").mkdir(parents=True, mode=0o700)
        fixture_binary = package_target / "bin/synology-drive-sync"
        shutil.copy2(binary, fixture_binary)
        fixture_binary.chmod(0o755)

        source = temporary / "source"
        source.mkdir(mode=0o700)
        (source / "payload.txt").write_text("payload", encoding="utf-8")

        environment = {
            key: value for key, value in os.environ.items() if not key.startswith("SDSYNC_")
        }
        environment.update(
            {
                "SYNOPKG_PKGDEST": str(package_target),
                "SYNOPKG_PKGHOME": str(package_home),
                "SYNOPKG_PKGVAR": str(package_var),
                "HOME": str(temporary),
            }
        )

        configured = subprocess.run(
            [
                "/bin/sh",
                str(manager),
                "configure-profile",
                "--name",
                "argparse-check",
                "--source",
                str(source),
                "--url",
                "https://files.example.invalid",
                "--username",
                "argparse-check",
                "--remote",
                "/argparse-check",
                "--remote-log-url",
                "https://logs.example.invalid/ingest",
                "--default",
            ],
            env=environment,
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
        if configured.returncode != 0:
            raise AssertionError(
                "could not build a Synology package fixture profile for argument-parsing "
                f"validation: stdout={configured.stdout!r} stderr={configured.stderr!r}"
            )

        password_input = temporary / "password.input"
        password_input.write_text("test-only-secret\n", encoding="utf-8")
        stored = subprocess.run(
            [
                "/bin/sh",
                str(manager),
                "set-password",
                "argparse-check",
                "--from-file",
                str(password_input),
            ],
            env=environment,
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
        if stored.returncode != 0:
            raise AssertionError(
                "could not store a Synology package fixture password for argument-parsing "
                f"validation: stdout={stored.stdout!r} stderr={stored.stderr!r}"
            )

        for delete_value in ("false", "true"):
            run_environment = dict(environment)
            run_environment["SDSYNC_DELETE"] = delete_value
            result = subprocess.run(
                ["/bin/sh", str(runner), "sync", "argparse-check", "false", "foreground", "-"],
                env=run_environment,
                capture_output=True,
                text=True,
                timeout=20,
                check=False,
            )
            _assert_no_clap_conflict(
                result, f"sdsync-run with an ambient SDSYNC_DELETE={delete_value}"
            )


SYSTEMD_BINARY_PATH = Path("/usr/local/bin/synology-drive-sync")


def validate_systemd_wrapper_argument_parsing() -> None:
    """Execute the real systemd wrapper's --no-delete construction against the real
    binary, with the shipped example env file loaded.

    packaging/systemd/systemd-run.sh hardcodes `binary=/usr/local/bin/synology-drive-sync`
    with no override, so there is no way to point it at a sandboxed copy of the binary
    without writing to that exact path. That is safe on an ephemeral GitHub Actions
    runner -- the whole VM is discarded after the job -- but must never run against a
    developer's real machine, where that path could be a real production install. This
    only runs when GITHUB_ACTIONS=true, and only if nothing is already installed there.
    """
    if os.name != "posix":
        print("skipped systemd wrapper argument-parsing check on this host (requires POSIX sh)")
        return
    if os.environ.get("GITHUB_ACTIONS") != "true":
        print(
            "skipped systemd wrapper argument-parsing check: it must install the real "
            f"binary at the wrapper's hardcoded {SYSTEMD_BINARY_PATH}, which this validator "
            "only does on an ephemeral GitHub Actions runner (GITHUB_ACTIONS=true)"
        )
        return
    if SYSTEMD_BINARY_PATH.exists():
        print(
            "skipped systemd wrapper argument-parsing check: refusing to overwrite an "
            f"existing {SYSTEMD_BINARY_PATH}"
        )
        return
    binary = _validator_binary()
    if binary is None:
        print("skipped systemd wrapper argument-parsing check: no built binary found")
        return

    wrapper = ROOT / "packaging/systemd/systemd-run.sh"
    shipped = _parse_simple_env_file(read("packaging/systemd/sync.env.example"))

    subprocess.run(
        ["sudo", "install", "-D", "-m", "0755", str(binary), str(SYSTEMD_BINARY_PATH)],
        check=True,
        timeout=15,
    )
    try:
        with tempfile.TemporaryDirectory(prefix="sdsync-systemd-argparse-") as raw_temp:
            temporary = Path(raw_temp)
            credentials = temporary / "credentials"
            credentials.mkdir(mode=0o700)
            password_credential = credentials / "dsm-password"
            password_credential.write_text("test-only-secret\n", encoding="utf-8")
            password_credential.chmod(0o600)

            source = temporary / "source"
            source.mkdir(mode=0o700)
            (source / "payload.txt").write_text("payload", encoding="utf-8")

            # Real systemd creates this directory itself via StateDirectory= before ever
            # running the unit; the wrapper never mkdir's its lock file's parent.
            state = temporary / "state"
            state.mkdir(mode=0o700)

            merged = dict(shipped)
            merged.update(
                {
                    "SDSYNC_SOURCE": str(source),
                    "SDSYNC_URL": "https://files.example.invalid",
                    "SDSYNC_LOCK_FILE": str(state / "service.lock"),
                }
            )
            environment = {
                key: value for key, value in os.environ.items() if not key.startswith("SDSYNC_")
            }
            environment.update(merged)
            environment["CREDENTIALS_DIRECTORY"] = str(credentials)

            for delete_value in (shipped["SDSYNC_DELETE"], "true"):
                run_environment = dict(environment)
                run_environment["SDSYNC_DELETE"] = delete_value
                result = subprocess.run(
                    ["/bin/sh", str(wrapper)],
                    env=run_environment,
                    capture_output=True,
                    text=True,
                    timeout=20,
                    check=False,
                )
                _assert_no_clap_conflict(
                    result,
                    "systemd wrapper's shipped example env file with "
                    f"SDSYNC_DELETE={delete_value}",
                )
    finally:
        subprocess.run(["sudo", "rm", "-f", str(SYSTEMD_BINARY_PATH)], check=False, timeout=15)


def validate_windows_wrapper_argument_vector() -> None:
    """Assert the Task Scheduler installer's constructed argv against the real binary.

    packaging/windows/Install-SynologyDriveSyncTask.ps1 never executes the sync binary
    itself for the documented direct-mode invocation (packaging/windows/README.md); it
    only hands its constructed argument list to New-ScheduledTaskAction, for Task
    Scheduler to invoke later. That list construction also depends on the ScheduledTasks
    module and WindowsIdentity, both Windows-only, so faithfully running the real script
    on a Linux CI runner is not practical even with cmdlet mocking. This instead
    reconstructs the exact argv the installer builds for its documented example
    (-Source -Remote -Url -Username -At, --delete unset so --no-delete is appended) and
    asserts the real binary parses it, with an ambient SDSYNC_DELETE the way a persisted
    Windows user environment variable would be inherited by the scheduled task. The URL is
    swapped for the guaranteed-unreachable *.invalid TLD so this never dials a real host,
    the same convention tests/command_e2e.rs and this file's Docker checks already use.
    """
    binary = _validator_binary()
    if binary is None:
        print("skipped Windows wrapper argument-vector check: no built binary found")
        return

    with tempfile.TemporaryDirectory(prefix="sdsync-windows-argparse-") as raw_temp:
        temporary = Path(raw_temp)
        source = temporary / "source"
        source.mkdir()
        (source / "payload.txt").write_text("payload", encoding="utf-8")
        log_file = temporary / "logs" / "sync.log"

        # Mirrors Install-SynologyDriveSyncTask.ps1's direct-mode $arguments construction
        # for the packaging/windows/README.md example, with every default left in place:
        # Jobs=2, --delete unset, LogFormat json, Progress never.
        arguments = [
            "sync",
            str(source),
            "/team/export",
            "--url",
            "https://files.example.invalid",
            "--username",
            "mirror-bot",
            "--jobs",
            "2",
            "--quiet",
            "--log-format",
            "json",
            "--log-file",
            str(log_file),
            "--progress",
            "never",
            "--no-delete",
        ]

        base_environment = {
            key: value for key, value in os.environ.items() if not key.startswith("SDSYNC_")
        }
        for delete_value in ("false", "true"):
            environment = base_environment | {"SDSYNC_DELETE": delete_value}
            result = subprocess.run(
                [str(binary), *arguments],
                env=environment,
                capture_output=True,
                text=True,
                timeout=15,
                check=False,
            )
            _assert_no_clap_conflict(
                result,
                "Task Scheduler installer's constructed argv with ambient "
                f"SDSYNC_DELETE={delete_value}",
            )


def main() -> int:
    validate_systemd()
    validate_cron()
    validate_launchd()
    validate_windows()
    validate_docker()
    validate_docker_runner_behavior()
    validate_rendered_compose_identity()
    validate_installers_and_docs()
    validate_negation_flag_regressions()
    validate_authentication_conflict_regressions()
    validate_credentials_conflict_regressions()
    validate_batch_profile_conflict_still_enforced()
    validate_cron_wrapper_argument_parsing()
    validate_synology_wrapper_argument_parsing()
    validate_systemd_wrapper_argument_parsing()
    validate_windows_wrapper_argument_vector()
    print("validated service-management contracts across systemd, launchd, Windows, cron, and Docker")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except AssertionError as error:
        print(f"service asset validation failed: {error}", file=sys.stderr)
        raise SystemExit(1)
