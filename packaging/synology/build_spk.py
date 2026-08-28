#!/usr/bin/env python3
"""Build a deterministic DSM 7 SPK from static ELFs and a native UI bundle."""

from __future__ import annotations

import argparse
import binascii
import functools
import gzip
import hashlib
import io
import json
import math
import os
import re
import struct
import tarfile
import zlib
from dataclasses import dataclass
from pathlib import Path


HERE = Path(__file__).resolve().parent
REPOSITORY = HERE.parents[1]
PACKAGE = "synology-drive-sync"
DSM_APP_CLASS = "SYNO.SDS.App.SynologyDriveSync.Instance"
UI_SOURCE = HERE / "ui-src"
UI_ICON_SIZES = (16, 24, 32, 48, 64, 72, 256)
UI_HELP_PAGES = (
    "overview",
    "profiles",
    "routines",
    "health",
    "activity",
    "notifications",
    "security",
    "settings",
    "about",
)
ICON_ARROW_HALF_THICKNESS = 0.032
ICON_ARROW_HALF_WIDTH = 0.075
ICON_TOP_BODY = (
    (0.245, 0.455),
    (0.275, 0.350),
    (0.360, 0.270),
    (0.500, 0.240),
    (0.640, 0.270),
    (0.710, 0.350),
)
ICON_TOP_TIP = (0.790, 0.445)
ICON_BOTTOM_BODY = tuple((1.0 - x, 1.0 - y) for x, y in ICON_TOP_BODY)
ICON_BOTTOM_TIP = (1.0 - ICON_TOP_TIP[0], 1.0 - ICON_TOP_TIP[1])
ICON_BORDER_THICKNESS = 2.0 / 256.0
ICON_ORBIT_HALF_THICKNESS = 0.005
ICON_ORBIT_POINTS = (
    (0.285, 0.155),
    (0.715, 0.155),
    (0.845, 0.285),
    (0.845, 0.715),
    (0.715, 0.845),
    (0.285, 0.845),
    (0.155, 0.715),
    (0.155, 0.285),
    (0.285, 0.155),
)
ICON_CENTER_RADIUS = 0.055
ICON_BACKGROUND = (11.0, 7.0, 6.0)
ICON_BORDER = (74.0, 27.0, 16.0)
ICON_ORBIT = (107.0, 39.0, 24.0)
ICON_TOP = (255.0, 106.0, 26.0)
ICON_BOTTOM = (215.0, 46.0, 22.0)
ICON_CENTER = (255.0, 210.0, 163.0)


@dataclass(frozen=True)
class ArchitectureContract:
    """Binary and DSM metadata contract for one release artifact."""

    elf_class: int
    machine: int
    info_arches: tuple[str, ...]
    rust_target: str
    arm_eabi: int | None = None
    hard_float: bool = False

    @property
    def info_value(self) -> str:
        return " ".join(self.info_arches)


# Synology's current package toolkit unifies alpine/alpine4k as ``armv7`` but
# deliberately leaves the other compatible ARMv7 platforms as exact platform
# values. One hard-float userspace binary can serve them because this package
# has no platform-specific kernel module. Do not replace these values with
# ``armv7l``: that is Linux's uname machine name, not a DSM INFO arch token.
ARCHITECTURES = {
    "x86_64": ArchitectureContract(
        elf_class=2,
        machine=62,
        info_arches=("x86_64",),
        rust_target="x86_64-unknown-linux-musl",
    ),
    "i686": ArchitectureContract(
        elf_class=1,
        machine=3,
        info_arches=("i686",),
        rust_target="i686-unknown-linux-musl",
    ),
    "armv7": ArchitectureContract(
        elf_class=1,
        machine=40,
        info_arches=(
            "armv7",
            "armada370",
            "armada375",
            "armada38x",
            "armadaxp",
            "comcerto2k",
            "monaco",
        ),
        rust_target="armv7-unknown-linux-musleabihf",
        arm_eabi=5,
        hard_float=True,
    ),
    "armv8": ArchitectureContract(
        elf_class=2,
        machine=183,
        info_arches=("armv8",),
        rust_target="aarch64-unknown-linux-musl",
    ),
}
# Kept as a small compatibility surface for scripts importing the old mapping.
ARCH_MACHINES = {name: contract.machine for name, contract in ARCHITECTURES.items()}
VERSION_PATTERN = re.compile(r"[0-9]+(?:[._-][0-9]+)*\Z")
SOURCE_DATE_EPOCH = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))

