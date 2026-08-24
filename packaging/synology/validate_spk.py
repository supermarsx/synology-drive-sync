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
    UI_ICON_SIZES,
    elf_contract,
    elf_data_contract,
    normalized_versions,
    png_icon,
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
    "bin/sdsync-dsm-api",
    "libexec/sdsync-common",
    "libexec/sdsync-controller",
    "libexec/sdsync-run",
    "share/licenses/synology-drive-sync-LICENSE",
    "share/licenses/musl-COPYRIGHT",
    "share/licenses/THIRD_PARTY_LICENSES.html",
    "ui/api.cgi",
    "ui/config",
    "ui/index.html",
    "ui/app.css",
    "ui/app.js",
    "ui/images/icon.svg",
    "ui/texts/enu/strings",
    "ui/texts/enu/mails",
    *{f"ui/images/icon_{size}.png" for size in UI_ICON_SIZES},
}
REQUIRED_INFO = {
    "package",
    "version",
    "os_min_ver",
    "os_max_ver",
    "description",
    "arch",
    "maintainer",
    "dsmuidir",
    "dsmappname",
}
APP_ID = "com.supermarsx.SynologyDriveSync"
DSM_MINIMUM = "7.0-40759"
DSM_MAXIMUM = "7.4-99999"
NOTIFICATION_KEYS = {"sync_succeeded", "sync_failed", "doctor_failed"}


class ValidationError(AssertionError):
    pass


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=sorted(ARCHITECTURES))
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--api-binary", type=Path)
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
        if member.mode & 0o6000:
            raise ValidationError(f"{label} contains setuid/setgid archive member: {member.name}")
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


def validate_resource(payload: bytes) -> None:
    model = json.loads(payload)
    expected = {
        "sysnotify": {
            "texts_dir": "ui/texts",
            "app_privileges": [
                {
                    "app_id": APP_ID,
                    "categories": ["Synology Drive Sync"],
                }
            ],
        }
    }
    if model != expected:
        raise ValidationError("conf/resource must register only the fixed sysnotify contract")


def validate_ui_config(payload: bytes) -> None:
    model = json.loads(payload)
    applications = model.get(".url")
    if not isinstance(applications, dict) or set(applications) != {APP_ID}:
        raise ValidationError("ui/config must register exactly the DSM application id")
    application = applications[APP_ID]
    if not isinstance(application, dict):
        raise ValidationError("ui/config application entry must be an object")
    expected = {
        "type": "url",
        "icon": "images/icon_{0}.png",
        "title": "Synology Drive Sync",
        "desc": "Configure, diagnose, and monitor one-way File Station sync",
        "url": "3rdparty/synology-drive-sync/index.html",
        "allUsers": False,
    }
    for key, value in expected.items():
        if application.get(key) != value:
            raise ValidationError(f"ui/config has invalid {key!r}")
    expected_preloads = {
        f"notifications:{event}_{suffix}"
        for event in NOTIFICATION_KEYS
        for suffix in ("title", "message")
    }
    if set(application.get("preloadTexts", [])) != expected_preloads:
        raise ValidationError("ui/config preloadTexts does not match sysnotify texts")
    if set(application) != set(expected) | {"preloadTexts"}:
        raise ValidationError("ui/config contains an unreviewed DSM application property")


def validate_ui_texts(strings_payload: bytes, mails_payload: bytes) -> None:
    strings = strings_payload.decode("utf-8")
    mails = mails_payload.decode("utf-8")
    for event in NOTIFICATION_KEYS:
        for suffix in ("title", "message"):
            if not re.search(rf"(?m)^{re.escape(event)}_{suffix}=\"[^\r\n]+\"$", strings):
                raise ValidationError(f"ui notification strings are missing {event}_{suffix}")
        if mails.count(f"[{event}]") != 1:
            raise ValidationError(f"ui notification mails are missing [{event}]")
    if set(re.findall(r"(?m)^\[([^\]]+)\]$", mails)) != NOTIFICATION_KEYS:
        raise ValidationError("ui notification mails contain an unregistered event")
    if "%PASSWORD%" in strings + mails or "%TOTP%" in strings + mails or "%TOKEN%" in strings + mails:
        raise ValidationError("ui notification text must never interpolate secret material")
    if re.search(r"(?m)^Category: (?!Synology Drive Sync$)", mails):
        raise ValidationError("ui notification category does not match conf/resource")


