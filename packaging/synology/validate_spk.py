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
    *{f"ui/images/icon_{size}.png" for size in UI_ICON_SIZES},
}
APP_ID = "com.supermarsx.SynologyDriveSync"
DSM_MINIMUM = "7.0-40759"
DSM_MAXIMUM = "7.4-99999"
FIXED_INFO = {
    "package": PACKAGE,
    "os_min_ver": DSM_MINIMUM,
    "os_max_ver": DSM_MAXIMUM,
    "displayname": "Synology Drive Sync",
    "description": (
        "One-way local-folder sync to configurable remote Synology File Station folders"
    ),
    "maintainer": "supermarsx",
    "maintainer_url": "https://github.com/supermarsx/synology-drive-sync",
    "helpurl": "https://github.com/supermarsx/synology-drive-sync/tree/main/packaging/synology",
    "thirdparty": "yes",
    "ctl_stop": "yes",
    "ctl_uninstall": "yes",
    "precheckstartstop": "yes",
    "silent_install": "yes",
    "silent_upgrade": "yes",
    "silent_uninstall": "no",
    "dsmuidir": "ui",
    "dsmappname": APP_ID,
}
WEBMAN_ROUTE_ROOT = PurePosixPath("/webman/3rdparty") / PACKAGE
WEBMAN_ENTRYPOINT = (WEBMAN_ROUTE_ROOT / "index.html").as_posix()
DSM_UI_ENTRYPOINT = PurePosixPath(FIXED_INFO["dsmuidir"]) / "index.html"
REQUIRED_INFO = set(FIXED_INFO) | {"version", "arch", "extractsize"}
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
    if archive.pax_headers:
        raise ValidationError(f"{label} contains unsupported global PAX headers")
    members: dict[str, tarfile.TarInfo] = {}
    for member in archive.getmembers():
        if member.pax_headers:
            raise ValidationError(
                f"{label} contains unsupported member PAX headers: {member.name}"
            )
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


def load_unique_json(payload: bytes, label: str) -> object:
    def unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
        result: dict[str, object] = {}
        for key, value in pairs:
            if key in result:
                raise ValidationError(f"{label} contains duplicate JSON key: {key}")
            result[key] = value
        return result

    return json.loads(payload, object_pairs_hook=unique_object)


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
    unexpected = values.keys() - REQUIRED_INFO
    if unexpected:
        raise ValidationError(f"INFO contains unknown fields: {sorted(unexpected)}")
    return values


def png_dimensions(payload: bytes) -> tuple[int, int]:
    if len(payload) < 24 or payload[:8] != b"\x89PNG\r\n\x1a\n" or payload[12:16] != b"IHDR":
        raise ValidationError("package icon is not a PNG with an IHDR chunk")
    return struct.unpack_from(">II", payload, 16)


def webman_payload_member(url: object) -> str:
    if not isinstance(url, str) or url != WEBMAN_ENTRYPOINT:
        raise ValidationError(
            f"ui/config url must be the canonical DSM Webman entry point "
            f"{WEBMAN_ENTRYPOINT!r}"
        )
    relative = PurePosixPath(url).relative_to(WEBMAN_ROUTE_ROOT)
    member = PurePosixPath(FIXED_INFO["dsmuidir"]) / relative
    if member != DSM_UI_ENTRYPOINT:
        raise ValidationError(
            "ui/config Webman route does not map to the packaged DSM UI entry point"
        )
    return member.as_posix()