ELF_CLASS_NAMES = {1: "ELF32", 2: "ELF64"}
ELF_DATA_NAMES = {1: "little-endian", 2: "big-endian"}
EF_ARM_EABIMASK = 0xFF000000
EF_ARM_ABI_FLOAT_SOFT = 0x00000200
EF_ARM_ABI_FLOAT_HARD = 0x00000400


class PackageError(ValueError):
    """A deterministic package-input validation failure."""


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Build a DSM 7 SPK around prebuilt static Linux binaries and the "
            "prebuilt native DSM AppWindow bundle."
        )
    )
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument(
        "--api-binary",
        required=True,
        type=Path,
        help="prebuilt static sdsync-dsm-api ELF for the controller and CGI bridge",
    )
    parser.add_argument("--arch", required=True, choices=sorted(ARCHITECTURES))
    parser.add_argument("--version", required=True)
    parser.add_argument("--output", required=True, type=Path)
    return parser.parse_args()


def normalized_versions(raw: str) -> tuple[str, str]:
    release = raw[1:] if raw.startswith("v") else raw
    if not VERSION_PATTERN.fullmatch(release):
        raise PackageError(
            "version must contain only numeric components separated by '.', '-' or '_'"
        )
    # DSM recommends a separate monotonically increasing build component. Release
    # tags in this project are semantic versions, so give each feature version build 1.
    dsm = release if "-" in release else f"{release}-1"
    return release, dsm


