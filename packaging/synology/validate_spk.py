#!/usr/bin/env python3
"""Validate DSM package templates and built SPKs without installing anything."""

from __future__ import annotations

import argparse
import hashlib
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
    DSM_APP_CLASS,
    HERE,
    PackageError,
    UI_SOURCE,
    UI_HELP_PAGES,
    UI_ICON_SIZES,
    UI_MODULE_DIGEST_HEX_LENGTH,
    UI_SOURCE_MODULE,
    elf_contract,
    elf_data_contract,
    native_ui_module_name,
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
    "share/licenses/DSM_UI_THIRD_PARTY_LICENSES.txt",
    "ui/api.cgi",
    "ui/config",
    "ui/style.css",
    "ui/images/icon.svg",
    "ui/helptoc.conf",
    "ui/texts/enu/strings",
    *{f"ui/help/enu/{page}.html" for page in UI_HELP_PAGES},
    *{f"ui/images/icon_{size}.png" for size in UI_ICON_SIZES},
}
APP_ID = DSM_APP_CLASS
APP_NAMESPACE = "SYNO.SDS.App.SynologyDriveSync"
NATIVE_MODULE_PATTERN = re.compile(
    rf"SynologyDriveSync\.[0-9a-f]{{{UI_MODULE_DIGEST_HEX_LENGTH}}}\.js"
)
NATIVE_STYLE = "ui/style.css"
CANONICAL_API = "/webman/3rdparty/synology-drive-sync/api.cgi"
DSM_TOKEN_API = "/webapi/entry.cgi?api=SYNO.API.Auth&version=6&method=token"
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
    "auto_upgrade_from": "26.7-1",
    "dsmuidir": f"{PACKAGE}:ui",
    "dsmappname": APP_ID,
}
REQUIRED_INFO = set(FIXED_INFO) | {"version", "arch", "extractsize", "checksum"}
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


def validate_native_application(
    application: object, label: str, *, generated: bool = False
) -> None:
    if not isinstance(application, dict):
        raise ValidationError(f"{label} native AppWindow entry must be an object")
    expected = {
        "type": "app",
        "icon": "images/icon_{0}.png",
        "title": "Synology Drive Sync",
        "desc": "app:description",
        "appWindow": APP_ID,
        "allUsers": False,
        "allowMultiInstance": False,
        "hidden": False,
    }
    for key, value in expected.items():
        if application.get(key) != value:
            raise ValidationError(f"{label} has invalid native AppWindow property {key!r}")
    expected_preloads = {
        f"notifications:{event}_{suffix}"
        for event in NOTIFICATION_KEYS
        for suffix in ("title", "message")
    }
    preload_texts = application.get("preloadTexts")
    if (
        not isinstance(preload_texts, list)
        or any(not isinstance(value, str) for value in preload_texts)
        or len(preload_texts) != len(set(preload_texts))
        or set(preload_texts) != expected_preloads
    ):
        raise ValidationError(f"{label} preloadTexts does not match notification texts")
    allowed = set(expected) | {"preloadTexts"}
    if generated:
        if application.get("depend") != []:
            raise ValidationError(
                f"{label} must contain the deterministic empty DSM dependency list"
            )
        allowed.add("depend")
    if set(application) != allowed:
        raise ValidationError(f"{label} contains an unreviewed DSM application property")


def validate_source_app_config(payload: bytes) -> None:
    model = load_unique_json(payload, "ui-src/app.config")
    if not isinstance(model, dict) or set(model) != {APP_ID}:
        raise ValidationError("ui-src/app.config must define exactly the native AppWindow class")
    validate_native_application(model[APP_ID], "ui-src/app.config")


def validate_ui_config(payload: bytes) -> str:
    model = load_unique_json(payload, "ui/config")
    if not isinstance(model, dict) or len(model) != 1:
        raise ValidationError(
            "ui/config must register exactly one reviewed native JavaScript module"
        )
    module = next(iter(model))
    if not isinstance(module, str) or NATIVE_MODULE_PATTERN.fullmatch(module) is None:
        raise ValidationError(
            "ui/config native module must use the exact content-addressed filename contract"
        )
    applications = model[module]
    if not isinstance(applications, dict) or set(applications) != {APP_ID}:
        raise ValidationError("ui/config module must define exactly the native AppWindow class")
    validate_native_application(applications[APP_ID], "ui/config", generated=True)
    return f"ui/{module}"


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
        "help": set(UI_HELP_PAGES),
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


def validate_external_links(document: str, label: str) -> None:
    anchors = re.findall(r"<a\b[^>]*>", document, re.IGNORECASE)
    for anchor in anchors:
        if not re.search(r'\btarget=["\']_blank["\']', anchor, re.IGNORECASE):
            raise ValidationError(f"{label} external links must open in a new context")
        relation = re.search(r'\brel=["\']([^"\']+)["\']', anchor, re.IGNORECASE)
        relation_tokens = set(relation.group(1).lower().split()) if relation else set()
        if not {"noopener", "noreferrer"}.issubset(relation_tokens):
            raise ValidationError(f"{label} external links must prevent opener and referrer access")
    for tag in re.findall(r"<[^>]+>", document):
        if not re.search(r'\b(?:href|src)=["\'](?:https?:)?//', tag, re.IGNORECASE):
            continue
        if not re.match(r"<a\b", tag, re.IGNORECASE):
            raise ValidationError(f"{label} contains a remote asset")
        if not re.search(r'\bhref=["\']https://', tag, re.IGNORECASE):
            raise ValidationError(f"{label} external links must use HTTPS")


def validate_dsm_help(
    toc_payload: bytes,
    documents: dict[str, bytes],
) -> None:
    expected_toc = {
        "app": APP_ID,
        "title": "app:title",
        "content": "overview.html",
        "toc": [
            {"title": f"help:{page}", "content": f"{page}.html"}
            for page in UI_HELP_PAGES
        ],
    }
    if load_unique_json(toc_payload, "ui/helptoc.conf") != expected_toc:
        raise ValidationError(
            "ui/helptoc.conf must bind every reviewed dashboard section to the native AppWindow"
        )
    if set(documents) != set(UI_HELP_PAGES):
        raise ValidationError("DSM Help document set does not match the reviewed dashboard sections")
    for page, payload in documents.items():
        try:
            document = payload.decode("utf-8")
        except UnicodeDecodeError as error:
            raise ValidationError(f"DSM Help document {page}.html is not UTF-8") from error
        for marker in (
            '<html class="img-no-display">',
            '../../../../help/help.css',
            '../../../../help/scrollbar/flexcroll.css',
            '../../../../help/scrollbar/flexcroll.js',
            '../../../../help/scrollbar/initFlexcroll.js',
            "<h1>",
        ):
            if marker not in document:
                raise ValidationError(
                    f"DSM Help document {page}.html is missing {marker!r}"
                )
        validate_external_links(document, f"DSM Help document {page}.html")


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