def validate_ui_config(payload: bytes) -> str:
    model = load_unique_json(payload, "ui/config")
    if not isinstance(model, dict):
        raise ValidationError("ui/config must be a JSON object")
    applications = model.get(".url")
    if not isinstance(applications, dict) or set(applications) != {APP_ID}:
        raise ValidationError("ui/config must register exactly the DSM application id")
    application = applications[APP_ID]
    if not isinstance(application, dict):
        raise ValidationError("ui/config application entry must be an object")
    entrypoint = webman_payload_member(application.get("url"))
    expected = {
        "type": "url",
        "icon": "images/icon_{0}.png",
        "title": "Synology Drive Sync",
        "desc": "Configure, diagnose, and monitor one-way File Station sync",
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
        raise ValidationError("ui/config preloadTexts does not match notification texts")
    if set(application) != set(expected) | {"url", "preloadTexts"}:
        raise ValidationError("ui/config contains an unreviewed DSM application property")
    return entrypoint


def validate_ui_texts(strings_payload: bytes) -> None:
    strings = strings_payload.decode("utf-8")
    sections: dict[str, dict[str, str]] = {}
    current: dict[str, str] | None = None
    for line_number, line in enumerate(strings.splitlines(), 1):
        if not line:
            continue
        section = re.fullmatch(r"\[([a-z][a-z0-9_]*)\]", line)
        if section:
            name = section.group(1)
            if name in sections:
                raise ValidationError(f"ui notification strings duplicate section [{name}]")
            current = {}
            sections[name] = current
            continue
        entry = re.fullmatch(r'([a-z][a-z0-9_]*)="([^"\r\n]+)"', line)
        if current is None or entry is None:
            raise ValidationError(
                f"ui notification strings contain malformed line {line_number}"
            )
        key, value = entry.groups()
        if key in current:
            raise ValidationError(f"ui notification strings duplicate key {key}")
        current[key] = value
    expected = {
        "app": {"title", "description"},
        "notifications": {
            f"{event}_{suffix}"
            for event in NOTIFICATION_KEYS
            for suffix in ("title", "message")
        },
    }
    if {section: set(values) for section, values in sections.items()} != expected:
        raise ValidationError(
            "ui notification strings must contain only the reviewed sections and keys"
        )
    if re.search(r"%[A-Z][A-Z0-9_]*%", strings):
        raise ValidationError("ui notification text must be fixed and non-interpolating")


def validate_notifier(payload: bytes) -> None:
    source = payload.decode("utf-8")
    legacy = "/usr/syno/bin/synonotify"
    direct = "/usr/syno/bin/synodsmnotify"
    if legacy in source:
        raise ValidationError("DSM notifier must not depend on reserved synonotify events")
    availability = f"if [ -x {direct} ] && [ ! -L {direct} ]; then"
    if source.count(availability) != 1 or source.count(direct) != 5:
        raise ValidationError("DSM notifier executable validation is not the reviewed contract")
    normalized = re.sub(r"\\\r?\n[ \t]*", " ", source)
    normalized = re.sub(r"[ \t]+", " ", normalized)
    for event in NOTIFICATION_KEYS:
        command = (
            f"if {direct} -c {APP_ID} @administrators "
            f"{PACKAGE}:notifications:{event}_title "
            f"{PACKAGE}:notifications:{event}_message "
            ">/dev/null 2>&1; then return 0; else notify_status=$?; fi"
        )
        if normalized.count(command) != 1:
            raise ValidationError(
                f"DSM notifier must use only fixed reviewed argv for {event}"
            )


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
    headers_start = script.find("  function authenticatedHeaders(")
    headers_end = script.find("\n  async function apiGet(", headers_start)
    if headers_start < 0 or headers_end < 0:
        raise ValidationError("DSM UI script is missing the authenticated-header bridge")
    authenticated_headers = script[headers_start:headers_end]
    marker_names = re.findall(r'["\']X-SDSYNC-Request["\']', script)
    marker_values = re.findall(
        r'const\s+authenticated\s*=\s*Object\.assign\(\s*\{\s*\}\s*,\s*headers\s*,\s*\{\s*'
        r'["\']X-SDSYNC-Request["\']\s*:\s*["\']([^"\']*)["\']\s*\}\s*\)\s*;',
        authenticated_headers,
    )
    if (
        len(marker_names) != 1
        or marker_values != ["1"]
        or authenticated_headers.count("return authenticated;") != 1
    ):
        raise ValidationError("DSM UI must emit exactly one X-SDSYNC-Request header with value 1")
    for method, start_marker, end_marker in (
        ("GET", "  async function apiGet(", "\n  function canMutate("),
        ("POST", "  async function apiPost(", "\n  function setConnected("),
    ):
        request_start = script.find(start_marker)
        request_end = script.find(end_marker, request_start)
        if request_start < 0 or request_end < 0:
            raise ValidationError(f"DSM UI script is missing the {method} request bridge")
        request_source = script[request_start:request_end]
        if len(re.findall(r"\bheaders\s*:\s*authenticatedHeaders\s*\(", request_source)) != 1:
            raise ValidationError(
                f"DSM UI {method} requests must emit X-SDSYNC-Request through authenticatedHeaders"
            )
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
    model = load_unique_json(payload, "conf/privilege")
    if not isinstance(model, dict):
        raise ValidationError("conf/privilege must be a JSON object")

    def reject_elevation(value: object) -> None:
        if isinstance(value, dict):
            for key, nested in value.items():
                if "capabilit" in str(key).lower():
                    raise ValidationError("conf/privilege requests Linux capabilities")
                reject_elevation(nested)
        elif isinstance(value, list):
            for nested in value:
                reject_elevation(nested)
        elif isinstance(value, str) and value.lower() == "root":
            raise ValidationError("conf/privilege requests the root identity")

    reject_elevation(model)

    # Fail a setid request with a specific error even though the rootless
    # contract below does not allow a tool stanza at all. This keeps tampered
    # manifests from being mistaken for harmless schema drift.
    tools = model.get("tool", [])
    if isinstance(tools, list):
        for entry in tools:
            if not isinstance(entry, dict):
                continue
            permission = entry.get("permission")
            if isinstance(permission, str) and re.fullmatch(r"[0-7]{4}", permission):
                if int(permission, 8) & 0o6000:
                    raise ValidationError(
                        "conf/privilege tool permission requests setuid/setgid bits"
                    )

    expected = {
        "defaults": {"run-as": "package"},
        "join-groupname": "http",
    }
    if model != expected:
        raise ValidationError(
            "conf/privilege must use the reviewed rootless package/http contract"
        )


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
        HERE / "package/ui/config",
        HERE / "package/ui/index.html",
        HERE / "package/ui/app.css",
        HERE / "package/ui/app.js",
        HERE / "package/ui/images/icon.svg",
        HERE / "package/ui/texts/enu/strings",
    ] + [HERE / "scripts" / name for name in REQUIRED_SCRIPTS]
    missing = [str(path.relative_to(HERE)) for path in required_files if not path.is_file()]
    if missing:
        raise ValidationError(f"source package is missing files: {missing}")
    forbidden_resources = (
        HERE / "conf/resource",
        HERE / "package/ui/texts/enu/mails",
    )
    for path in forbidden_resources:
        if path.exists() or path.is_symlink():
            raise ValidationError(
                f"third-party source package must not acquire reserved resource: "
                f"{path.relative_to(HERE)}"
            )
    validate_privilege((HERE / "conf/privilege").read_bytes())
    ui_entrypoint = validate_ui_config((HERE / "package/ui/config").read_bytes())
    source_entrypoint = HERE / "package" / ui_entrypoint
    if source_entrypoint.is_symlink() or not source_entrypoint.is_file():
        raise ValidationError(
            "ui/config Webman route does not resolve to a regular source UI entry point"
        )
    validate_ui_texts((HERE / "package/ui/texts/enu/strings").read_bytes())
    validate_notifier((HERE / "package/libexec/sdsync-common").read_bytes())
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
    template_info = parse_info(template.encode("utf-8"))
    expected_template = {
        **FIXED_INFO,
        "version": "@DSM_VERSION@",
        "arch": "@ARCH@",
        "extractsize": "@EXTRACT_SIZE_KIB@",
    }
    if template_info != expected_template:
        raise ValidationError("INFO.template does not match the exact reviewed schema")
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
        if any(
            PurePosixPath(name).parts[:2] == ("conf", "resource")
            for name in members
        ):
            raise ValidationError(
                f"{path.name} must not acquire the reserved conf/resource interface"
            )
        required_outer = {
            "INFO", "package.tgz", "PACKAGE_ICON.PNG", "PACKAGE_ICON_256.PNG",
            "LICENSE", "LICENSES/musl-COPYRIGHT",
            "LICENSES/THIRD_PARTY_LICENSES.html", "conf/privilege",
        } | {f"scripts/{name}" for name in REQUIRED_SCRIPTS}
        missing = required_outer - members.keys()
        if missing:
            raise ValidationError(f"{path.name} is missing members: {sorted(missing)}")
        allowed_outer = required_outer | {"LICENSES", "scripts", "conf"}
        unexpected = members.keys() - allowed_outer
        if unexpected:
            raise ValidationError(
                f"{path.name} contains unexpected outer members: {sorted(unexpected)}"
            )
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
        declared_extract_kib = info["extractsize"]
        if not re.fullmatch(r"[1-9][0-9]*", declared_extract_kib):
            raise ValidationError(
                "INFO extractsize must be a canonical positive integer in KiB"
            )
        if requested_arch and requested_arch != arch:
            raise ValidationError(
                f"INFO arch {info_arch} resolves to {arch}, not requested {requested_arch}"
            )
        for key, expected in FIXED_INFO.items():
            if info[key] != expected:
                raise ValidationError(
                    f"INFO fixed field {key!r} must be {expected!r}"
                )
        expected_info_version = filename_info_version(path, arch)
        if info["version"] != expected_info_version:
            raise ValidationError(
                f"INFO version {info['version']!r} does not match filename version "
                f"{expected_info_version!r}"
            )
        validate_privilege(member_bytes(outer, members["conf/privilege"]))
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
        if any(
            PurePosixPath(name).parts[:4] == ("ui", "texts", "enu", "mails")
            for name in inner_members
        ):
            raise ValidationError(
                "package.tgz must not contain reserved sysnotify mail templates"
            )
        missing = REQUIRED_PAYLOAD - inner_members.keys()
        if missing:
            raise ValidationError(f"package.tgz is missing members: {sorted(missing)}")
        allowed_inner = REQUIRED_PAYLOAD | {
            "bin",
            "libexec",
            "share",
            "share/licenses",
            "ui",
            "ui/images",
            "ui/texts",
            "ui/texts/enu",
        }
        unexpected = inner_members.keys() - allowed_inner
        if unexpected:
            raise ValidationError(
                f"package.tgz contains unexpected inner members: {sorted(unexpected)}"
            )
        payload_bytes = sum(
            member.size for member in inner_members.values() if member.isfile()
        )
        expected_extract_kib = str((payload_bytes + 1023) // 1024)
        if declared_extract_kib != expected_extract_kib:
            raise ValidationError(
                f"INFO extractsize {declared_extract_kib} does not match "
                f"package.tgz regular-file size {expected_extract_kib} KiB"
            )
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
        ui_entrypoint = validate_ui_config(member_bytes(inner, inner_members["ui/config"]))
        if ui_entrypoint not in inner_members:
            raise ValidationError(
                "ui/config Webman route does not resolve inside package.tgz"
            )
        require_regular_mode(
            inner_members, ui_entrypoint, 0o644, "DSM Webman UI entry point"
        )
        validate_ui_texts(member_bytes(inner, inner_members["ui/texts/enu/strings"]))
        validate_notifier(member_bytes(inner, inner_members["libexec/sdsync-common"]))
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