def elf_data_contract(data: bytes, arch: str) -> None:
    contract = ARCHITECTURES[arch]
    if len(data) < 16 or data[:4] != b"\x7fELF":
        raise PackageError("binary is not an ELF executable")
    elf_class = data[4]
    if elf_class != contract.elf_class:
        expected_class = ELF_CLASS_NAMES[contract.elf_class]
        actual_class = ELF_CLASS_NAMES.get(elf_class, f"unknown class {elf_class}")
        raise PackageError(
            f"binary is {actual_class}, expected {expected_class} for DSM arch {arch}"
        )
    data_encoding = data[5]
    if data_encoding != 1:
        actual_encoding = ELF_DATA_NAMES.get(
            data_encoding, f"unknown data encoding {data_encoding}"
        )
        raise PackageError(
            f"binary is {actual_encoding}, expected little-endian for DSM arch {arch}"
        )
    if data[6] != 1:
        raise PackageError("binary has an unsupported ELF identification version")

    is_64_bit = elf_class == 2
    elf_header_size = 64 if is_64_bit else 52
    program_header_size = 56 if is_64_bit else 32
    dynamic_entry_size = 16 if is_64_bit else 8
    if len(data) < elf_header_size:
        raise PackageError(f"binary has a truncated {ELF_CLASS_NAMES[elf_class]} header")
    elf_type = struct.unpack_from("<H", data, 16)[0]
    if elf_type not in (2, 3):
        raise PackageError("binary must be an ELF ET_EXEC or ET_DYN executable")
    machine = struct.unpack_from("<H", data, 18)[0]
    expected = contract.machine
    if machine != expected:
        raise PackageError(
            f"binary ELF machine is {machine}, expected {expected} for DSM arch {arch}"
        )
    if struct.unpack_from("<I", data, 20)[0] != 1:
        raise PackageError("binary has an unsupported ELF header version")
    if is_64_bit:
        program_offset = struct.unpack_from("<Q", data, 32)[0]
        elf_flags = struct.unpack_from("<I", data, 48)[0]
        declared_header_size = struct.unpack_from("<H", data, 52)[0]
        declared_program_size = struct.unpack_from("<H", data, 54)[0]
        program_count = struct.unpack_from("<H", data, 56)[0]
    else:
        program_offset = struct.unpack_from("<I", data, 28)[0]
        elf_flags = struct.unpack_from("<I", data, 36)[0]
        declared_header_size = struct.unpack_from("<H", data, 40)[0]
        declared_program_size = struct.unpack_from("<H", data, 42)[0]
        program_count = struct.unpack_from("<H", data, 44)[0]
    if declared_header_size != elf_header_size:
        raise PackageError(
            f"binary has an invalid {ELF_CLASS_NAMES[elf_class]} header size"
        )
    if declared_program_size < program_header_size:
        raise PackageError(
            f"binary has an invalid {ELF_CLASS_NAMES[elf_class]} program-header size"
        )
    if program_count == 0:
        raise PackageError("binary has no ELF program headers")
    if program_offset + declared_program_size * program_count > len(data):
        raise PackageError("binary has truncated ELF program headers")

    if contract.arm_eabi is not None:
        eabi = (elf_flags & EF_ARM_EABIMASK) >> 24
        if eabi != contract.arm_eabi:
            raise PackageError(
                f"binary ARM EABI is {eabi}, expected EABI{contract.arm_eabi} "
                f"for DSM arch {arch}"
            )
        float_abi = elf_flags & (EF_ARM_ABI_FLOAT_SOFT | EF_ARM_ABI_FLOAT_HARD)
        expected_float = (
            EF_ARM_ABI_FLOAT_HARD if contract.hard_float else EF_ARM_ABI_FLOAT_SOFT
        )
        if float_abi != expected_float:
            expected_name = "hard-float" if contract.hard_float else "soft-float"
            raise PackageError(
                f"binary ARM float ABI flags are 0x{float_abi:x}, expected "
                f"EABI{contract.arm_eabi} {expected_name} for DSM arch {arch}"
            )

    executable_load = False
    for index in range(program_count):
        offset = program_offset + index * declared_program_size
        kind = struct.unpack_from("<I", data, offset)[0]
        if is_64_bit:
            flags = struct.unpack_from("<I", data, offset + 4)[0]
            file_offset = struct.unpack_from("<Q", data, offset + 8)[0]
            file_size = struct.unpack_from("<Q", data, offset + 32)[0]
        else:
            file_offset = struct.unpack_from("<I", data, offset + 4)[0]
            file_size = struct.unpack_from("<I", data, offset + 16)[0]
            flags = struct.unpack_from("<I", data, offset + 24)[0]
        if kind == 1:
            if file_offset + file_size > len(data):
                raise PackageError("binary has a truncated PT_LOAD segment")
            if flags & 1 and file_size > 0:
                executable_load = True
        if kind == 3:  # PT_INTERP
            raise PackageError(
                "binary has a dynamic program interpreter; build a static musl ELF"
            )
        if kind != 2:  # PT_DYNAMIC
            continue
        dynamic_offset = file_offset
        dynamic_size = file_size
        if dynamic_offset + dynamic_size > len(data):
            raise PackageError("binary has a truncated PT_DYNAMIC segment")
        for entry in range(
            dynamic_offset, dynamic_offset + dynamic_size, dynamic_entry_size
        ):
            if entry + dynamic_entry_size > len(data):
                raise PackageError("binary has a truncated dynamic entry")
            tag = struct.unpack_from("<q" if is_64_bit else "<i", data, entry)[0]
            if tag == 0:
                break
            if tag == 1:  # DT_NEEDED
                raise PackageError(
                    "binary declares a DT_NEEDED library; build a fully static musl ELF"
                )
    if not executable_load:
        raise PackageError("binary has no non-empty executable PT_LOAD segment")


def elf_contract(path: Path, arch: str) -> None:
    if path.is_symlink() or not path.is_file():
        raise PackageError(f"binary must be a non-symlink regular file: {path}")
    elf_data_contract(path.read_bytes(), arch)


def tar_info(name: str, mode: int, size: int = 0, directory: bool = False) -> tarfile.TarInfo:
    if mode & 0o6000:
        raise PackageError(f"archive members must not carry setuid/setgid bits: {name}")
    info = tarfile.TarInfo(name)
    info.mode = mode
    info.size = size
    info.mtime = SOURCE_DATE_EPOCH
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    if directory:
        info.type = tarfile.DIRTYPE
    return info


def add_bytes(archive: tarfile.TarFile, name: str, payload: bytes, mode: int) -> None:
    archive.addfile(tar_info(name, mode, len(payload)), io.BytesIO(payload))


