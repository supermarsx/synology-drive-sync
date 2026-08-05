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


def main() -> int:
    validate_systemd()
    validate_cron()
    validate_launchd()
    validate_windows()
    validate_docker()
    validate_docker_runner_behavior()
    validate_rendered_compose_identity()
    validate_installers_and_docs()
    print("validated service-management contracts across systemd, launchd, Windows, cron, and Docker")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except AssertionError as error:
        print(f"service asset validation failed: {error}", file=sys.stderr)
        raise SystemExit(1)