def validate_native_api_source(payload: bytes) -> None:
    source = payload.decode("utf-8")
    endpoints = re.findall(r'["\']([^"\']*api\.cgi)["\']', source)
    if endpoints != [CANONICAL_API]:
        raise ValidationError("native DSM UI must use only the canonical absolute package CGI path")
    token_endpoints = re.findall(r'["\'](/webapi/[^"\']+)["\']', source)
    if token_endpoints != [DSM_TOKEN_API]:
        raise ValidationError("native DSM UI must use exactly the reviewed DSM token bootstrap path")
    for marker in (
        f'export const DSM_TOKEN_URL = "{DSM_TOKEN_API}";',
        'credentials: "same-origin"',
        'cache: "no-store"',
        'redirect: "error"',
        '"X-SDSYNC-CSRF": effectiveCsrfToken',
        'const MAX_DSM_TOKEN_RESPONSE_BYTES = 16 * 1024;',
        'const MAX_DSM_TOKEN_BYTES = 1024;',
        'const DSM_TOKEN_BOOTSTRAP_TIMEOUT_MS = 5000;',
        'const DSM_TOKEN_RETRY_DELAY_MS = 30000;',
        'let cachedDsmToken = "";',
        "encodeURIComponent(value)",
        "encoded.length <= MAX_DSM_TOKEN_BYTES",
        "async function bootstrapDsmToken()",
        "async function ensureDsmToken()",
        "let dsmAuthGeneration = 0;",
        "const csrfGenerationByAuth = new WeakMap();",
        "dsmAuthGeneration += 1;",
        "function dsmAuthSnapshot()",
        "function linkedAbortAttempt(parentSignal)",
        "const controller = new AbortControllerClass();",
        'parentSignal.addEventListener("abort", abort, { once: true });',
        'parentSignal.removeEventListener("abort", abort);',
        "requestSignal = auth && auth.signal ? auth.signal : undefined",
        "signal: requestSignal",
        "async function csrfForCurrentAuthGeneration(auth, csrfToken, dsmAuth, limits = null)",
        "const requestDsmAuth = dsmAuthSnapshot();",
        "const effectiveCsrfToken = await csrfForCurrentAuthGeneration(auth, csrfToken, requestDsmAuth, limits);",
        "crypto.getRandomValues",
        "request_id: id",
        "operation: action",
        "arguments: payload",
        "signal.addEventListener(\"abort\", cancel, { once: true })",
        "signal.removeEventListener(\"abort\", cancel)",
        "window.clearTimeout(timer)",
        "const RESULT_POLL_INTERVAL_MS = 2000;",
        "const RESULT_POLL_OBSERVATION_FAILURES = 5;",
        "const POST_DISPATCH_REPLAY_DELAYS_MS = Object.freeze([250, 1000]);",
        "const POST_DISPATCH_MAX_ATTEMPTS = POST_DISPATCH_REPLAY_DELAYS_MS.length + 1;",
        'export const REQUEST_STATUS_SCHEMA = "sdsync.dsm-request-status.v1";',
        '"request-status": Object.freeze(["request_id"])',
        "const RECONCILIATION_AUTH_TTL_MS = 5 * 60 * 1000;",
        "const reconciliationAuthByOwner = new WeakMap();",
        "export function purgeReconciliationAuth(auth)",
        "export async function reconcileMutationRequest(",
        "class QueuedOutcomeUnknownError extends Error",
        "this.outcomeUnknown = true;",
    ):
        if marker not in source:
            raise ValidationError(f"native DSM API source is missing security contract {marker!r}")
    if source.count('credentials: "same-origin"') != 3:
        raise ValidationError(
            "native DSM token bootstrap, GET, and POST must all use same-origin credentials"
        )
    if (
        source.count("function linkedAbortAttempt(parentSignal)") != 1
        or source.count("linkedAbortAttempt(auth && auth.signal)") != 4
        or source.count("linkedAbortAttempt(auth.signal)") != 1
        or source.count("signal: requestSignal") != 1
        or source.count("signal: requestAttempt.signal") != 1
        or source.count("if (onTimeout) onTimeout();") != 3
    ):
        raise ValidationError(
            "native DSM bounded requests must link, abort, and release AppWindow cancellation attempts"
        )
    for forbidden in (
        r"consumeLaunchToken",
        r"\bwindow\.location\b",
        r"\bwindow\.history\b",
        r"\bhistory\.replaceState\b",
        r"launch[- ]token",
        r"\blocalStorage\b",
        r"\bsessionStorage\b",
        r"\bindexedDB\b",
        r"\bdocument\.cookie\b",
    ):
        if re.search(forbidden, source, re.IGNORECASE):
            raise ValidationError(
                "native DSM API must not derive authentication from launch state or persistent storage"
            )

    auth_cache_start = source.find("const reconciliationAuthByOwner = new WeakMap();")
    auth_cache_end = source.find("\nexport class QueuedOutcomeUnknownError", auth_cache_start)
    if auth_cache_start < 0 or auth_cache_end < 0:
        raise ValidationError("native DSM API source is missing scoped reconciliation authentication")
    auth_cache_source = source[auth_cache_start:auth_cache_end]
    for marker in (
        "function reconciliationAuthOwner(auth)",
        "function reconciliationAuthExpired(remembered, now)",
        "function clearReconciliationAuthEntry(entries, requestId)",
        'remembered.token = "";',
        "function rememberReconciliationAuth(auth, requestId, dsmAuth)",
        "reconciliationAuthByOwner.get(owner)",
        "expiresAt: now + RECONCILIATION_AUTH_TTL_MS",
        "function rememberedReconciliationAuth(auth, requestId)",
        "function forgetReconciliationAuth(auth, requestId)",
        "export function purgeReconciliationAuth(auth)",
        "for (const requestId of [...entries.keys()])",
        "reconciliationAuthByOwner.delete(owner);",
    ):
        if marker not in auth_cache_source:
            raise ValidationError(
                f"native DSM reconciliation authentication cache is missing {marker!r}"
            )
    if "reconciliationAuthByRequestId" in source or "localStorage" in auth_cache_source:
        raise ValidationError(
            "native DSM reconciliation authentication must remain owner-scoped and memory-only"
        )

    bootstrap_start = source.find("async function bootstrapDsmToken()")
    bootstrap_end = source.find("\nfunction authenticatedHeaders(", bootstrap_start)
    if bootstrap_start < 0 or bootstrap_end < 0:
        raise ValidationError("native DSM API source is missing the bounded DSM token bootstrap")
    bootstrap_source = source[bootstrap_start:bootstrap_end]
    for marker in (
        "window.fetch(DSM_TOKEN_URL",
        'method: "GET"',
        'credentials: "same-origin"',
        'cache: "no-store"',
        'redirect: "error"',
        'headers: { Accept: "application/json" }',
        "response.redirected",
        "response.status !== 200",
        'response.headers.get("content-type")',
        'response.headers.get("content-length")',
        "MAX_DSM_TOKEN_RESPONSE_BYTES",
        "boundedUtf8Length(body)",
        "JSON.parse(body)",
        "model.success !== true",
        "normalizeDsmToken(data.synotoken)",
    ):
        if marker not in bootstrap_source:
            raise ValidationError(
                f"native DSM token bootstrap is missing bounded contract {marker!r}"
            )
    if "console." in bootstrap_source or re.search(
        r"[?&](?:SynoToken|synotoken)=", source, re.IGNORECASE
    ):
        raise ValidationError("native DSM token bootstrap must not log or place the token in a URL")

    headers_start = source.find("function authenticatedHeaders(")
    headers_end = source.find("\nfunction exactKeys(", headers_start)
    if headers_start < 0 or headers_end < 0:
        raise ValidationError("native DSM API source is missing the authenticated-header bridge")
    authenticated_headers = source[headers_start:headers_end]
    if "function authenticatedHeaders(headers, dsmAuth)" not in authenticated_headers:
        raise ValidationError(
            "native DSM headers must use only the pinned module-owned authentication snapshot"
        )
    marker_names = re.findall(r'["\']X-SDSYNC-Request["\']', source)
    if (
        len(marker_names) != 1
        or 'Object.assign({}, headers, { "X-SDSYNC-Request": "1" })' not in authenticated_headers
    ):
        raise ValidationError("native DSM UI must emit exactly one X-SDSYNC-Request header with value 1")
    if source.count('"X-SYNO-TOKEN"') != 1 or (
        'authenticated["X-SYNO-TOKEN"] = dsmAuth.token' not in authenticated_headers
    ):
        raise ValidationError(
            "native DSM UI must emit the bounded in-memory DSM token only through X-SYNO-TOKEN"
        )
    for method, start_marker, end_marker in (
        ("GET", "export async function apiGet(", "\nfunction delay("),
        ("POST", "export async function apiPost(", None),
    ):
        request_start = source.find(start_marker)
        request_end = len(source) if end_marker is None else source.find(end_marker, request_start)
        if request_start < 0 or request_end < 0:
            raise ValidationError(f"native DSM API source is missing the {method} request bridge")
        request_source = source[request_start:request_end]
        if request_source.count("await ensureDsmToken();") != 1:
            raise ValidationError(
                f"native DSM {method} requests must await the shared DSM token bootstrap"
            )
        if method == "GET":
            configured_limits = request_source.find(
                "const configuredLimits = normalizedRequestLimits(options);"
            )
            default_limits = request_source.find(
                "const limits = configuredLimits || terminalAttemptLimits();",
                configured_limits,
            )
            auth_snapshot = request_source.find(
                "const requestDsmAuth = dsmAuthSnapshot();"
            )
            deferred_generation = request_source.find(
                'const deferredCsrfGeneration = action === "csrf";'
            )
            bounded_attempt = request_source.find(
                "const attempt = linkedAbortAttempt(auth && auth.signal);",
                deferred_generation,
            )
            bounded_acceptance = request_source.find(
                "model = await withinLimit(", bounded_attempt
            )
            bounded_request = request_source.find(
                "apiGetWithDsmAuth(", bounded_acceptance
            )
            bounded_signal = request_source.find(
                "attempt.signal", bounded_request
            )
            bounded_abort = request_source.find(
                "attempt.abort", bounded_signal
            )
            bounded_release = request_source.find(
                "attempt.release();", bounded_abort
            )
            deferred_commit_guard = request_source.find(
                "if (deferredCsrfGeneration) {", bounded_release
            )
            deferred_commit = request_source.find(
                "rememberCsrfGeneration(auth, model, requestDsmAuth.generation)",
                deferred_commit_guard,
            )
            model_return = request_source.find("return model;", deferred_commit)
            if (
                "export async function apiGet(auth, action, parameters = {}, options = undefined)"
                not in request_source
                or request_source.count("apiGetWithDsmAuth(") != 1
                or request_source.count("!deferredCsrfGeneration") != 1
                or request_source.count(
                    "rememberCsrfGeneration(auth, model, requestDsmAuth.generation)"
                )
                != 1
                or "limits.readTimeoutMs" not in request_source
                or '"read_observation"' not in request_source
                or "client read limit" not in request_source
                or "if (!limits)" in request_source
                or not (
                    0
                    <= configured_limits
                    < default_limits
                    < auth_snapshot
                    < deferred_generation
                    < bounded_attempt
                    < bounded_acceptance
                    < bounded_request
                    < bounded_signal
                    < bounded_abort
                    < bounded_release
                    < deferred_commit_guard
                    < deferred_commit
                    < model_return
                )
            ):
                raise ValidationError(
                    "native DSM bounded GET reads must abort linked fetches and suppress detached CSRF generation writes"
                )
        elif len(re.findall(r"\bheaders\s*:\s*authenticatedHeaders\s*\(", request_source)) != 1:
            raise ValidationError(
                "native DSM POST requests must emit X-SDSYNC-Request through authenticatedHeaders"
            )

    get_dispatch_start = source.find("async function apiGetWithDsmAuth(")
    get_dispatch_end = source.find("\nexport async function apiGet(", get_dispatch_start)
    if get_dispatch_start < 0 or get_dispatch_end < 0:
        raise ValidationError("native DSM API source is missing exact-snapshot GET dispatch")
    get_dispatch_source = source[get_dispatch_start:get_dispatch_end]
    if (
        "requestSignal = auth && auth.signal ? auth.signal : undefined"
        not in get_dispatch_source
        or get_dispatch_source.count("fetch(endpoint(action, parameters), {") != 1
        or get_dispatch_source.count("signal: requestSignal") != 1
        or get_dispatch_source.count("headers: authenticatedHeaders(") != 1
        or "{ Accept: \"application/json\" }, dsmAuth" not in get_dispatch_source
        or 'if (rememberGeneration && action === "csrf") {' not in get_dispatch_source
        or get_dispatch_source.count(
            "rememberCsrfGeneration(auth, model, dsmAuth.generation)"
        )
        != 1
        or "ensureDsmToken" in get_dispatch_source
    ):
        raise ValidationError(
            "native DSM GET requests must use one pinned authentication snapshot without re-bootstrap"
        )

    csrf_reissue_start = source.find("async function csrfForCurrentAuthGeneration(")
    csrf_reissue_end = source.find("\nexport async function apiPost(", csrf_reissue_start)
    if csrf_reissue_start < 0 or csrf_reissue_end < 0:
        raise ValidationError("native DSM API source is missing bounded CSRF generation reissue")
    csrf_reissue_source = source[csrf_reissue_start:csrf_reissue_end]
    bounded_branch = csrf_reissue_source.find("if (limits) {")
    bounded_attempt = csrf_reissue_source.find(
        "const attempt = linkedAbortAttempt(auth.signal);", bounded_branch
    )
    bounded_wait = csrf_reissue_source.find("model = await withinLimit(", bounded_attempt)
    bounded_request = csrf_reissue_source.find(
        'apiGetWithDsmAuth(auth, "csrf", {}, dsmAuth, false, attempt.signal)',
        bounded_wait,
    )
    bounded_abort = csrf_reissue_source.find("attempt.abort", bounded_request)
    bounded_release = csrf_reissue_source.find("attempt.release();", bounded_abort)
    unbounded_branch = csrf_reissue_source.find("} else {", bounded_release)
    unbounded_request = csrf_reissue_source.find(
        'model = await apiGetWithDsmAuth(auth, "csrf", {}, dsmAuth, false);',
        unbounded_branch,
    )
    model_validation = csrf_reissue_source.find(
        "if (!validCsrfModel(model))", unbounded_request
    )
    generation_commit = csrf_reissue_source.find(
        "rememberCsrfGeneration(auth, model, dsmAuth.generation)", model_validation
    )
    replacement_publish = csrf_reissue_source.find(
        'if (typeof auth.onCsrfReissued === "function")', generation_commit
    )
    if (
        "async function csrfForCurrentAuthGeneration(auth, csrfToken, dsmAuth, limits = null)"
        not in csrf_reissue_source
        or csrf_reissue_source.count("apiGetWithDsmAuth(") != 2
        or csrf_reissue_source.count(
            "rememberCsrfGeneration(auth, model, dsmAuth.generation)"
        )
        != 1
        or "limits.csrfReissueTimeoutMs" not in csrf_reissue_source
        or '"csrf_reissue"' not in csrf_reissue_source
        or not (
            0
            <= bounded_branch
            < bounded_attempt
            < bounded_wait
            < bounded_request
            < bounded_abort
            < bounded_release
            < unbounded_branch
            < unbounded_request
            < model_validation
            < generation_commit
            < replacement_publish
        )
    ):
        raise ValidationError(
            "native DSM bounded CSRF reissue must abort its linked read and commit only after an accepted valid response"
        )

    poll_start = source.find("async function pollJobResult(")
    poll_end = source.find("\nfunction requestId(", poll_start)
    if poll_start < 0 or poll_end < 0:
        raise ValidationError("native DSM API source is missing queued-result observation")
    poll_source = source[poll_start:poll_end]
    for marker in (
        "pollIntervalMs = RESULT_POLL_INTERVAL_MS",
        "const attempt = linkedAbortAttempt(auth && auth.signal);",
        '"result",\n          { job_id: jobId },',
        "attempt.signal",
        "attempt.abort",
        "attempt.release();",
        "limits = null",
        "observation = null",
        "limits.resultRequestTimeoutMs",
        "queuedObservationTimeout(",
        "let consecutiveObservationFailures = 0;",
        "for (;;)",
        "if (auth && auth.signal && auth.signal.aborted) throw error;",
        "consecutiveObservationFailures += 1;",
        "if (!observation && consecutiveObservationFailures >= RESULT_POLL_OBSERVATION_FAILURES)",
        "consecutiveObservationFailures = 0;",
        "status.state === \"pending\"",
        "status.state === \"expired_or_missing\"",
        "status.result.ok === false",
        "status.result.ok !== true && status.result.ok !== false",
        "status.client_request_id !== requestId",
        "failure.resultOutput = boundedText(",
        "status.result.output",
    ):
        if marker not in poll_source:
            raise ValidationError(
                f"native DSM queued-result observer is missing {marker!r}"
            )
    if poll_source.count("apiGetWithDsmAuth(") != 1:
        raise ValidationError(
            "native DSM queued-result observer must use one linked GET attempt per poll"
        )
    if poll_source.count(
        "await delay(interval, auth && auth.signal, limits, observation);"
    ) != 2:
        raise ValidationError(
            "native DSM queued-result observer must retry pending and transport observations"
        )
    if poll_source.count("throw new QueuedOutcomeUnknownError(") != 4:
        raise ValidationError(
            "native DSM queued-result observer must distinguish all outcome-unknown states"
        )
    for forbidden in (
        "RESULT_POLL_ATTEMPTS",
        "for (let attempt",
        "within two minutes",
        "poll deadline",
        "poll timeout",
    ):
        if forbidden.lower() in poll_source.lower():
            raise ValidationError(
                "native DSM queued-result observer must not invent a client terminal horizon"
            )

    request_status_start = source.find("function exactRequestStatusKeys(")
    request_status_end = source.find("\nasync function csrfForCurrentAuthGeneration(", request_status_start)
    if request_status_start < 0 or request_status_end < 0:
        raise ValidationError("native DSM API source is missing authenticated request reconciliation")
    request_status_source = source[request_status_start:request_status_end]
    for marker in (
        "function trustedRequestStatus(model, requestId, expectedOperation)",
        "model.schema !== REQUEST_STATUS_SCHEMA || model.request_id !== requestId",
        'model.state === "unresolved"',
        '["request_id", "schema", "state"]',
        '["job_id", "operation", "request_id", "schema", "state"]',
        "model.operation !== expectedOperation",
        "const attempt = linkedAbortAttempt(auth && auth.signal);",
        '"request-status",\n        { request_id: requestId },',
        "dsmAuth,\n        true,\n        attempt.signal",
        "limits.readTimeoutMs",
        "attempt.abort",
        "attempt.release();",
        "if (model.state !== \"unresolved\") return model;",
        "limits.requestReconciliationPollIntervalMs",
        "limits.requestReconciliationTimeoutMs",
        "export async function reconcileMutationRequest(",
        "rememberedReconciliationAuth(auth, trustedRequestId)",
        "const recovered = await recoverQueuedRequest(",
        "const result = await awaitQueuedResult(",
        'schema: "sdsync.dsm-reconciled-result.v1"',
        "operation: expectedOperation",
        "forgetReconciliationAuth(auth, trustedRequestId);",
    ):
        if marker not in request_status_source:
            raise ValidationError(
                f"native DSM request reconciliation is missing {marker!r}"
            )
    if (
        request_status_source.count('"request-status"') != 1
        or "fetch(API_URL" in request_status_source
        or "apiPost(" in request_status_source
    ):
        raise ValidationError(
            "native DSM request reconciliation must be a single authenticated read path"
        )

    post_start = source.find("export async function apiPost(")
    post_source = source[post_start:] if post_start >= 0 else ""
    for marker in (
        "const TERMINAL_API_ATTEMPT_TIMEOUTS = Object.freeze({",
        "csrfReissueTimeoutMs: 10000",
        "postRequestTimeoutMs: 45000",
        "postResponseTimeoutMs: 10000",
        "readTimeoutMs: 10000",
        "resultRequestTimeoutMs: 10000",
        "function terminalAttemptLimits()",
        "resultObservationTimeoutMs: null",
        "setTimer: (callback, milliseconds) => window.setTimeout(callback, milliseconds)",
        "clearTimer: (timer) => window.clearTimeout(timer)",
    ):
        if marker not in source:
            raise ValidationError(
                f"native DSM terminal POST attempt bounds are missing {marker!r}"
            )
    if post_start < 0 or (
        "if (!awaitTerminal) {" not in post_source
        or "const result = await awaitQueuedResult(" not in post_source
        or "forgetReconciliationAuth(auth, id);" not in post_source
        or "error.outcomeUnknown !== true && error.requiresInspection !== true" not in post_source
    ):
        raise ValidationError(
            "native DSM POST must choose terminal observation and retain recoverable inspection authentication"
        )
    auth_snapshot = post_source.find("const requestDsmAuth = dsmAuthSnapshot();")
    csrf_refresh = post_source.find(
        "const effectiveCsrfToken = await csrfForCurrentAuthGeneration(auth, csrfToken, requestDsmAuth, limits);"
    )
    request_identity = post_source.find("const id = requestId();")
    serialized_request = post_source.find("const request = JSON.stringify({", request_identity)
    replay_loop = post_source.find(
        "for (let attempt = 0; attempt < POST_DISPATCH_MAX_ATTEMPTS; attempt += 1) {",
        serialized_request,
    )
    post_dispatch = post_source.find("const dispatched = fetch(API_URL, {")
    if not (
        0
        <= auth_snapshot
        < csrf_refresh
        < request_identity
        < serialized_request
        < replay_loop
        < post_dispatch
    ):
        raise ValidationError(
            "native DSM POST must pin authentication and one request identity before exact replay"
        )
    for marker in (
        "const boundedObservationLimits = normalizedRequestLimits(options);",
        "const limits = boundedObservationLimits || terminalAttemptLimits();",
        "let dispatchAmbiguous = false;",
        "attempt < POST_DISPATCH_MAX_ATTEMPTS",
        "const requestAttempt = linkedAbortAttempt(auth && auth.signal);",
        "signal: requestAttempt.signal",
        "body: request",
        "queued.request_id === id",
        "validJobId(queued.job_id)",
        "dispatchAmbiguous = true;",
        "recovered = await requestStatusOnce(",
        "const recovered = await recoverQueuedRequest(",
        "POST_DISPATCH_REPLAY_DELAYS_MS[attempt]",
        "requestAttempt.release();",
        "throw dispatchedOutcomeUnknown(id, action, dispatchStage);",
        "rememberReconciliationAuth(auth, id, requestDsmAuth);",
        "const result = await awaitQueuedResult(",
    ):
        if marker not in post_source:
            raise ValidationError(
                f"native DSM POST exact-request recovery is missing {marker!r}"
            )
    if (
        post_source.count("fetch(API_URL, {") != 1
        or post_source.count("apiPost(") != 1
        or post_source.count("const id = requestId();") != 1
        or post_source.count("body: request") != 1
        or post_source.count("requestAttempt.abort") != 2
    ):
        raise ValidationError(
            "native DSM POST must have exactly one package dispatch implementation and one bounded exact-request replay identity"
        )
    if re.search(
        r"(?:fetch\s*\(|new\s+(?:WebSocket|EventSource)\s*\()\s*['\"](?:https?:)?//",
        source,
        re.IGNORECASE,
    ):
        raise ValidationError("native DSM API source contains an external network endpoint")
    for forbidden in (
        "document.documentElement", "window.location.hash", "location.hash",
        "hashchange", "eval(", "new Function(", ".innerHTML", "insertAdjacentHTML",
    ):
        if forbidden in source:
            raise ValidationError(f"native DSM API source contains forbidden construct {forbidden}")