def _json_object(path: Path) -> dict[str, object]:
    if path.is_symlink() or not path.is_file():
        raise PackageError(
            f"native DSM UI input must be a non-symlink regular file: {path}"
        )
    payload = path.read_bytes()
    if not payload:
        raise PackageError(f"native DSM UI input must not be empty: {path}")
    try:
        parsed = json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise PackageError(f"native DSM UI JSON is invalid: {path}: {error}") from error
    if not isinstance(parsed, dict):
        raise PackageError(f"native DSM UI JSON must contain an object: {path}")
    return parsed


def _regular_file_bytes(path: Path) -> bytes:
    if path.is_symlink() or not path.is_file():
        raise PackageError(
            "native DSM UI input must be a non-symlink regular file: "
            f"{path}; run the pinned pnpm build in {UI_SOURCE} first"
        )
    payload = path.read_bytes()
    if not payload:
        raise PackageError(f"native DSM UI input must not be empty: {path}")
    return payload


def _validate_external_links(document: str, label: str) -> None:
    """Allow inert HTTPS anchors while rejecting remote assets and opener access."""
    for tag in re.findall(r"<[^>]+>", document):
        remote_reference = re.search(
            r'\b(?:href|src)=["\'](?:https?:)?//', tag, re.IGNORECASE
        )
        if remote_reference is None:
            continue
        if not re.match(r"<a\b", tag, re.IGNORECASE):
            raise PackageError(f"{label} contains a remote asset")
        if not re.search(r'\bhref=["\']https://', tag, re.IGNORECASE):
            raise PackageError(f"{label} external links must use HTTPS")
        if not re.search(r'\btarget=["\']_blank["\']', tag, re.IGNORECASE):
            raise PackageError(f"{label} external links must open in a new context")
        relation = re.search(r'\brel=["\']([^"\']+)["\']', tag, re.IGNORECASE)
        relation_tokens = set(relation.group(1).lower().split()) if relation else set()
        if not {"noopener", "noreferrer"}.issubset(relation_tokens):
            raise PackageError(f"{label} external links must prevent opener and referrer access")