def validate_svg_icon(payload: bytes) -> None:
    source = payload.decode("utf-8")
    if '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256"' not in source:
        raise ValidationError("authored icon source must be a bounded 256x256 SVG")
    if '<rect x="16" y="16" width="224" height="224"' not in source:
        raise ValidationError("authored icon source must preserve the reviewed safe bounds")
    reviewed = source.replace('xmlns="http://www.w3.org/2000/svg"', "")
    if re.search(r"<(?:script|foreignObject)\b|(?:href|src)=|https?://", reviewed, re.IGNORECASE):
        raise ValidationError("authored icon source contains an external or executable construct")


def validate_ui_static(index_payload: bytes, css_payload: bytes, script_payload: bytes) -> None:
    index = index_payload.decode("utf-8")
    css = css_payload.decode("utf-8")
    script = script_payload.decode("utf-8")
    required_routes = {
        "overview", "profiles", "routines", "health", "activity",
        "notifications", "settings",
    }
    if set(re.findall(r'data-route="([a-z]+)"', index)) != required_routes:
        raise ValidationError("DSM UI navigation does not expose the required sections")
    csp_match = re.search(
        r'<meta\s+http-equiv="Content-Security-Policy"\s+content="([^"]+)"',
        index,
        re.IGNORECASE,
    )
    if not csp_match:
        raise ValidationError("DSM UI must define a Content-Security-Policy")
    csp = csp_match.group(1)
    for directive in (
        "default-src 'self'", "script-src 'self'", "style-src 'self'",
        "connect-src 'self'", "object-src 'none'", "base-uri 'none'",
        "form-action 'self'",
    ):
        if directive not in csp:
            raise ValidationError(f"DSM UI CSP is missing {directive}")
    if re.search(r"\son[a-z]+\s*=", index, re.IGNORECASE):
        raise ValidationError("DSM UI contains an inline event handler")
    referrer = '<meta name="referrer" content="no-referrer">'
    first_subresource = min(position for position in (index.find("<link"), index.find("<script")) if position >= 0)
    if index.find(referrer) < 0 or index.find(referrer) > first_subresource:
        raise ValidationError("DSM UI must suppress referrers before loading subresources")
    script_tags = re.findall(r"<script([^>]*)>(.*?)</script>", index, re.IGNORECASE | re.DOTALL)
    if not script_tags or any("src=" not in attributes.lower() or body.strip() for attributes, body in script_tags):
        raise ValidationError("DSM UI scripts must be external, local, and non-inline")
    for attribute in re.findall(r'(?:src|href)="([^"]+)"', index, re.IGNORECASE):
        if re.match(r"(?:[a-z]+:)?//", attribute, re.IGNORECASE):
            raise ValidationError(f"DSM UI loads an external asset: {attribute}")
    if re.search(r"url\(\s*['\"]?(?:https?:)?//", css, re.IGNORECASE):
        raise ValidationError("DSM UI CSS loads an external asset")
    if re.search(
        r"(?:fetch\s*\(|new\s+(?:WebSocket|EventSource)\s*\()\s*['\"](?:https?:)?//",
        script,
        re.IGNORECASE,
    ):
        raise ValidationError("DSM UI script contains an external network endpoint")
    for forbidden in ("eval(", "new Function(", ".innerHTML", "insertAdjacentHTML", "document.write("):
        if forbidden in script:
            raise ValidationError(f"DSM UI script contains forbidden DOM construct {forbidden}")
    for required in (
        "X-SYNO-TOKEN", "X-SDSYNC-CSRF", "crypto.getRandomValues",
        "request_id", "operation: action", "arguments: payload",
        "capabilities.mutations === true", 'hasCapability("secrets")',
        "has_remote_log_token", "remote-log-token",
        "sdsync.dsm-result-status.v1", 'result: Object.freeze(["job_id"])',
        "pollJobResult(queued.job_id)", "awaitTerminal === false", "expired_or_missing",
    ):
        if required not in script:
            raise ValidationError(f"DSM UI script is missing security contract {required!r}")
    if not re.search(r'name="connect_timeout"[^>]*\bmin="1"[^>]*\bmax="600"', index):
        raise ValidationError("DSM UI connect-timeout bounds drifted from the bridge")
    if not re.search(r'name="cooldown_seconds"[^>]*\bmin="60"[^>]*\bmax="604800"', index):
        raise ValidationError("DSM UI notification-cooldown bounds drifted from the bridge")
    for secret_name in ("password", "totp", "remote_log_token"):
        if re.search(rf"localStorage[^\n]*{secret_name}|{secret_name}[^\n]*localStorage", script, re.IGNORECASE):
            raise ValidationError(f"DSM UI persists {secret_name} in localStorage")
    if "prefers-reduced-motion" not in css or ":focus-visible" not in css:
        raise ValidationError("DSM UI CSS is missing reduced-motion or focus treatment")
    if "@media (max-width:" not in css:
        raise ValidationError("DSM UI CSS is not responsive")