def validate_native_build_contract(
    main_payload: bytes,
    app_payload: bytes,
    api_payload: bytes,
    css_payload: bytes,
    webpack_payload: bytes,
    config_define_payload: bytes,
    package_payload: bytes,
    runtime_styles_payload: bytes,
    control_layout_payload: bytes,
    action_icon_payload: bytes | None = None,
    security_panel_payload: bytes | None = None,
) -> None:
    main = main_payload.decode("utf-8")
    app = app_payload.decode("utf-8")
    css = css_payload.decode("utf-8")
    webpack = webpack_payload.decode("utf-8")
    runtime_styles = runtime_styles_payload.decode("utf-8")
    control_layout = control_layout_payload.decode("utf-8")

    for marker in (
        'import Vue from "vue";',
        'import "./styles/native.css";',
        'import runtimeCss from "./styles/native.css?runtime";',
        'import { installRuntimeStyles } from "./runtimeStyles";',
        "installRuntimeStyles(runtimeCss);",
        f'SYNO.namespace("{APP_NAMESPACE}");',
        f"{APP_ID} = Vue.extend({{",
        "components: { App }",
        'template: "<App/>"',
    ):
        if marker not in main:
            raise ValidationError(f"native DSM entry module is missing {marker!r}")
    if main.count("Vue.extend(") != 1 or main.count(f"{APP_ID} =") != 1:
        raise ValidationError("native DSM entry module must define exactly one reviewed Vue class")

    for marker in (
        'const RUNTIME_STYLE_ID = "sdsync-current-runtime-style";',
        "export function installRuntimeStyles(cssText, targetDocument = document)",
        "targetDocument.getElementById(RUNTIME_STYLE_ID)",
        'targetDocument.createElement("style")',
        'style.setAttribute("data-sdsync-runtime-style", "current")',
        "style.textContent !== cssText",
        "style.textContent = cssText",
    ):
        if marker not in runtime_styles:
            raise ValidationError(
                f"native DSM runtime stylesheet source is missing {marker!r}"
            )
    if runtime_styles.count("export function installRuntimeStyles(") != 1:
        raise ValidationError(
            "native DSM runtime stylesheet source must expose exactly one installer"
        )

    for marker in (
        "export function installControlLayout(root)",
        "new ResizeObserver(",
        "new MutationObserver(",
        'root.querySelectorAll(RESPONSIVE_FORM_SELECTOR)',
        "return () => {",
    ):
        if marker not in control_layout:
            raise ValidationError(
                f"native DSM control-layout source is missing {marker!r}"
            )
    for marker in (
        'import { installControlLayout } from "./controlLayout";',
        "this.controlLayoutCleanup = installControlLayout(this.$el);",
        "if (this.controlLayoutCleanup) this.controlLayoutCleanup();",
    ):
        if marker not in app:
            raise ValidationError(
                f"native DSM component is missing control-layout contract {marker!r}"
            )

    structural_markers = (
        f'<v-app-instance class-name="{APP_ID}">',
        "<v-app-window",
        'ref="appWindow"',
        "</v-app-window>",
        "</v-app-instance>",
    )
    try:
        positions = [app.index(marker) for marker in structural_markers]
    except ValueError as error:
        raise ValidationError("native DSM component is missing its AppWindow structure") from error
    if positions != sorted(positions):
        raise ValidationError("native DSM AppWindow components are not correctly nested")
    if app.count('title="Synology Drive Sync"') != 1 or ':title="windowTitle"' in app:
        raise ValidationError("native DSM AppWindow title must be the literal package display name")
    root_marker = '<div class="sdsync-app" :class="themeClass">'
    toasts_marker = '<div class="sdsync-toasts" aria-live="polite"'
    modal_marker = (
        '<div v-if="confirmation.visible" class="sdsync-modal-backdrop" '
        'role="presentation"'
    )
    if app.count(root_marker) != 1 or app.count(toasts_marker) != 1 or app.count(modal_marker) != 1:
        raise ValidationError("native DSM AppWindow must own exactly one root, toast host, and modal host")
    if not (
        app.index(root_marker)
        < app.index("<main class=\"sdsync-workspace\">")
        < app.index("</main>")
        < app.index(toasts_marker)
        < app.index(modal_marker)
        < app.index("</v-app-window>")
    ):
        raise ValidationError("native DSM toast and modal hosts must remain at the AppWindow root")
    for marker in ("<v-button", "<v-form", "<v-form-item", "<v-single-select"):
        if marker not in app:
            raise ValidationError(f"native DSM component is missing DSM Vue control {marker}")
    for marker in (
        "beforeDestroy()", "this.stopTimers();",
        'document.removeEventListener("visibilitychange", this.visibilityHandler)',
        'this.mediaQuery.removeEventListener("change", this.mediaHandler)',
        "window.clearTimeout(this.snapshotTimer)",
        "window.clearTimeout(this.logTimer)",
        "this.disposed = true;",
        "this.abortController.abort();",
        "this.toastTimers.forEach((timer) => window.clearTimeout(timer))",
        "this.removeConfirmationKeyHandler();",
        "purgeReconciliationAuth(this.auth);",
        "this.confirmationPriorFocus = null;",
        "this.clearSecrets();", 'this.csrfToken = "";',
    ):
        if marker not in app:
            raise ValidationError(f"native DSM component is missing destruction cleanup {marker!r}")
    if "window.setInterval" in app or "setInterval(" in app:
        raise ValidationError("native DSM component must not retain unmanaged interval timers")

    for marker in (
        ':aria-label="item.title"',
        ':title="item.title"',
        'type="time" aria-label="Window starts"',
        'type="time" aria-label="Window ends"',
        'multiple size="4" aria-label="Wait for routines"',
        'role="dialog" aria-modal="true"',
        'aria-labelledby="sdsync-confirm-title"',
        'aria-describedby="sdsync-confirm-message"',
        'this.confirmationPriorFocus = document.activeElement;',
        'document.addEventListener("keydown", this.confirmationKeyHandler, true);',
        'document.removeEventListener("keydown", this.confirmationKeyHandler, true);',
        'event.key === "Escape"',
        'event.key !== "Tab"',
        "priorFocus && priorFocus.isConnected && priorFocus.focus",
        'if (this.route === "profiles" && route !== "profiles") {',
        'if (this.profileSaveState === "saving" || this.profileConnectionState === "testing") {',
        'this.toast("Profile operation in progress",',
        'closeProfile(options = undefined) {',
        'this.hasCapability("request_reconciliation")',
        'reconcileMutationRequest(',
        'Reconcile profile request',
        'Reconcile connection request',
        '{ id: "about", title: "About", icon: "about" }',
        '<action-icon :name="item.icon" :size="18" />',
        'import { ActionIcon } from "./ActionIcon";',
        "components: { ActionIcon, ControlHelp, SecurityPanel }",
        'about: "about.html"',
        'this.snapshot && this.snapshot.package && this.snapshot.package.version',
        'https://github.com/supermarsx/synology-drive-sync/releases',
        'https://supermarsx.github.io/synology-drive-sync/release-selector.html',
        'target="_blank" rel="noopener noreferrer"',
    ):
        if marker not in app:
            raise ValidationError(
                f"native DSM component is missing AppWindow interaction contract {marker!r}"
            )

    protected_close_binding = (
        ':disabled="profileSaveState === \'saving\' || profileConnectionState === \'testing\' '
        '|| profileReconciliationState === \'checking\' || profileOutcomeUnresolved '
        '|| connectionOutcomeUnresolved"'
    )
    if app.count(protected_close_binding) != 2:
        raise ValidationError(
            "native DSM profile Close and Cancel must both preserve unresolved drafts"
        )
    close_start = app.find("closeProfile(options = undefined) {")
    close_end = app.find("\n    clearSecrets()", close_start)
    close_source = app[close_start:close_end] if close_start >= 0 and close_end >= 0 else ""
    close_guard = close_source.find("|| this.profileOutcomeUnresolved")
    connection_guard = close_source.find("|| this.connectionOutcomeUnresolved) return;", close_guard)
    secret_clear = close_source.find("this.clearSecrets();", connection_guard)
    if not (0 <= close_guard < connection_guard < secret_clear):
        raise ValidationError(
            "native DSM profile close must fail closed before clearing an unresolved draft"
        )

    for marker in (
        "canChangeProfiles() { return this.canMutate && !this.operationBusy && !this.profileOutcomeUnresolved && !this.connectionOutcomeUnresolved",
        "canManageSecrets() { return this.canMutate && !this.operationBusy && !this.profileOutcomeUnresolved && !this.connectionOutcomeUnresolved",
    ):
        if marker not in app:
            raise ValidationError(
                "native DSM unresolved recovery must freeze profile and protected-secret inputs"
            )
    visibility_start = app.find("this.visibilityHandler = () => {")
    visibility_end = app.find(
        'document.addEventListener("visibilitychange", this.visibilityHandler);',
        visibility_start,
    )
    visibility_source = (
        app[visibility_start:visibility_end]
        if visibility_start >= 0 and visibility_end >= 0
        else ""
    )
    for marker in (
        "const protectedProfileDraft = this.profileEditorOpen === true",
        'this.profileSaveState === "saving"',
        'this.profileConnectionState === "testing"',
        'this.profileReconciliationState === "checking"',
        "|| this.profileOutcomeUnresolved",
        "|| this.connectionOutcomeUnresolved);",
        "if (!protectedProfileDraft) this.clearSecrets();",
    ):
        if marker not in visibility_source:
            raise ValidationError(
                "native DSM visibility cleanup must preserve protected recovery drafts"
            )

    connection_test_start = app.find("async testProfileAuthentication(event) {")
    connection_test_end = app.find("\n    openLocalSourceBrowser(", connection_test_start)
    connection_test_source = (
        app[connection_test_start:connection_test_end]
        if connection_test_start >= 0 and connection_test_end >= 0
        else ""
    )
    remote_open_start = app.find("openRemotePathBrowser(event) {")
    remote_open_end = app.find("\n    showPathBrowser(", remote_open_start)
    remote_open_source = (
        app[remote_open_start:remote_open_end]
        if remote_open_start >= 0 and remote_open_end >= 0
        else ""
    )
    if (
        'isolatedIncidentUnresolved(this, "connection")' not in connection_test_source
        or "apiPost(" not in connection_test_source
        or connection_test_source.find('isolatedIncidentUnresolved(this, "connection")')
        > connection_test_source.find("apiPost(")
        or 'isolatedIncidentUnresolved(this, "connection")' not in remote_open_source
        or "showPathBrowser(" not in remote_open_source
        or remote_open_source.find('isolatedIncidentUnresolved(this, "connection")')
        > remote_open_source.find("showPathBrowser(")
        or "!this.connectionOutcomeUnresolved" not in app
    ):
        raise ValidationError(
            "native DSM connection recovery must block overlapping authentication and browse requests"
        )

    reconcile_start = app.find("async reconcileProfileIncident(event) {")
    reconcile_end = app.find("\n    hasCapability(name)", reconcile_start)
    reconcile_source = app[reconcile_start:reconcile_end] if reconcile_start >= 0 and reconcile_end >= 0 else ""
    for marker, expected_count in (
        ("reconcileMutationRequest(", 2),
        ("recovered.request_id !== requestId", 2),
        ("recovered.operation !== operation", 2),
        ("recovered.job_id !== incident.jobId", 2),
        ("caught.requestId === requestId", 2),
        ("caught.operation === operation", 2),
        ("caught.jobId === incident.jobId", 2),
    ):
        if reconcile_source.count(marker) != expected_count:
            raise ValidationError(
                "native DSM reconciliation must correlate the exact request, job, and operation"
            )
    if "apiPost(" in reconcile_source:
        raise ValidationError("native DSM manual reconciliation must remain read-only")

    route_icons = {
        "overview", "profiles", "routines", "health", "activity",
        "notifications", "security", "settings", "about",
    }
    for icon in route_icons:
        marker = f'{{ id: "{icon}", title:'
        route_start = app.find(marker)
        if route_start < 0 or f'icon: "{icon}"' not in app[route_start:route_start + 120]:
            raise ValidationError(
                f"native DSM route {icon!r} must use its canonical shared ActionIcon name"
            )

    if (action_icon_payload is None) != (security_panel_payload is None):
        raise ValidationError("shared ActionIcon validation requires both component sources")
    if action_icon_payload is not None and security_panel_payload is not None:
        action_icon = action_icon_payload.decode("utf-8")
        security_panel = security_panel_payload.decode("utf-8")
        for icon in route_icons | {"help", "save", "chevron-down"}:
            icon_property = rf'^\s*(?:"{re.escape(icon)}"|{re.escape(icon)}):\s*\['
            if not re.search(icon_property, action_icon, re.MULTILINE):
                raise ValidationError(f"shared ActionIcon source is missing canonical icon {icon!r}")
        for marker in (
            "export const ActionIcon = {",
            'name: "ActionIcon"',
            'class: "sdsync-action-icon"',
            '"aria-hidden": "true"',
        ):
            if marker not in action_icon:
                raise ValidationError(f"shared ActionIcon source is missing contract {marker!r}")
        for marker in (
            'import { ActionIcon } from "./ActionIcon";',
            "components: { ActionIcon, PolicyHelp }",
            '<action-icon name="help" :size="14" />',
            '<action-icon name="save" />',
            '<action-icon name="chevron-down" />',
        ):
            if marker not in security_panel:
                raise ValidationError(
                    f"native DSM security panel is missing shared ActionIcon contract {marker!r}"
                )

    operation_guards = (
        r"openProfile\(name\)\s*\{\s*if \(this\.operationBusy\) return;",
        r'async saveProfile\(event\)\s*\{.{0,600}?if \(!this\.canChangeProfiles \|\| this\.operationBusy \|\| this\.profileSaveState === "saving"\) return',
        r"async saveProfileSecrets\(event\)\s*\{.{0,500}?if \(!profile \|\| !this\.canManageSecrets \|\| this\.operationBusy\) return;",
        r"async removeProfile\(\)\s*\{.{0,400}?if \(!this\.canChangeProfiles \|\| !this\.selectedProfile \|\| this\.operationBusy\) return;",
        r'openRoutine\(profile = ""\)\s*\{\s*if \(this\.operationBusy \|\| \(!profile && !this\.canChangeRoutines\)\) return;',
        r"async saveRoutine\(event\)\s*\{.{0,700}?this\.operationBusy\) return;",
        r"async removeRoutine\(\)\s*\{.{0,700}?this\.operationBusy\) return;",
        r"async saveAlerts\(event\)\s*\{.{0,400}?this\.operationBusy\) return;",
        r"async saveSecurityPolicy\(event\)\s*\{.{0,400}?if \(!this\.canMutate \|\| !this\.securityDirty \|\| this\.operationBusy\) return;",
        r"async executeOperation\(kind, payload\)\s*\{.{0,400}?if \(!this\.canRunOperations \|\| this\.operationBusy \|\| this\.disposed\) return;",
        r"async quickRun\(\)\s*\{\s*if \(!this\.canRunOperations \|\| this\.operationBusy\) return;",
        r"async runDoctor\(event\)\s*\{.{0,300}?if \(!this\.canRunOperations \|\| this\.operationBusy\) return;",
    )
    for guard in operation_guards:
        if not re.search(guard, app, re.DOTALL):
            raise ValidationError("native DSM mutation surface is missing a global operationBusy guard")
    mutation_scope_guards = {
        "saveSecurityPolicy": 'scopeMutationOutcomeUnresolved\\(this, "security"\\)',
        "testProfileAuthentication": 'scopeMutationOutcomeUnresolved\\(this, "profile"\\)',
        "openRemotePathBrowser": 'scopeMutationOutcomeUnresolved\\(this, "profile"\\)',
        "saveProfile": 'scopeMutationOutcomeUnresolved\\(this, "profile"\\)',
        "saveProfileSecrets": 'scopeMutationOutcomeUnresolved\\(this, "profile"\\)',
        "removeProfile": 'scopeMutationOutcomeUnresolved\\(this, "profile"\\)',
        "saveRoutine": 'scopeMutationOutcomeUnresolved\\(this, "routine"\\)',
        "removeRoutine": 'scopeMutationOutcomeUnresolved\\(this, "routine"\\)',
        "saveAlerts": 'scopeMutationOutcomeUnresolved\\(this, "alerts"\\)',
        "executeOperation": 'isolatedIncidentUnresolved\\(this, "operations"\\)',
        "saveNotificationPreferences": 'scopeMutationOutcomeUnresolved\\(this, "interface"\\)',
        "saveInterfaceSettings": 'scopeMutationOutcomeUnresolved\\(this, "interface"\\)',
    }
    for method, scoped_guard in mutation_scope_guards.items():
        guard = (
            rf"(?:async\s+)?{re.escape(method)}\([^)]*\)\s*\{{"
            rf".{{0,420}}?{scoped_guard}"
        )
        if not re.search(guard, app, re.DOTALL):
            raise ValidationError(
                f"native DSM mutation surface method {method} is missing its scope-isolated mutation guard"
            )
    for marker in (
        'const awaitTerminal = kind === "doctor";',
        "Object.assign({ kind }, payload),\n          awaitTerminal",
        'quickPlan() { return this.executeOperation("plan"',
        'return this.executeOperation("run"',
        'return this.executeOperation("doctor"',
    ):
        if marker not in app:
            raise ValidationError(
                "native DSM Doctor must terminal-poll while plan and run return queued"
            )
    for forbidden in (
        "<iframe", "createElement(\"iframe\")", "createElement('iframe')",
        "<object", "<embed", "index.html", "document.documentElement",
        "window.location", "window.history", "history.replaceState", "location.hash", "hashchange", "v-html",
        ".innerHTML", "insertAdjacentHTML", "document.write(", "eval(", "new Function(",
        "consumeLaunchToken", "X-SYNO-TOKEN", "SynoToken", "launch token",
    ):
        if forbidden.lower() in app.lower():
            raise ValidationError(f"native DSM component contains forbidden launcher or DOM construct {forbidden}")
    validate_external_links(app, "native DSM component")
    for forbidden in (
        "Your sync estate, at a glance.", "sdsync-hero", "sdsync-check-grid",
        "sdsync-editor-placeholder", "Select a profile or create one",
        "Fixed, non-secret messages", "sdsync-section-heading",
    ):
        if forbidden in app:
            raise ValidationError("native DSM component contains marketing or Help-only filler")

    if ".sdsync-app" not in css or ".sdsync-app.is-light" not in css:
        raise ValidationError("native DSM styles must scope dark/light themes to the AppWindow root")
    if "@media (prefers-reduced-motion:" not in css or ":focus-visible" not in css:
        raise ValidationError(
            "native DSM styles must provide reduced-motion and keyboard-focus treatment"
        )
    if re.search(r"(^|[},])\s*(?::root|html\b|body\b)", css, re.IGNORECASE):
        raise ValidationError("native DSM styles must not mutate DSM document-level selectors")
    for line in css.splitlines():
        selector = line.strip()
        if selector.endswith("{") and not selector.startswith("@") and not selector.startswith(".sdsync-"):
            raise ValidationError(f"native DSM stylesheet has an unscoped selector {selector!r}")
    if re.search(r"url\(\s*['\"]?(?:https?:)?//", css, re.IGNORECASE):
        raise ValidationError("native DSM stylesheet loads a remote asset")
    if "sourceMappingURL" in css:
        raise ValidationError("native DSM stylesheet must not reference a source map")

    if not re.search(r"externals\s*:\s*\{\s*vue\s*:\s*['\"]Vue['\"]\s*\}", webpack):
        raise ValidationError("native DSM webpack build must externalize Vue to the DSM global")
    for marker in (
        'filename: "SynologyDriveSync.js"',
        'new MiniCssExtractPlugin({ filename: "style.css" })',
        "splitChunks: false", "runtimeChunk: false", "devtool: false",
    ):
        if marker not in webpack:
            raise ValidationError(f"native DSM webpack contract is missing {marker!r}")

    config_define = load_unique_json(config_define_payload, "ui-src/config.define")
    if config_define != {
        UI_SOURCE_MODULE: {
            "JSfiles": [f"dist/{UI_SOURCE_MODULE}"],
            "params": "-s -c skip",
        }
    }:
        raise ValidationError("ui-src/config.define does not bind the reviewed native module")
    package = load_unique_json(package_payload, "ui-src/package.json")
    if not isinstance(package, dict):
        raise ValidationError("ui-src/package.json must be an object")
    dependencies = package.get("dependencies", {})
    dev_dependencies = package.get("devDependencies")
    if dependencies not in ({}, None) or not isinstance(dev_dependencies, dict):
        raise ValidationError("native DSM UI may only use pinned build-time dependencies")
    if dev_dependencies.get("vue") != "2.7.14" or any(
        not isinstance(version, str) or not re.fullmatch(r"\d+\.\d+\.\d+", version)
        for version in dev_dependencies.values()
    ):
        raise ValidationError("native DSM UI build dependencies must be exact and use DSM Vue 2.7.14")

    validate_native_api_source(api_payload)