def native_ui_payloads() -> tuple[tuple[bytes, str], ...]:
    """Validate and render the fixed AppWindow payload staged in the SPK."""
    app_config_path = UI_SOURCE / "app.config"
    app_config = _json_object(app_config_path)
    if set(app_config) != {DSM_APP_CLASS}:
        raise PackageError(
            f"app.config must define exactly the native DSM class {DSM_APP_CLASS}"
        )
    application = app_config[DSM_APP_CLASS]
    expected_application = {
        "type": "app",
        "title": "Synology Drive Sync",
        "desc": "app:description",
        "appWindow": DSM_APP_CLASS,
        "allUsers": False,
        "allowMultiInstance": False,
        "hidden": False,
        "icon": "images/icon_{0}.png",
        "preloadTexts": [
            "notifications:sync_succeeded_title",
            "notifications:sync_succeeded_message",
            "notifications:sync_failed_title",
            "notifications:sync_failed_message",
            "notifications:doctor_failed_title",
            "notifications:doctor_failed_message",
        ],
    }
    if not isinstance(application, dict):
        raise PackageError(f"app.config entry {DSM_APP_CLASS} must be an object")
    if set(application) != set(expected_application):
        raise PackageError(
            f"app.config entry {DSM_APP_CLASS} must contain only the reviewed fields"
        )
    for key, expected in expected_application.items():
        if application.get(key) != expected:
            raise PackageError(
                f"app.config entry {DSM_APP_CLASS}.{key} must equal {expected!r}"
            )

    config_define_path = UI_SOURCE / "config.define"
    config_define = _json_object(config_define_path)
    expected_define = {
        "SynologyDriveSync.js": {
            "JSfiles": ["dist/SynologyDriveSync.js"],
            "params": "-s -c skip",
        }
    }
    if config_define != expected_define:
        raise PackageError(
            "config.define must map SynologyDriveSync.js to the deterministic "
            "dist/SynologyDriveSync.js bundle"
        )

    # parse_requires.py adds the dependency list and GenerateJSDepend.php
    # combines app.config with config.define into this module-keyed installed
    # form. Render the same wrapper deterministically so SPK construction never
    # depends on an unpinned DSM toolkit installation.
    installed_application = dict(application)
    installed_application["depend"] = []
    installed_config = (
        json.dumps(
            {"SynologyDriveSync.js": {DSM_APP_CLASS: installed_application}},
            ensure_ascii=False,
            indent=2,
            separators=(",", ": "),
        )
        + "\n"
    ).encode("utf-8")
    help_toc_path = HERE / "package/ui/helptoc.conf"
    help_toc = _json_object(help_toc_path)
    expected_help_toc = {
        "app": DSM_APP_CLASS,
        "title": "app:title",
        "content": "overview.html",
        "toc": [
            {
                "title": f"help:{page}",
                "content": f"{page}.html",
            }
            for page in UI_HELP_PAGES
        ],
    }
    if help_toc != expected_help_toc:
        raise PackageError(
            "helptoc.conf must bind every reviewed dashboard section to the native DSM AppWindow"
        )

    help_sources: list[tuple[Path, str]] = []
    for page in UI_HELP_PAGES:
        source = HERE / f"package/ui/help/enu/{page}.html"
        payload = _regular_file_bytes(source)
        try:
            document = payload.decode("utf-8")
        except UnicodeDecodeError as error:
            raise PackageError(f"DSM Help document is not UTF-8: {source}") from error
        for marker in (
            '<html class="img-no-display">',
            '../../../../help/help.css',
            '../../../../help/scrollbar/flexcroll.css',
            '../../../../help/scrollbar/flexcroll.js',
            '../../../../help/scrollbar/initFlexcroll.js',
            "<h1>",
        ):
            if marker not in document:
                raise PackageError(f"DSM Help document {source.name} is missing {marker!r}")
        _validate_external_links(document, f"DSM Help document {source.name}")
        help_sources.append((source, f"ui/help/enu/{page}.html"))

    strings_path = HERE / "package/ui/texts/enu/strings"
    strings = _regular_file_bytes(strings_path)
    strings_text = strings.decode("utf-8")
    for page in UI_HELP_PAGES:
        if not re.search(rf'^{re.escape(page)}="[^"\r\n]+"$', strings_text, re.MULTILINE):
            raise PackageError(f"DSM Help text key is missing: help:{page}")
    sources = (
        (UI_SOURCE / "dist/SynologyDriveSync.js", "ui/SynologyDriveSync.js"),
        (UI_SOURCE / "dist/style.css", "ui/style.css"),
        (HERE / "package/ui/images/icon.svg", "ui/images/icon.svg"),
        (strings_path, "ui/texts/enu/strings"),
        (help_toc_path, "ui/helptoc.conf"),
        *help_sources,
    )
    return ((installed_config, "ui/config"),) + tuple(
        (_regular_file_bytes(source), destination) for source, destination in sources
    )


def _rounded_square_distance(x: float, y: float) -> float:
    half = 0.4375
    radius = 0.19
    qx = abs(x - 0.5) - (half - radius)
    qy = abs(y - 0.5) - (half - radius)
    return math.hypot(max(qx, 0.0), max(qy, 0.0)) + min(max(qx, qy), 0.0) - radius


def _triangle_contains(
    x: float,
    y: float,
    first: tuple[float, float],
    second: tuple[float, float],
    third: tuple[float, float],
) -> bool:
    def edge(
        point_x: float,
        point_y: float,
        start: tuple[float, float],
        end: tuple[float, float],
    ) -> float:
        return (point_x - end[0]) * (start[1] - end[1]) - (start[0] - end[0]) * (point_y - end[1])

    first_edge = edge(x, y, first, second)
    second_edge = edge(x, y, second, third)
    third_edge = edge(x, y, third, first)
    return not ((first_edge < 0 or second_edge < 0 or third_edge < 0) and (first_edge > 0 or second_edge > 0 or third_edge > 0))


def _arrow_triangle(
    body: tuple[tuple[float, float], ...],
    tip: tuple[float, float],
) -> tuple[tuple[float, float], ...]:
    """Return a head whose rear edge is exactly the body endpoint.

    The head is rasterized after the trace body, but the body also terminates at
    this base rather than continuing beneath the head. That keeps the arrowhead
    visibly forward at DSM's 16px icon size instead of looking tucked behind the
    stroke.
    """
    base = body[-1]
    delta_x = tip[0] - base[0]
    delta_y = tip[1] - base[1]
    length = math.hypot(delta_x, delta_y)
    if length <= 0:
        raise PackageError("icon arrow tip must be ahead of its trace body")
    perpendicular = (-delta_y / length, delta_x / length)
    return (
        tip,
        (
            base[0] + perpendicular[0] * ICON_ARROW_HALF_WIDTH,
            base[1] + perpendicular[1] * ICON_ARROW_HALF_WIDTH,
        ),
        (
            base[0] - perpendicular[0] * ICON_ARROW_HALF_WIDTH,
            base[1] - perpendicular[1] * ICON_ARROW_HALF_WIDTH,
        ),
    )