def validate_privilege(payload: bytes) -> None:
    model = json.loads(payload)
    if model.get("defaults", {}).get("run-as") != "package":
        raise ValidationError("conf/privilege must default to run-as package")
    forbidden = json.dumps(model)
    if '"root"' in forbidden or "capabilities" in forbidden:
        raise ValidationError("conf/privilege requests root or Linux capabilities")
    expected = {
        "preinst", "postinst", "preuninst", "postuninst", "preupgrade",
        "postupgrade", "prestart", "start", "prestop", "stop", "status",
    }
    control_scripts = model.get("ctrl-script")
    if not isinstance(control_scripts, list):
        raise ValidationError("conf/privilege must declare lifecycle actions as a list")
    actions: set[str] = set()
    for entry in control_scripts:
        if not isinstance(entry, dict) or set(entry) != {"action", "run-as"}:
            raise ValidationError(
                "conf/privilege lifecycle entries must use only action and run-as"
            )
        action = entry.get("action")
        if not isinstance(action, str) or action not in expected:
            raise ValidationError("conf/privilege contains an invalid lifecycle action")
        if action in actions:
            raise ValidationError("conf/privilege contains a duplicate lifecycle action")
        if entry.get("run-as") != "package":
            raise ValidationError("conf/privilege lifecycle actions must run as package")
        actions.add(action)
    if actions != expected:
        raise ValidationError("conf/privilege does not explicitly cover every lifecycle action")
    tools = model.get("tool")
    if not isinstance(tools, list):
        raise ValidationError("conf/privilege must declare package-owned tools")
    tool_contract: dict[str, str] = {}
    for entry in tools:
        if not isinstance(entry, dict) or set(entry) != {"relpath", "user", "group", "permission"}:
            raise ValidationError("conf/privilege tool entries must use the reviewed fields")
        path = entry.get("relpath")
        if not isinstance(path, str) or path in tool_contract:
            raise ValidationError("conf/privilege contains an invalid or duplicate tool path")
        if entry.get("user") != "package" or entry.get("group") != "package":
            raise ValidationError("conf/privilege tools must remain package-owned")
        permission = entry.get("permission")
        if not isinstance(permission, str) or not re.fullmatch(r"[0-7]{4}", permission):
            raise ValidationError("conf/privilege contains an invalid tool mode")
        mode = int(permission, 8)
        if mode & 0o022:
            raise ValidationError("conf/privilege tool is group/world writable")
        if mode & 0o6000 and path != "ui/api.cgi":
            raise ValidationError("only the authenticated CGI bridge may be setuid/setgid")
        tool_contract[path] = permission
    expected_tools = {
        "bin/synology-drive-sync": "0755",
        "bin/sdsync-dsm": "0755",
        "bin/sdsync-dsm-api": "0755",
        "libexec/sdsync-common": "0755",
        "libexec/sdsync-controller": "0755",
        "libexec/sdsync-run": "0755",
        "ui/api.cgi": "4755",
    }
    if tool_contract != expected_tools:
        raise ValidationError("conf/privilege tool ownership/modes do not match the reviewed contract")