def _javascript_string_literals(source: str):
    """Yield decoded ordinary JavaScript string literals outside comments/templates."""
    simple_escapes = {
        "b": "\b",
        "f": "\f",
        "n": "\n",
        "r": "\r",
        "t": "\t",
        "v": "\v",
        "0": "\0",
    }
    index = 0
    length = len(source)
    while index < length:
        current = source[index]
        following = source[index + 1] if index + 1 < length else ""
        if current == "/" and following == "/":
            newline = source.find("\n", index + 2)
            index = length if newline < 0 else newline + 1
            continue
        if current == "/" and following == "*":
            end = source.find("*/", index + 2)
            index = length if end < 0 else end + 2
            continue
        if current == "`":
            index += 1
            while index < length:
                if source[index] == "\\":
                    index += 2
                elif source[index] == "`":
                    index += 1
                    break
                else:
                    index += 1
            continue
        if current not in ("'", '"'):
            index += 1
            continue

        quote = current
        index += 1
        decoded: list[str] = []
        valid = True
        while index < length:
            current = source[index]
            if current == quote:
                index += 1
                if valid:
                    yield "".join(decoded)
                break
            if current in "\r\n":
                valid = False
                index += 1
                break
            if current != "\\":
                decoded.append(current)
                index += 1
                continue

            index += 1
            if index >= length:
                valid = False
                break
            escaped = source[index]
            index += 1
            if escaped in simple_escapes:
                decoded.append(simple_escapes[escaped])
            elif escaped == "x":
                digits = source[index:index + 2]
                if len(digits) != 2 or re.fullmatch(r"[0-9a-fA-F]{2}", digits) is None:
                    valid = False
                    break
                decoded.append(chr(int(digits, 16)))
                index += 2
            elif escaped == "u":
                if index < length and source[index] == "{":
                    end = source.find("}", index + 1)
                    digits = source[index + 1:end] if end >= 0 else ""
                    if (
                        not digits
                        or len(digits) > 6
                        or re.fullmatch(r"[0-9a-fA-F]+", digits) is None
                        or int(digits, 16) > 0x10FFFF
                    ):
                        valid = False
                        break
                    decoded.append(chr(int(digits, 16)))
                    index = end + 1
                else:
                    digits = source[index:index + 4]
                    if len(digits) != 4 or re.fullmatch(r"[0-9a-fA-F]{4}", digits) is None:
                        valid = False
                        break
                    decoded.append(chr(int(digits, 16)))
                    index += 4
            elif escaped == "\r":
                if index < length and source[index] == "\n":
                    index += 1
            elif escaped == "\n":
                pass
            else:
                decoded.append(escaped)
        else:
            break