def _segment_contains(
    x: float,
    y: float,
    start: tuple[float, float],
    end: tuple[float, float],
    half_width: float,
    *,
    extend_start: bool,
    extend_end: bool,
) -> bool:
    delta_x = end[0] - start[0]
    delta_y = end[1] - start[1]
    length = math.hypot(delta_x, delta_y)
    if length <= 0:
        return False
    projection = ((x - start[0]) * delta_x + (y - start[1]) * delta_y) / (
        length * length
    )
    extension = half_width / length
    minimum = -extension if extend_start else 0.0
    maximum = 1.0 + extension if extend_end else 1.0
    distance = abs((x - start[0]) * delta_y - (y - start[1]) * delta_x) / length
    return minimum <= projection <= maximum and distance <= half_width


def _trace_contains(
    x: float,
    y: float,
    points: tuple[tuple[float, float], ...],
    half_width: float,
    *,
    stop_at_final_base: bool = False,
) -> bool:
    last_segment = len(points) - 2
    return any(
        _segment_contains(
            x,
            y,
            points[index],
            points[index + 1],
            half_width,
            extend_start=True,
            extend_end=not (stop_at_final_base and index == last_segment),
        )
        for index in range(len(points) - 1)
    )


@functools.lru_cache(maxsize=len(UI_ICON_SIZES))
def png_icon(size: int) -> bytes:
    """Rasterize the authored sync mark deterministically with safe transparent bounds."""
    if size not in UI_ICON_SIZES:
        raise PackageError(f"unsupported UI icon size: {size}")
    supersample = 4
    top_arrow = _arrow_triangle(ICON_TOP_BODY, ICON_TOP_TIP)
    bottom_arrow = _arrow_triangle(ICON_BOTTOM_BODY, ICON_BOTTOM_TIP)
    rows: list[bytes] = []
    for y in range(size):
        row = bytearray([0])
        for x in range(size):
            red = green = blue = alpha = 0.0
            for sample_y in range(supersample):
                normalized_y = (y + (sample_y + 0.5) / supersample) / size
                for sample_x in range(supersample):
                    normalized_x = (x + (sample_x + 0.5) / supersample) / size
                    square_distance = _rounded_square_distance(
                        normalized_x, normalized_y
                    )
                    if square_distance > 0:
                        continue
                    pixel = ICON_BACKGROUND
                    if square_distance >= -ICON_BORDER_THICKNESS:
                        pixel = ICON_BORDER
                    if _trace_contains(
                        normalized_x,
                        normalized_y,
                        ICON_ORBIT_POINTS,
                        ICON_ORBIT_HALF_THICKNESS,
                    ):
                        pixel = ICON_ORBIT
                    if (
                        abs(normalized_x - 0.5) + abs(normalized_y - 0.5)
                        <= ICON_CENTER_RADIUS
                    ):
                        pixel = ICON_CENTER

                    on_top_body = _trace_contains(
                        normalized_x,
                        normalized_y,
                        ICON_TOP_BODY,
                        ICON_ARROW_HALF_THICKNESS,
                        stop_at_final_base=True,
                    )
                    on_bottom_body = _trace_contains(
                        normalized_x,
                        normalized_y,
                        ICON_BOTTOM_BODY,
                        ICON_ARROW_HALF_THICKNESS,
                        stop_at_final_base=True,
                    )
                    if on_top_body:
                        pixel = ICON_TOP
                    if on_bottom_body:
                        pixel = ICON_BOTTOM

                    # Heads are composited last and the trace bodies stop exactly
                    # at their rear edges, so no body stroke can cover an apex.
                    if _triangle_contains(normalized_x, normalized_y, *top_arrow):
                        pixel = ICON_TOP
                    if _triangle_contains(normalized_x, normalized_y, *bottom_arrow):
                        pixel = ICON_BOTTOM
                    red += pixel[0]
                    green += pixel[1]
                    blue += pixel[2]
                    alpha += 255.0
            samples = supersample * supersample
            if alpha:
                coverage = alpha / (255.0 * samples)
                row.extend(
                    (
                        round(red / (samples * coverage)),
                        round(green / (samples * coverage)),
                        round(blue / (samples * coverage)),
                        round(255 * coverage),
                    )
                )
            else:
                row.extend((0, 0, 0, 0))
        rows.append(bytes(row))
    raw = b"".join(rows)

    def chunk(kind: bytes, payload: bytes) -> bytes:
        return (
            struct.pack(">I", len(payload))
            + kind
            + payload
            + struct.pack(">I", binascii.crc32(kind + payload) & 0xFFFFFFFF)
        )

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def payload_archive(binary: Path, api_binary: Path) -> tuple[bytes, int]:
    script_sources = (
        HERE / "package/bin/sdsync-dsm",
        HERE / "package/libexec/sdsync-common",
        HERE / "package/libexec/sdsync-controller",
        HERE / "package/libexec/sdsync-run",
    )
    notice_sources = (
        REPOSITORY / "license.md",
        REPOSITORY / "third_party_licenses.html",
        HERE / "licenses/musl-COPYRIGHT",
        HERE / "licenses/DSM_UI_THIRD_PARTY_LICENSES.txt",
    )
    ui_payloads = native_ui_payloads()
    rendered_icons = {size: png_icon(size) for size in UI_ICON_SIZES}
    installed_size = (
        binary.stat().st_size
        + 2 * api_binary.stat().st_size
        + sum(path.stat().st_size for path in script_sources + notice_sources)
        + sum(len(payload) for payload, _destination in ui_payloads)
        + sum(len(payload) for payload in rendered_icons.values())
    )
    tar_stream = io.BytesIO()
    with tarfile.open(fileobj=tar_stream, mode="w", format=tarfile.PAX_FORMAT) as archive:
        for directory in (
            "bin",
            "libexec",
            "share",
            "share/licenses",
            "ui",
            "ui/images",
            "ui/help",
            "ui/help/enu",
            "ui/texts",
            "ui/texts/enu",
        ):
            archive.addfile(tar_info(directory, 0o755, directory=True))
        add_bytes(archive, "bin/synology-drive-sync", binary.read_bytes(), 0o755)
        api_payload = api_binary.read_bytes()
        add_bytes(archive, "bin/sdsync-dsm-api", api_payload, 0o755)
        # The dashboard entry point is an ordinary, rootless socket client. Keep
        # both the archive and the installed privilege contract free of
        # setuid/setgid bits; the package-owned API service owns the private side
        # of the fixed Unix socket instead of relying on an identity-changing CGI.
        add_bytes(archive, "ui/api.cgi", api_payload, 0o755)
        for source, destination in (
            (HERE / "package/bin/sdsync-dsm", "bin/sdsync-dsm"),
            (HERE / "package/libexec/sdsync-common", "libexec/sdsync-common"),
            (HERE / "package/libexec/sdsync-controller", "libexec/sdsync-controller"),
            (HERE / "package/libexec/sdsync-run", "libexec/sdsync-run"),
        ):
            add_bytes(archive, destination, source.read_bytes(), 0o755)
        add_bytes(
            archive,
            "share/licenses/synology-drive-sync-LICENSE",
            (REPOSITORY / "license.md").read_bytes(),
            0o644,
        )
        add_bytes(
            archive,
            "share/licenses/musl-COPYRIGHT",
            (HERE / "licenses/musl-COPYRIGHT").read_bytes(),
            0o644,
        )
        add_bytes(
            archive,
            "share/licenses/THIRD_PARTY_LICENSES.html",
            (REPOSITORY / "third_party_licenses.html").read_bytes(),
            0o644,
        )
        add_bytes(
            archive,
            "share/licenses/DSM_UI_THIRD_PARTY_LICENSES.txt",
            (HERE / "licenses/DSM_UI_THIRD_PARTY_LICENSES.txt").read_bytes(),
            0o644,
        )
        for payload, destination in ui_payloads:
            add_bytes(archive, destination, payload, 0o644)
        for size, icon in rendered_icons.items():
            add_bytes(archive, f"ui/images/icon_{size}.png", icon, 0o644)
    compressed = io.BytesIO()
    with gzip.GzipFile(
        filename="", fileobj=compressed, mode="wb", mtime=SOURCE_DATE_EPOCH
    ) as compressor:
        compressor.write(tar_stream.getvalue())
    return compressed.getvalue(), installed_size


