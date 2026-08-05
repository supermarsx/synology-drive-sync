#!/usr/bin/env python3
"""Build a deterministic, manually installable DSM 7 SPK from one static ELF."""

from __future__ import annotations

import argparse
import binascii
import gzip
import io
import os
import re
import struct
import tarfile
import zlib
from pathlib import Path


HERE = Path(__file__).resolve().parent
REPOSITORY = HERE.parents[1]
PACKAGE = "synology-drive-sync"
ARCH_MACHINES = {"x86_64": 62, "armv8": 183}
VERSION_PATTERN = re.compile(r"[0-9]+(?:[._-][0-9]+)*\Z")
SOURCE_DATE_EPOCH = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))


class PackageError(ValueError):
    """A deterministic package-input validation failure."""


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build a DSM 7 SPK around a prebuilt static Linux binary."
    )
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--arch", required=True, choices=sorted(ARCH_MACHINES))
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
    if len(data) < 64 or data[:4] != b"\x7fELF":
        raise PackageError("binary is not an ELF executable")
    if data[4] != 2 or data[5] != 1:
        raise PackageError("binary must be a little-endian ELF64 executable")
    elf_type = struct.unpack_from("<H", data, 16)[0]
    if elf_type not in (2, 3):
        raise PackageError("binary must be an ELF ET_EXEC or ET_DYN executable")
    machine = struct.unpack_from("<H", data, 18)[0]
    expected = ARCH_MACHINES[arch]
    if machine != expected:
        raise PackageError(
            f"binary ELF machine is {machine}, expected {expected} for DSM arch {arch}"
        )
    program_offset = struct.unpack_from("<Q", data, 32)[0]
    program_size = struct.unpack_from("<H", data, 54)[0]
    program_count = struct.unpack_from("<H", data, 56)[0]
    if program_size < 56:
        raise PackageError("binary has an invalid ELF64 program-header size")
    if program_count == 0:
        raise PackageError("binary has no ELF program headers")
    if program_offset + program_size * program_count > len(data):
        raise PackageError("binary has truncated ELF program headers")
    executable_load = False
    for index in range(program_count):
        offset = program_offset + index * program_size
        kind = struct.unpack_from("<I", data, offset)[0]
        flags = struct.unpack_from("<I", data, offset + 4)[0]
        file_offset = struct.unpack_from("<Q", data, offset + 8)[0]
        file_size = struct.unpack_from("<Q", data, offset + 32)[0]
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
        dynamic_offset = struct.unpack_from("<Q", data, offset + 8)[0]
        dynamic_size = struct.unpack_from("<Q", data, offset + 32)[0]
        if dynamic_offset + dynamic_size > len(data):
            raise PackageError("binary has a truncated PT_DYNAMIC segment")
        for entry in range(dynamic_offset, dynamic_offset + dynamic_size, 16):
            if entry + 16 > len(data):
                raise PackageError("binary has a truncated dynamic entry")
            tag = struct.unpack_from("<q", data, entry)[0]
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


def png_icon(size: int) -> bytes:
    """Generate a deterministic dark-blue icon with a bright two-arrow sync mark."""
    rows: list[bytes] = []
    center = size // 2
    thickness = max(2, size // 18)
    for y in range(size):
        row = bytearray([0])
        for x in range(size):
            edge = min(x, y, size - 1 - x, size - 1 - y)
            background = (12 + min(edge, 8), 24 + min(edge, 8), 38 + min(edge, 10), 255)
            upper = center - size // 7
            lower = center + size // 7
            rightward = upper - thickness <= y <= upper + thickness and size // 4 <= x <= 3 * size // 4
            leftward = lower - thickness <= y <= lower + thickness and size // 4 <= x <= 3 * size // 4
            arrow_right = x >= 3 * size // 4 - thickness * 2 and abs(y - upper) <= (x - (3 * size // 4 - thickness * 2))
            arrow_left = x <= size // 4 + thickness * 2 and abs(y - lower) <= ((size // 4 + thickness * 2) - x)
            pixel = (55, 210, 186, 255) if rightward or leftward or arrow_right or arrow_left else background
            row.extend(pixel)
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


def payload_archive(binary: Path) -> tuple[bytes, int]:
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
    installed_size = binary.stat().st_size + sum(
        path.stat().st_size for path in script_sources + notice_sources
    )
    tar_stream = io.BytesIO()
    with tarfile.open(fileobj=tar_stream, mode="w", format=tarfile.PAX_FORMAT) as archive:
        for directory in ("bin", "libexec", "share", "share/licenses"):
            archive.addfile(tar_info(directory, 0o755, directory=True))
        add_bytes(archive, "bin/synology-drive-sync", binary.read_bytes(), 0o755)
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
    compressed = io.BytesIO()
    with gzip.GzipFile(
        filename="", fileobj=compressed, mode="wb", mtime=SOURCE_DATE_EPOCH
    ) as compressor:
        compressor.write(tar_stream.getvalue())
    return compressed.getvalue(), installed_size


def render_info(arch: str, dsm_version: str, extract_size: int) -> bytes:
    template = (HERE / "INFO.template").read_text(encoding="utf-8")
    rendered = (
        template.replace("@ARCH@", arch)
        .replace("@DSM_VERSION@", dsm_version)
        .replace("@EXTRACT_SIZE_KIB@", str((extract_size + 1023) // 1024))
    )
    if "@" in rendered:
        raise PackageError("INFO.template contains an unresolved placeholder")
    return rendered.encode("utf-8")


def create_spk(binary: Path, arch: str, release: str, dsm_version: str, output: Path) -> Path:
    payload, installed_size = payload_archive(binary)
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
    os.replace(temporary, destination)
    return destination


def main() -> int:
    arguments = parse_arguments()
    release, dsm_version = normalized_versions(arguments.version)
    # Check the caller-supplied path before resolving it; otherwise Path.resolve() erases the
    # symlink property that this package boundary promises to reject.
    elf_contract(arguments.binary, arguments.arch)
    binary = arguments.binary.resolve()
    destination = create_spk(
        binary,
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