def validate_source() -> None:
    required_files = [
        HERE / "INFO.template",
        HERE / "conf/privilege",
        HERE / "conf/resource",
        HERE / "licenses/musl-COPYRIGHT",
        HERE / "build-spk.sh",
        HERE / "build_spk.py",
        HERE / "package/bin/sdsync-dsm",
        HERE / "package/libexec/sdsync-common",
        HERE / "package/libexec/sdsync-controller",
        HERE / "package/libexec/sdsync-run",
        HERE / "package/ui/config",
        HERE / "package/ui/index.html",
        HERE / "package/ui/app.css",
        HERE / "package/ui/app.js",
        HERE / "package/ui/images/icon.svg",
        HERE / "package/ui/texts/enu/strings",
        HERE / "package/ui/texts/enu/mails",
    ] + [HERE / "scripts" / name for name in REQUIRED_SCRIPTS]
    missing = [str(path.relative_to(HERE)) for path in required_files if not path.is_file()]
    if missing:
        raise ValidationError(f"source package is missing files: {missing}")
    validate_privilege((HERE / "conf/privilege").read_bytes())
    validate_resource((HERE / "conf/resource").read_bytes())
    validate_ui_config((HERE / "package/ui/config").read_bytes())
    validate_ui_texts(
        (HERE / "package/ui/texts/enu/strings").read_bytes(),
        (HERE / "package/ui/texts/enu/mails").read_bytes(),
    )
    validate_svg_icon((HERE / "package/ui/images/icon.svg").read_bytes())
    validate_ui_static(
        (HERE / "package/ui/index.html").read_bytes(),
        (HERE / "package/ui/app.css").read_bytes(),
        (HERE / "package/ui/app.js").read_bytes(),
    )
    template = (HERE / "INFO.template").read_text(encoding="utf-8")
    for token in ("@DSM_VERSION@", "@ARCH@", "@EXTRACT_SIZE_KIB@"):
        if template.count(token) != 1:
            raise ValidationError(f"INFO.template must contain {token} exactly once")
    for line in (
        f'os_min_ver="{DSM_MINIMUM}"',
        f'os_max_ver="{DSM_MAXIMUM}"',
        'dsmuidir="ui"',
        f'dsmappname="{APP_ID}"',
    ):
        if template.count(line) != 1:
            raise ValidationError(f"INFO.template must contain exactly {line}")
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
    path: Path,
    requested_arch: str | None,
    expected_binary: bytes | None = None,
    expected_api_binary: bytes | None = None,
) -> str:
    if path.is_symlink() or not path.is_file():
        raise ValidationError(f"SPK is not a non-symlink regular file: {path}")
    with tarfile.open(path, "r:*") as outer:
        members = safe_members(outer, path.name)
        required_outer = {
            "INFO", "package.tgz", "PACKAGE_ICON.PNG", "PACKAGE_ICON_256.PNG",
            "LICENSE", "LICENSES/musl-COPYRIGHT",
            "LICENSES/THIRD_PARTY_LICENSES.html", "conf/privilege", "conf/resource",
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
        if (
            info["package"] != PACKAGE
            or info["os_min_ver"] != DSM_MINIMUM
            or info["os_max_ver"] != DSM_MAXIMUM
            or info["dsmuidir"] != "ui"
            or info["dsmappname"] != APP_ID
        ):
            raise ValidationError("INFO package identity, DSM bounds, or UI registration is invalid")
        expected_info_version = filename_info_version(path, arch)
        if info["version"] != expected_info_version:
            raise ValidationError(
                f"INFO version {info['version']!r} does not match filename version "
                f"{expected_info_version!r}"
            )
        validate_privilege(member_bytes(outer, members["conf/privilege"]))
        validate_resource(member_bytes(outer, members["conf/resource"]))
        for script in REQUIRED_SCRIPTS:
            require_regular_mode(members, f"scripts/{script}", 0o755, "lifecycle script")
        package_icon = member_bytes(outer, members["PACKAGE_ICON.PNG"])
        package_icon_256 = member_bytes(outer, members["PACKAGE_ICON_256.PNG"])
        if png_dimensions(package_icon) != (64, 64):
            raise ValidationError("PACKAGE_ICON.PNG must be 64x64")
        if png_dimensions(package_icon_256) != (256, 256):
            raise ValidationError("PACKAGE_ICON_256.PNG must be 256x256")
        if package_icon != png_icon(64) or package_icon_256 != png_icon(256):
            raise ValidationError("Package Center icons do not match the deterministic source mark")
        payload = member_bytes(outer, members["package.tgz"])
    with tarfile.open(fileobj=io.BytesIO(payload), mode="r:gz") as inner:
        inner_members = safe_members(inner, f"{path.name}:package.tgz")
        missing = REQUIRED_PAYLOAD - inner_members.keys()
        if missing:
            raise ValidationError(f"package.tgz is missing members: {sorted(missing)}")
        executables = (
            "bin/synology-drive-sync", "bin/sdsync-dsm", "libexec/sdsync-common",
            "bin/sdsync-dsm-api", "libexec/sdsync-controller", "libexec/sdsync-run",
        )
        for executable in executables:
            require_regular_mode(inner_members, executable, 0o755, "payload executable")
        require_regular_mode(inner_members, "ui/api.cgi", 0o755, "authenticated CGI bridge")
        for name, member in inner_members.items():
            mode = member.mode & 0o7777
            if mode & 0o022:
                raise ValidationError(f"package payload is group/world writable: {name}")
            if mode & 0o6000:
                raise ValidationError(f"unexpected setuid/setgid payload member: {name}")
        embedded_binary = member_bytes(inner, inner_members["bin/synology-drive-sync"])
        elf_data_contract(embedded_binary, arch)
        if expected_binary is not None and embedded_binary != expected_binary:
            raise ValidationError("--binary bytes do not match the executable embedded in the SPK")
        embedded_api = member_bytes(inner, inner_members["bin/sdsync-dsm-api"])
        cgi_api = member_bytes(inner, inner_members["ui/api.cgi"])
        elf_data_contract(embedded_api, arch)
        if embedded_api != cgi_api:
            raise ValidationError("ui/api.cgi must exactly match bin/sdsync-dsm-api")
        if expected_api_binary is not None and embedded_api != expected_api_binary:
            raise ValidationError("--api-binary bytes do not match the helper embedded in the SPK")
        validate_ui_config(member_bytes(inner, inner_members["ui/config"]))
        validate_ui_texts(
            member_bytes(inner, inner_members["ui/texts/enu/strings"]),
            member_bytes(inner, inner_members["ui/texts/enu/mails"]),
        )
        validate_svg_icon(member_bytes(inner, inner_members["ui/images/icon.svg"]))
        validate_ui_static(
            member_bytes(inner, inner_members["ui/index.html"]),
            member_bytes(inner, inner_members["ui/app.css"]),
            member_bytes(inner, inner_members["ui/app.js"]),
        )
        for size in UI_ICON_SIZES:
            icon_name = f"ui/images/icon_{size}.png"
            icon_payload = member_bytes(inner, inner_members[icon_name])
            if png_dimensions(icon_payload) != (size, size):
                raise ValidationError(f"{icon_name} must be {size}x{size}")
            if icon_payload != png_icon(size):
                raise ValidationError(f"{icon_name} does not match the deterministic source mark")
    return arch


def main() -> int:
    args = arguments()
    validate_source()
    expected_binary = None
    expected_api_binary = None
    if args.binary:
        if not args.arch:
            raise ValidationError("--binary requires --arch")
        elf_contract(args.binary, args.arch)
        expected_binary = args.binary.read_bytes()
    if args.api_binary:
        if not args.arch:
            raise ValidationError("--api-binary requires --arch")
        elf_contract(args.api_binary, args.arch)
        expected_api_binary = args.api_binary.read_bytes()
    if bool(args.binary) != bool(args.api_binary):
        raise ValidationError("--binary and --api-binary must be supplied together")
    for path in args.spk:
        arch = validate_spk(path, args.arch, expected_binary, expected_api_binary)
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
