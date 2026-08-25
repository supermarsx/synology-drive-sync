#!/usr/bin/env python3
"""Build a deterministic, manually installable DSM 7 SPK from one static ELF."""

from __future__ import annotations

import argparse
import binascii
import functools
import gzip
import io
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
UI_ICON_SIZES = (16, 24, 32, 48, 64, 72, 256)


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
        description="Build a DSM 7 SPK around a prebuilt static Linux binary."
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


def _arrow_triangle(angle: float) -> tuple[tuple[float, float], ...]:
    radius = 0.265
    tip = (0.5 + radius * math.cos(angle), 0.5 + radius * math.sin(angle))
    tangent = (-math.sin(angle), math.cos(angle))
    radial = (math.cos(angle), math.sin(angle))
    base = (tip[0] - tangent[0] * 0.105, tip[1] - tangent[1] * 0.105)
    return (
        tip,
        (base[0] + radial[0] * 0.07, base[1] + radial[1] * 0.07),
        (base[0] - radial[0] * 0.07, base[1] - radial[1] * 0.07),
    )


@functools.lru_cache(maxsize=len(UI_ICON_SIZES))
def png_icon(size: int) -> bytes:
    """Rasterize the authored sync mark deterministically with safe transparent bounds."""
    if size not in UI_ICON_SIZES:
        raise PackageError(f"unsupported UI icon size: {size}")
    supersample = 4
    arrow_one = _arrow_triangle(-0.10)
    arrow_two = _arrow_triangle(3.02)
    rows: list[bytes] = []
    for y in range(size):
        row = bytearray([0])
        for x in range(size):
            red = green = blue = alpha = 0.0
            for sample_y in range(supersample):
                normalized_y = (y + (sample_y + 0.5) / supersample) / size
                for sample_x in range(supersample):
                    normalized_x = (x + (sample_x + 0.5) / supersample) / size
                    if _rounded_square_distance(normalized_x, normalized_y) > 0:
                        continue
                    delta_x = normalized_x - 0.5
                    delta_y = normalized_y - 0.5
                    radius = math.hypot(delta_x, delta_y)
                    glow = max(0.0, 1.0 - radius / 0.62)
                    pixel = (9 + 7 * glow, 18 + 13 * glow, 16 + 11 * glow)
                    angle = math.atan2(delta_y, delta_x)
                    on_arc = abs(radius - 0.265) <= 0.033 and (
                        -2.82 <= angle <= -0.10 or 0.34 <= angle <= 3.02
                    )
                    on_arrow = _triangle_contains(
                        normalized_x, normalized_y, *arrow_one
                    ) or _triangle_contains(normalized_x, normalized_y, *arrow_two)
                    on_center = radius <= 0.055
                    on_orbit = abs(radius - 0.355) <= 0.004
                    if on_arc or on_arrow:
                        pixel = (98.0, 229.0, 197.0)
                    elif on_center:
                        pixel = (45.0, 182.0, 156.0)
                    elif on_orbit:
                        pixel = (37.0, 66.0, 58.0)
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
        REPOSITORY / "LICENSE",
        REPOSITORY / "THIRD_PARTY_LICENSES.html",
        HERE / "licenses/musl-COPYRIGHT",
    )
    ui_sources = (
        (HERE / "package/ui/config", "ui/config"),
        (HERE / "package/ui/index.html", "ui/index.html"),
        (HERE / "package/ui/app.css", "ui/app.css"),
        (HERE / "package/ui/app.js", "ui/app.js"),
        (HERE / "package/ui/images/icon.svg", "ui/images/icon.svg"),
        (HERE / "package/ui/texts/enu/strings", "ui/texts/enu/strings"),
        (HERE / "package/ui/texts/enu/mails", "ui/texts/enu/mails"),
    )
    rendered_icons = {size: png_icon(size) for size in UI_ICON_SIZES}
    installed_size = (
        binary.stat().st_size
        + 2 * api_binary.stat().st_size
        + sum(path.stat().st_size for path in script_sources + notice_sources)
        + sum(path.stat().st_size for path, _destination in ui_sources)
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
            (REPOSITORY / "LICENSE").read_bytes(),
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
            (REPOSITORY / "THIRD_PARTY_LICENSES.html").read_bytes(),
            0o644,
        )
        for source, destination in ui_sources:
            add_bytes(archive, destination, source.read_bytes(), 0o644)
        for size, icon in rendered_icons.items():
            add_bytes(archive, f"ui/images/icon_{size}.png", icon, 0o644)
    compressed = io.BytesIO()
    with gzip.GzipFile(
        filename="", fileobj=compressed, mode="wb", mtime=SOURCE_DATE_EPOCH
    ) as compressor:
        compressor.write(tar_stream.getvalue())
    return compressed.getvalue(), installed_size


def render_info(arch: str, dsm_version: str, extract_size: int) -> bytes:
    template = (HERE / "INFO.template").read_text(encoding="utf-8")
    rendered = (
        template.replace("@ARCH@", ARCHITECTURES[arch].info_value)
        .replace("@DSM_VERSION@", dsm_version)
        .replace("@EXTRACT_SIZE_KIB@", str((extract_size + 1023) // 1024))
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
    info = render_info(arch, dsm_version, installed_size)
    output.mkdir(parents=True, exist_ok=True)
    destination = output / f"{PACKAGE}-{release}-{arch}.spk"
    temporary = output / f".{destination.name}.tmp-{os.getpid()}"
    with tarfile.open(temporary, mode="w", format=tarfile.PAX_FORMAT) as archive:
        add_bytes(archive, "INFO", info, 0o644)
        add_bytes(archive, "PACKAGE_ICON.PNG", png_icon(64), 0o644)
        add_bytes(archive, "PACKAGE_ICON_256.PNG", png_icon(256), 0o644)
        license_path = REPOSITORY / "LICENSE"
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
            (REPOSITORY / "THIRD_PARTY_LICENSES.html").read_bytes(),
            0o644,
        )
        add_bytes(archive, "package.tgz", payload, 0o644)
        for directory in ("scripts", "conf"):
            archive.addfile(tar_info(directory, 0o755, directory=True))
        for source in sorted((HERE / "scripts").iterdir(), key=lambda item: item.name):
            if source.is_file():
                add_bytes(archive, f"scripts/{source.name}", source.read_bytes(), 0o755)
        add_bytes(archive, "conf/privilege", (HERE / "conf/privilege").read_bytes(), 0o644)
        add_bytes(archive, "conf/resource", (HERE / "conf/resource").read_bytes(), 0o644)
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