def render_info(
    arch: str, dsm_version: str, extract_size: int, package_checksum: str
) -> bytes:
    template = (HERE / "INFO.template").read_text(encoding="utf-8")
    rendered = (
        template.replace("@ARCH@", ARCHITECTURES[arch].info_value)
        .replace("@DSM_VERSION@", dsm_version)
        .replace("@EXTRACT_SIZE_KIB@", str((extract_size + 1023) // 1024))
        .replace("@PACKAGE_TGZ_MD5@", package_checksum)
    )
    if "@" in rendered:
        raise PackageError("INFO.template contains an unresolved placeholder")
    return rendered.encode("utf-8")


def create_spk(
    binary: Path,
    api_binary: Path,
    arch: str,
    release: str,
    dsm_version: str,
    output: Path,
) -> Path:
    payload, installed_size = payload_archive(binary, api_binary)
    # Synology's INFO checksum is specifically the lowercase MD5 of the exact
    # compressed package.tgz member, not a replacement for release SHA-256 or
    # provenance attestations.
    package_checksum = hashlib.md5(payload, usedforsecurity=False).hexdigest()
    info = render_info(arch, dsm_version, installed_size, package_checksum)
    output.mkdir(parents=True, exist_ok=True)
    destination = output / f"{PACKAGE}-{release}-{arch}.spk"
    temporary = output / f".{destination.name}.tmp-{os.getpid()}"
    with tarfile.open(temporary, mode="w", format=tarfile.PAX_FORMAT) as archive:
        add_bytes(archive, "INFO", info, 0o644)
        add_bytes(archive, "PACKAGE_ICON.PNG", png_icon(64), 0o644)
        add_bytes(archive, "PACKAGE_ICON_256.PNG", png_icon(256), 0o644)
        license_path = REPOSITORY / "license.md"
        add_bytes(archive, "LICENSE", license_path.read_bytes(), 0o644)
        archive.addfile(tar_info("LICENSES", 0o755, directory=True))
        add_bytes(
            archive,
            "LICENSES/musl-COPYRIGHT",
            (HERE / "licenses/musl-COPYRIGHT").read_bytes(),
            0o644,
        )
        add_bytes(
            archive,
            "LICENSES/THIRD_PARTY_LICENSES.html",
            (REPOSITORY / "third_party_licenses.html").read_bytes(),
            0o644,
        )
        add_bytes(
            archive,
            "LICENSES/DSM_UI_THIRD_PARTY_LICENSES.txt",
            (HERE / "licenses/DSM_UI_THIRD_PARTY_LICENSES.txt").read_bytes(),
            0o644,
        )
        add_bytes(archive, "package.tgz", payload, 0o644)
        for directory in ("scripts", "conf"):
            archive.addfile(tar_info(directory, 0o755, directory=True))
        for source in sorted((HERE / "scripts").iterdir(), key=lambda item: item.name):
            if source.is_file():
                add_bytes(archive, f"scripts/{source.name}", source.read_bytes(), 0o755)
        add_bytes(archive, "conf/privilege", (HERE / "conf/privilege").read_bytes(), 0o644)
    os.replace(temporary, destination)
    return destination


def main() -> int:
    arguments = parse_arguments()
    release, dsm_version = normalized_versions(arguments.version)
    # Check the caller-supplied path before resolving it; otherwise Path.resolve() erases the
    # symlink property that this package boundary promises to reject.
    elf_contract(arguments.binary, arguments.arch)
    elf_contract(arguments.api_binary, arguments.arch)
    binary = arguments.binary.resolve()
    api_binary = arguments.api_binary.resolve()
    destination = create_spk(
        binary,
        api_binary,
        arguments.arch,
        release,
        dsm_version,
        arguments.output.resolve(),
    )
    print(destination)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, PackageError, tarfile.TarError) as error:
        print(f"SPK build failed: {error}", file=__import__("sys").stderr)
        raise SystemExit(1)