def _validate_runtime_style_bundle(script: str, style: str) -> None:
    for marker in (
        "sdsync-current-runtime-style",
        "data-sdsync-runtime-style",
        "text/css",
        "getElementById",
        "createElement",
        "appendChild",
        "textContent",
    ):
        if marker not in script:
            raise ValidationError(
                f"native DSM bundle is missing runtime stylesheet injection marker {marker!r}"
            )
    if not any(literal == style for literal in _javascript_string_literals(script)):
        raise ValidationError(
            "native DSM bundle does not embed the exact packaged stylesheet bytes"
        )


def validate_native_bundle(script_payload: bytes, style_payload: bytes) -> None:
    script = script_payload.decode("utf-8")
    style = style_payload.decode("utf-8")
    for marker in (
        APP_ID, "v-app-instance", "v-app-window", CANONICAL_API,
        DSM_TOKEN_API, "X-SDSYNC-Request", "X-SDSYNC-CSRF", "X-SYNO-TOKEN",
        "sdsync.dsm-request-status.v1", "request-status", "request_reconciliation",
        "Reconcile profile request", "Reconcile connection request",
        "DSM returned an invalid reconciled connection result. The preserved request remains locked.",
        "Test the current draft once more to unlock File Station browsing.",
        "same-origin", "beforeDestroy",
        "https://github.com/supermarsx/synology-drive-sync/releases",
        "https://supermarsx.github.io/synology-drive-sync/release-selector.html",
        "noopener noreferrer",
    ):
        if marker not in script:
            raise ValidationError(f"native DSM bundle is missing reviewed contract {marker!r}")
    if not re.search(r"\b(?:const|let|var)\s+[A-Za-z_$][\w$]*\s*=\s*Vue\b", script):
        raise ValidationError("native DSM bundle must consume Vue from the DSM global")
    for forbidden in (
        "<iframe", "createElement(\"iframe\")", "createElement('iframe')", "index.html",
        "document.documentElement", "window.location", "window.history", "history.replaceState",
        "location.hash", "hashchange",
        "sourceMappingURL", "eval(", "new Function(", "__VUE_DEVTOOLS_GLOBAL_HOOK__",
        "You are running Vue in development mode", 'version:"2.7.14"',
        "consumeLaunchToken", "launch token",
    ):
        if forbidden.lower() in script.lower():
            raise ValidationError(f"native DSM bundle contains forbidden runtime {forbidden}")
    if re.search(
        r"(?:fetch\s*\(|new\s+(?:WebSocket|EventSource)\s*\()\s*['\"](?:https?:)?//",
        script,
        re.IGNORECASE,
    ):
        raise ValidationError("native DSM bundle contains an external network endpoint")
    if "eval(" in style or "sourceMappingURL" in style:
        raise ValidationError("native DSM style bundle contains executable or source-map content")
    if re.search(r"url\(\s*['\"]?(?:https?:)?//", style, re.IGNORECASE):
        raise ValidationError("native DSM style bundle loads a remote asset")
    if (
        ".sdsync-app" not in style
        or "@media (prefers-reduced-motion:" not in style
        or ":focus-visible" not in style
        or re.search(
            r"(^|[},])\s*(?::root|html\b|body\b)", style, re.IGNORECASE
        )
    ):
        raise ValidationError("native DSM style bundle is not isolated to the AppWindow")
    _validate_runtime_style_bundle(script, style)


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

    expected = {"defaults": {"run-as": "package"}}
    if model != expected:
        raise ValidationError(
            "conf/privilege must use the reviewed package-identity contract"
        )


def validate_source() -> None:
    required_files = [
        HERE / "INFO.template",
        HERE / "conf/privilege",
        HERE / "licenses/musl-COPYRIGHT",
        HERE / "licenses/DSM_UI_THIRD_PARTY_LICENSES.txt",
        HERE / "build-spk.sh",
        HERE / "build_spk.py",
        HERE / "package/bin/sdsync-dsm",
        HERE / "package/libexec/sdsync-common",
        HERE / "package/libexec/sdsync-controller",
        HERE / "package/libexec/sdsync-run",
        HERE / "package/ui/images/icon.svg",
        HERE / "package/ui/helptoc.conf",
        HERE / "package/ui/texts/enu/strings",
        UI_SOURCE / "app.config",
        UI_SOURCE / "config.define",
        UI_SOURCE / "Makefile",
        UI_SOURCE / "package.json",
        UI_SOURCE / "pnpm-lock.yaml",
        UI_SOURCE / "webpack.config.js",
        UI_SOURCE / "src/main.js",
        UI_SOURCE / "src/App.vue",
        UI_SOURCE / "src/ActionIcon.js",
        UI_SOURCE / "src/autosave.js",
        UI_SOURCE / "src/SecurityPanel.vue",
        UI_SOURCE / "src/api.js",
        UI_SOURCE / "src/runtimeStyles.js",
        UI_SOURCE / "src/controlLayout.js",
        UI_SOURCE / "src/styles/native.css",
        UI_SOURCE / "dist/SynologyDriveSync.js",
        UI_SOURCE / "dist/style.css",
    ] + [HERE / "scripts" / name for name in REQUIRED_SCRIPTS] + [
        HERE / f"package/ui/help/enu/{page}.html" for page in UI_HELP_PAGES
    ]
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
    for path in (
        HERE / "package/ui/config",
        HERE / "package/ui/app.js",
        HERE / "package/ui/app.css",
    ):
        if path.exists() or path.is_symlink():
            raise ValidationError(
                "native DSM source must not contain a legacy standalone launcher: "
                f"{path.relative_to(HERE)}"
            )
    validate_privilege((HERE / "conf/privilege").read_bytes())
    validate_source_app_config((UI_SOURCE / "app.config").read_bytes())
    dist_members = {
        path.name
        for path in (UI_SOURCE / "dist").iterdir()
    }
    if dist_members != {UI_SOURCE_MODULE, "style.css"}:
        raise ValidationError(
            "native DSM dist must contain only SynologyDriveSync.js and style.css"
        )
    validate_native_build_contract(
        (UI_SOURCE / "src/main.js").read_bytes(),
        (UI_SOURCE / "src/App.vue").read_bytes(),
        (UI_SOURCE / "src/api.js").read_bytes(),
        (UI_SOURCE / "src/styles/native.css").read_bytes(),
        (UI_SOURCE / "webpack.config.js").read_bytes(),
        (UI_SOURCE / "config.define").read_bytes(),
        (UI_SOURCE / "package.json").read_bytes(),
        (UI_SOURCE / "src/runtimeStyles.js").read_bytes(),
        (UI_SOURCE / "src/controlLayout.js").read_bytes(),
        (UI_SOURCE / "src/ActionIcon.js").read_bytes(),
        (UI_SOURCE / "src/SecurityPanel.vue").read_bytes(),
    )
    validate_native_bundle(
        (UI_SOURCE / "dist/SynologyDriveSync.js").read_bytes(),
        (UI_SOURCE / "dist/style.css").read_bytes(),
    )
    validate_ui_texts((HERE / "package/ui/texts/enu/strings").read_bytes())
    validate_dsm_help(
        (HERE / "package/ui/helptoc.conf").read_bytes(),
        {
            page: (HERE / f"package/ui/help/enu/{page}.html").read_bytes()
            for page in UI_HELP_PAGES
        },
    )
    validate_notifier((HERE / "package/libexec/sdsync-common").read_bytes())
    validate_svg_icon((HERE / "package/ui/images/icon.svg").read_bytes())
    template = (HERE / "INFO.template").read_text(encoding="utf-8")
    for token in (
        "@DSM_VERSION@",
        "@ARCH@",
        "@EXTRACT_SIZE_KIB@",
        "@PACKAGE_TGZ_MD5@",
    ):
        if template.count(token) != 1:
            raise ValidationError(f"INFO.template must contain {token} exactly once")
    template_info = parse_info(template.encode("utf-8"))
    expected_template = {
        **FIXED_INFO,
        "version": "@DSM_VERSION@",
        "arch": "@ARCH@",
        "extractsize": "@EXTRACT_SIZE_KIB@",
        "checksum": "@PACKAGE_TGZ_MD5@",
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
            "LICENSES/THIRD_PARTY_LICENSES.html",
            "LICENSES/DSM_UI_THIRD_PARTY_LICENSES.txt", "conf/privilege",
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
        expected_ui_notice = (
            HERE / "licenses/DSM_UI_THIRD_PARTY_LICENSES.txt"
        ).read_bytes()
        if member_bytes(
            outer, members["LICENSES/DSM_UI_THIRD_PARTY_LICENSES.txt"]
        ) != expected_ui_notice:
            raise ValidationError(
                f"{path.name} outer DSM UI third-party notices do not match the reviewed source"
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
        declared_checksum = info["checksum"]
        if not re.fullmatch(r"[0-9a-f]{32}", declared_checksum):
            raise ValidationError(
                "INFO checksum must be exactly 32 lowercase hexadecimal MD5 characters"
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
        native_modules = {
            name
            for name in inner_members
            if PurePosixPath(name).parent == PurePosixPath("ui")
            and NATIVE_MODULE_PATTERN.fullmatch(PurePosixPath(name).name) is not None
        }
        if len(native_modules) != 1:
            raise ValidationError(
                "package.tgz must contain exactly one content-addressed DSM AppWindow module"
            )
        allowed_inner = REQUIRED_PAYLOAD | {
            "bin",
            "libexec",
            "share",
            "share/licenses",
            "ui",
            "ui/help",
            "ui/help/enu",
            "ui/images",
            "ui/texts",
            "ui/texts/enu",
        } | native_modules
        unexpected = inner_members.keys() - allowed_inner
        if unexpected:
            raise ValidationError(
                f"package.tgz contains unexpected inner members: {sorted(unexpected)}"
            )
        if member_bytes(
            inner,
            inner_members["share/licenses/DSM_UI_THIRD_PARTY_LICENSES.txt"],
        ) != expected_ui_notice:
            raise ValidationError(
                "package.tgz DSM UI third-party notices do not match the reviewed source"
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
        ui_module = validate_ui_config(member_bytes(inner, inner_members["ui/config"]))
        if ui_module not in inner_members:
            raise ValidationError(
                "ui/config native module does not resolve inside package.tgz"
            )
        require_regular_mode(
            inner_members, ui_module, 0o644, "DSM native AppWindow module"
        )
        ui_bundle = member_bytes(inner, inner_members[ui_module])
        expected_ui_module = f"ui/{native_ui_module_name(ui_bundle)}"
        if ui_module != expected_ui_module:
            raise ValidationError(
                "ui/config native module digest does not match the exact packaged bundle bytes"
            )
        require_regular_mode(inner_members, NATIVE_STYLE, 0o644, "DSM native AppWindow style")
        validate_ui_texts(member_bytes(inner, inner_members["ui/texts/enu/strings"]))
        validate_dsm_help(
            member_bytes(inner, inner_members["ui/helptoc.conf"]),
            {
                page: member_bytes(
                    inner, inner_members[f"ui/help/enu/{page}.html"]
                )
                for page in UI_HELP_PAGES
            },
        )
        validate_notifier(member_bytes(inner, inner_members["libexec/sdsync-common"]))
        validate_svg_icon(member_bytes(inner, inner_members["ui/images/icon.svg"]))
        validate_native_bundle(
            ui_bundle,
            member_bytes(inner, inner_members[NATIVE_STYLE]),
        )
        for size in UI_ICON_SIZES:
            icon_name = f"ui/images/icon_{size}.png"
            icon_payload = member_bytes(inner, inner_members[icon_name])
            if png_dimensions(icon_payload) != (size, size):
                raise ValidationError(f"{icon_name} must be {size}x{size}")
            if icon_payload != png_icon(size):
                raise ValidationError(f"{icon_name} does not match the deterministic source mark")
    actual_checksum = hashlib.md5(payload, usedforsecurity=False).hexdigest()
    if declared_checksum != actual_checksum:
        raise ValidationError(
            f"INFO checksum {declared_checksum} does not match exact package.tgz MD5 "
            f"{actual_checksum}"
        )
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
