#!/usr/bin/env python3
"""Fail-closed state and asset contract for the calendar release workflow."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import stat
import sys
import tarfile
import zipfile
from pathlib import Path
from typing import Any, Iterable


CALENDAR_TAG = re.compile(r"^(?P<year>[0-9]{2})\.(?P<sequence>[1-9][0-9]*)$")
SHA = re.compile(r"^[0-9a-f]{40}$")
TIMESTAMP = re.compile(
    r"^[0-9]{4}-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12][0-9]|3[01])"
    r"T(?:[01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9]Z$"
)
REPOSITORY = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
MANIFEST_LINE = re.compile(r"^([0-9a-f]{64})  ([^/\\\r\n]+)$")
ASSET_DIGEST = re.compile(r"^sha256:[0-9a-f]{64}$")
SYNOLOGY_ARCHITECTURES = ("armv7", "armv8", "i686", "x86_64")


class ContractError(ValueError):
    """The supplied release state violates an invariant."""


def _object(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ContractError(f"{label} must be a JSON object")
    return value


def _array(value: Any, label: str) -> list[Any]:
    if not isinstance(value, list):
        raise ContractError(f"{label} must be a JSON array")
    return value


def _string(value: Any, label: str, *, allow_empty: bool = False) -> str:
    if not isinstance(value, str) or (not allow_empty and not value):
        qualifier = "a string" if allow_empty else "a non-empty string"
        raise ContractError(f"{label} must be {qualifier}")
    return value


def _boolean(value: Any, label: str) -> bool:
    if not isinstance(value, bool):
        raise ContractError(f"{label} must be a boolean")
    return value


def _sha(value: Any, label: str) -> str:
    candidate = _string(value, label)
    if not SHA.fullmatch(candidate):
        raise ContractError(f"{label} must be a lowercase 40-character Git SHA")
    return candidate


def _tag(value: Any, label: str = "tag") -> str:
    candidate = _string(value, label)
    if not CALENDAR_TAG.fullmatch(candidate):
        raise ContractError(f"{label} must match YY.N with a positive sequence")
    return candidate


def _tag_key(tag: str) -> tuple[int, int]:
    match = CALENDAR_TAG.fullmatch(tag)
    if match is None:
        raise ContractError(f"malformed calendar tag: {tag}")
    return int(match.group("year")), int(match.group("sequence"))


def _release_records(value: Any) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    for index, raw in enumerate(_array(value, "releases")):
        record = _object(raw, f"releases[{index}]")
        release_id = record.get("id")
        if isinstance(release_id, bool) or not isinstance(release_id, int) or release_id <= 0:
            raise ContractError(f"releases[{index}].id must be a positive integer")
        _string(record.get("tag_name"), f"releases[{index}].tag_name", allow_empty=True)
        _boolean(record.get("draft"), f"releases[{index}].draft")
        _string(
            record.get("target_commitish"),
            f"releases[{index}].target_commitish",
            allow_empty=True,
        )
        if "html_url" in record and record["html_url"] is not None:
            _string(record["html_url"], f"releases[{index}].html_url")
        records.append(record)
    return records


def _tag_records(value: Any) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    names: set[str] = set()
    for index, raw in enumerate(_array(value, "tags")):
        record = _object(raw, f"tags[{index}]")
        name = _tag(record.get("name"), f"tags[{index}].name")
        if name in names:
            raise ContractError(f"tags contains duplicate name {name}")
        names.add(name)
        records.append(
            {
                "name": name,
                "sha": _sha(record.get("sha"), f"tags[{index}].sha"),
                "ancestor_of_sha": _boolean(
                    record.get("ancestor_of_sha"),
                    f"tags[{index}].ancestor_of_sha",
                ),
            }
        )
    return records


def _release_description(release: dict[str, Any]) -> str:
    url = release.get("html_url") or "<no URL>"
    return (
        f"id={release['id']} tag={release['tag_name']} "
        f"draft={str(release['draft']).lower()} "
        f"target={release['target_commitish']} {url}"
    )


def select_version(document: Any) -> dict[str, Any]:
    """Select the exact YY.N release identity from a normalized snapshot."""

    data = _object(document, "input")
    sha = _sha(data.get("sha"), "sha")
    main_sha = _sha(data.get("main_sha"), "main_sha")
    reachable = _boolean(data.get("sha_reachable_from_main"), "sha_reachable_from_main")
    year = _string(data.get("year"), "year")
    if not re.fullmatch(r"[0-9]{2}", year):
        raise ContractError("year must contain exactly two decimal digits")
    repository = _string(data.get("repository"), "repository")
    if not REPOSITORY.fullmatch(repository):
        raise ContractError("repository must be an owner/name slug")
    build_timestamp = _string(data.get("build_timestamp"), "build_timestamp")
    if not TIMESTAMP.fullmatch(build_timestamp):
        raise ContractError("build_timestamp must be a UTC YYYY-MM-DDTHH:MM:SSZ value")

    releases = _release_records(data.get("releases"))
    tags = _tag_records(data.get("tags"))
    tags_by_name = {record["name"]: record for record in tags}

    published_for_sha = sorted(
        {
            release["tag_name"]
            for release in releases
            if not release["draft"]
            and CALENDAR_TAG.fullmatch(release["tag_name"])
            and release["tag_name"] in tags_by_name
            and tags_by_name[release["tag_name"]]["sha"] == sha
        },
        key=_tag_key,
    )
    drafts_for_sha = [
        release
        for release in releases
        if release["draft"]
        and release["target_commitish"] == sha
        and CALENDAR_TAG.fullmatch(release["tag_name"])
    ]
    drafts_for_sha.sort(key=lambda release: (_tag_key(release["tag_name"]), release["id"]))

    already_published = False
    draft_release_id: int | None = None
    if published_for_sha:
        tag = published_for_sha[-1]
        already_published = True
    elif len(drafts_for_sha) > 1:
        details = "\n".join(f"  {_release_description(item)}" for item in drafts_for_sha)
        commands = "\n".join(
            f"  gh api --method DELETE repos/{repository}/releases/{item['id']}"
            for item in drafts_for_sha
        )
        raise ContractError(
            f"multiple calendar-release drafts target {sha}; refusing an ambiguous resume.\n"
            f"Offending draft releases:\n{details}\n"
            f"Delete all but the one to keep, then re-run this workflow:\n{commands}\n"
            "Delete by id; deleting by duplicated tag name may remove the wrong draft."
        )
    elif len(drafts_for_sha) == 1:
        draft = drafts_for_sha[0]
        tag = draft["tag_name"]
        draft_release_id = draft["id"]
        existing = tags_by_name.get(tag)
        if existing is not None and existing["sha"] != sha:
            raise ContractError(
                f"draft release {tag} (id {draft_release_id}) conflicts with existing "
                f"tag at {existing['sha']}; expected {sha}"
            )
    else:
        tags_for_sha = sorted(
            (record["name"] for record in tags if record["sha"] == sha),
            key=_tag_key,
        )
        if tags_for_sha:
            tag = tags_for_sha[-1]
        else:
            sequences = [
                _tag_key(record["name"])[1]
                for record in tags
                if record["name"].startswith(f"{year}.")
            ]
            sequences.extend(
                _tag_key(release["tag_name"])[1]
                for release in releases
                if CALENDAR_TAG.fullmatch(release["tag_name"])
                and release["tag_name"].startswith(f"{year}.")
            )
            tag = f"{year}.{max(sequences, default=0) + 1}"

    if not already_published:
        if not reachable:
            raise ContractError(
                f"refusing to release {sha}: it is not reachable from origin/main ({main_sha})"
            )
        if tags:
            newest = max(tags, key=lambda record: _tag_key(record["name"]))
            if not newest["ancestor_of_sha"]:
                raise ContractError(
                    f"refusing to release {sha}: it predates the newest release "
                    f"{newest['name']} ({newest['sha']}); re-running an old CI run "
                    "cannot cut a new release of stale code"
                )

    previous = sorted(
        (
            record["name"]
            for record in tags
            if record["ancestor_of_sha"] and record["name"] != tag
        ),
        key=_tag_key,
    )
    return {
        "tag": tag,
        "prev_tag": previous[-1] if previous else "",
        "sha": sha,
        "build_timestamp": build_timestamp,
        "image": f"ghcr.io/{repository.lower()}",
        "already_published": already_published,
        "draft_release_id": draft_release_id,
    }


def _resolve_release_state(
    document: Any, *, expected_release_id: int | None = None
) -> dict[str, Any]:
    data = _object(document, "input")
    tag = _tag(data.get("tag"))
    sha = _sha(data.get("sha"), "sha")
    repository = _string(data.get("repository"), "repository")
    if not REPOSITORY.fullmatch(repository):
        raise ContractError("repository must be an owner/name slug")
    releases = _release_records(data.get("releases"))
    raw_tag_sha = data.get("tag_sha")
    tag_sha = None if raw_tag_sha is None else _sha(raw_tag_sha, "tag_sha")

    matches = [release for release in releases if release["tag_name"] == tag]
    if len(matches) > 1:
        details = "\n".join(f"  {_release_description(item)}" for item in matches)
        commands = "\n".join(
            f"  gh api --method DELETE repos/{repository}/releases/{item['id']}"
            for item in matches
        )
        raise ContractError(
            f"{len(matches)} releases carry the tag name {tag}; refusing an ambiguous resume.\n"
            f"Offending releases:\n{details}\n"
            f"Delete all but the one to keep, then re-run this workflow:\n{commands}\n"
            "Delete by id; deleting by duplicated tag name may remove the wrong release."
        )

    tag_matches = tag_sha == sha
    if tag_sha is not None and not tag_matches:
        raise ContractError(f"tag {tag} already points to {tag_sha}, expected {sha}")

    if not matches:
        if expected_release_id is not None:
            raise ContractError(
                f"release {tag} (expected id {expected_release_id}) no longer exists"
            )
        return {"release_id": None, "tag_matches": tag_matches}

    release = matches[0]
    if expected_release_id is not None and release["id"] != expected_release_id:
        raise ContractError(
            f"release {tag} changed immutable id from {expected_release_id} "
            f"to {release['id']}"
        )
    if not release["draft"]:
        if not tag_matches:
            raise ContractError(
                f"published release {tag} (id {release['id']}) does not resolve to expected "
                f"commit {sha}"
            )
        if expected_release_id is not None:
            return {
                "release_id": release["id"],
                "tag_matches": True,
                "published": True,
            }
        raise ContractError(
            f"release {tag} (id {release['id']}) is already published; refusing to modify "
            "immutable assets"
        )
    if not tag_matches and release["target_commitish"] != sha:
        raise ContractError(
            f"draft release {tag} (id {release['id']}) targets "
            f"{release['target_commitish']}, expected {sha}"
        )
    state = {"release_id": release["id"], "tag_matches": tag_matches}
    if expected_release_id is not None:
        state["published"] = False
    return state


def resolve_release_state(document: Any) -> dict[str, Any]:
    """Resolve a mutable draft while rejecting every published release."""

    return _resolve_release_state(document)


def resolve_publish_state(document: Any) -> dict[str, Any]:
    """Resolve the exact staged id for first publication or same-job recovery."""

    data = _object(document, "input")
    expected_release_id = data.get("expected_release_id")
    if (
        isinstance(expected_release_id, bool)
        or not isinstance(expected_release_id, int)
        or expected_release_id <= 0
    ):
        raise ContractError("expected_release_id must be a positive integer")
    return _resolve_release_state(data, expected_release_id=expected_release_id)


def draft_payload(tag: str, sha: str, notes: str) -> dict[str, Any]:
    """Return the one deterministic payload used for draft create and retry."""

    _tag(tag)
    _sha(sha, "sha")
    if not isinstance(notes, str):
        raise ContractError("notes must be a string")
    return {
        "tag_name": tag,
        "target_commitish": sha,
        "name": f"synology-drive-sync {tag}",
        "body": notes,
        "draft": True,
        "prerelease": False,
    }


def verify_image_index(document: Any) -> None:
    """Verify the exact supported runtime platform set of an OCI image index."""

    data = _object(document, "image index")
    if data.get("schemaVersion") != 2:
        raise ContractError("image index schemaVersion must be 2")
    if data.get("mediaType") != "application/vnd.oci.image.index.v1+json":
        raise ContractError("image index must use the OCI image index media type")

    manifests = _array(data.get("manifests"), "image index manifests")
    if not manifests:
        raise ContractError("image index manifests must not be empty")
    seen_digests: set[str] = set()
    runtime_platforms: list[str] = []
    for index, raw in enumerate(manifests):
        manifest = _object(raw, f"image index manifests[{index}]")
        digest = _string(manifest.get("digest"), f"image index manifests[{index}].digest")
        if not ASSET_DIGEST.fullmatch(digest):
            raise ContractError(
                f"image index manifests[{index}].digest must be a SHA-256 digest"
            )
        if digest in seen_digests:
            raise ContractError(f"image index contains duplicate digest {digest}")
        seen_digests.add(digest)
        size = manifest.get("size")
        if isinstance(size, bool) or not isinstance(size, int) or size <= 0:
            raise ContractError(
                f"image index manifests[{index}].size must be a positive integer"
            )
        platform = _object(
            manifest.get("platform"), f"image index manifests[{index}].platform"
        )
        operating_system = _string(
            platform.get("os"), f"image index manifests[{index}].platform.os"
        )
        architecture = _string(
            platform.get("architecture"),
            f"image index manifests[{index}].platform.architecture",
        )
        if (operating_system == "unknown") != (architecture == "unknown"):
            raise ContractError(
                "image index platform must be a runtime pair or unknown/unknown"
            )
        if operating_system != "unknown":
            runtime_platforms.append(f"{operating_system}/{architecture}")

    expected = ["linux/amd64", "linux/arm64"]
    if sorted(runtime_platforms) != expected:
        raise ContractError(
            f"image index runtime platforms are {sorted(runtime_platforms)}, expected {expected}"
        )


def verify_image_index_match(expected_document: Any, actual_document: Any) -> None:
    """Require two valid OCI indexes to have identical parsed JSON content."""

    verify_image_index(expected_document)
    verify_image_index(actual_document)
    expected_canonical = json.dumps(
        expected_document,
        sort_keys=True,
        ensure_ascii=False,
        separators=(",", ":"),
    )
    actual_canonical = json.dumps(
        actual_document,
        sort_keys=True,
        ensure_ascii=False,
        separators=(",", ":"),
    )
    if actual_canonical != expected_canonical:
        expected = _object(expected_document, "expected image index")
        actual = _object(actual_document, "actual image index")
        expected_digests = sorted(
            _string(item.get("digest"), "expected image index manifest digest")
            for item in _array(expected.get("manifests"), "expected image index manifests")
            if isinstance(item, dict)
        )
        actual_digests = sorted(
            _string(item.get("digest"), "actual image index manifest digest")
            for item in _array(actual.get("manifests"), "actual image index manifests")
            if isinstance(item, dict)
        )
        raise ContractError(
            "image index content differs from the exact expected index; "
            f"expected child digests {expected_digests}, actual child digests {actual_digests}"
        )


def archive_names(tag: str) -> list[str]:
    _tag(tag)
    names = [
        f"synology-drive-sync-{tag}-linux-aarch64.tar.gz",
        f"synology-drive-sync-{tag}-linux-x86_64.tar.gz",
        f"synology-drive-sync-{tag}-macos-aarch64.tar.gz",
        f"synology-drive-sync-{tag}-macos-x86_64.tar.gz",
        f"synology-drive-sync-{tag}-windows-aarch64.zip",
        f"synology-drive-sync-{tag}-windows-x86_64.zip",
        f"synology-drive-sync-{tag}-c-sdk-linux-aarch64.tar.gz",
        f"synology-drive-sync-{tag}-c-sdk-linux-x86_64.tar.gz",
        f"synology-drive-sync-{tag}-c-sdk-macos-aarch64.tar.gz",
        f"synology-drive-sync-{tag}-c-sdk-macos-x86_64.tar.gz",
        f"synology-drive-sync-{tag}-c-sdk-windows-aarch64.zip",
        f"synology-drive-sync-{tag}-c-sdk-windows-x86_64.zip",
        f"synology-drive-sync-{tag}-rust-sdk.tar.gz",
    ]
    names.extend(
        f"synology-drive-sync-{tag}-{architecture}.spk"
        for architecture in SYNOLOGY_ARCHITECTURES
    )
    return sorted(names)


def payload_names(tag: str) -> list[str]:
    names = archive_names(tag)
    names.extend(
        [
            "THIRD_PARTY_LICENSES.html",
            "install.ps1",
            "install.sh",
            f"synology-drive-sync-{tag}.cdx.json",
        ]
    )
    return sorted(names)


def release_asset_names(tag: str) -> list[str]:
    return sorted([*payload_names(tag), "SHA256SUMS"])


def sdk_archive_specs(tag: str) -> dict[str, list[str]]:
    """Map each C SDK archive to its exact regular-file membership."""

    _tag(tag)
    libraries = {
        ("linux", "x86_64"): ["libsdsync.so"],
        ("linux", "aarch64"): ["libsdsync.so"],
        ("macos", "x86_64"): ["libsdsync.dylib"],
        ("macos", "aarch64"): ["libsdsync.dylib"],
        ("windows", "x86_64"): ["sdsync.dll", "sdsync.lib"],
        ("windows", "aarch64"): ["sdsync.dll", "sdsync.lib"],
    }
    specs: dict[str, list[str]] = {}
    for (platform, architecture), library_names in libraries.items():
        root = f"synology-drive-sync-{tag}-c-sdk-{platform}-{architecture}"
        extension = "zip" if platform == "windows" else "tar.gz"
        archive = f"{root}.{extension}"
        members = [
            f"{root}/LICENSE",
            f"{root}/THIRD_PARTY_LICENSES.html",
            f"{root}/examples/ffi/basic.c",
            f"{root}/include/sdsync.h",
        ]
        members.extend(f"{root}/lib/{name}" for name in library_names)
        specs[archive] = sorted(members)
    return specs


def _safe_archive_name(name: str, archive: str) -> str:
    normalized = name.replace("\\", "/").rstrip("/")
    components = normalized.split("/")
    if not normalized or normalized.startswith("/") or any(
        component in ("", ".", "..") for component in components
    ):
        raise ContractError(f"{archive} contains unsafe member name {name!r}")
    return normalized


def _tar_regular_files(path: Path) -> list[str]:
    names: list[str] = []
    try:
        with tarfile.open(path, "r:gz") as archive:
            for member in archive.getmembers():
                name = _safe_archive_name(member.name, path.name)
                if member.isdir():
                    continue
                if not member.isfile():
                    raise ContractError(
                        f"{path.name} contains non-regular member {name}"
                    )
                names.append(name)
    except (tarfile.TarError, EOFError) as error:
        raise ContractError(f"could not parse {path.name} as tar.gz: {error}") from error
    return names


def _zip_regular_files(path: Path) -> list[str]:
    names: list[str] = []
    try:
        with zipfile.ZipFile(path) as archive:
            for member in archive.infolist():
                name = _safe_archive_name(member.filename, path.name)
                if member.is_dir():
                    continue
                mode = (member.external_attr >> 16) & 0xFFFF
                if mode and stat.S_ISLNK(mode):
                    raise ContractError(
                        f"{path.name} contains symbolic-link member {name}"
                    )
                names.append(name)
            bad = archive.testzip()
            if bad is not None:
                raise ContractError(f"{path.name} has a corrupt member: {bad}")
    except zipfile.BadZipFile as error:
        raise ContractError(f"could not parse {path.name} as zip: {error}") from error
    return names


def verify_sdk_archives(directory: Path, tag: str) -> None:
    """Verify all six C SDK archives have only the approved file layout."""

    specs = sdk_archive_specs(tag)
    for archive_name, expected in sorted(specs.items()):
        path = directory / archive_name
        if path.is_symlink() or not path.is_file():
            raise ContractError(f"C SDK archive is missing or not a regular file: {archive_name}")
        actual = (
            _zip_regular_files(path)
            if archive_name.endswith(".zip")
            else _tar_regular_files(path)
        )
        if len(actual) != len(set(actual)):
            raise ContractError(f"{archive_name} contains duplicate regular-file names")
        _expect_names(actual, expected, f"{archive_name} contents")


def _directory_files(directory: Path) -> dict[str, Path]:
    if not directory.is_dir():
        raise ContractError(f"asset directory does not exist: {directory}")
    files: dict[str, Path] = {}
    for entry in directory.iterdir():
        if entry.is_symlink():
            raise ContractError(f"release asset must not be a symlink: {entry.name}")
        if not entry.is_file():
            raise ContractError(f"release asset directory contains a non-file: {entry.name}")
        files[entry.name] = entry
    return files


def _expect_names(actual: Iterable[str], expected: Iterable[str], label: str) -> None:
    actual_set = set(actual)
    expected_set = set(expected)
    missing = sorted(expected_set - actual_set)
    extra = sorted(actual_set - expected_set)
    if missing or extra:
        parts = [f"{label} does not match the exact release contract"]
        if missing:
            parts.append("missing: " + ", ".join(missing))
        if extra:
            parts.append("unexpected: " + ", ".join(extra))
        raise ContractError("; ".join(parts))


def _digest(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def _manifest_text(files: dict[str, Path], names: Iterable[str]) -> str:
    return "".join(f"{_digest(files[name])}  {name}\n" for name in sorted(names))


def prepare_assets(directory: Path, tag: str) -> None:
    """Validate the 21 inputs and write deterministic checksum manifests."""

    expected = payload_names(tag)
    files = _directory_files(directory)
    known_manifests = {"SHA256SUMS", "ARCHIVE_SHA256SUMS"}
    _expect_names(
        (name for name in files if name not in known_manifests),
        expected,
        "release payload",
    )
    for manifest in known_manifests:
        path = directory / manifest
        if path.exists() and (path.is_symlink() or not path.is_file()):
            raise ContractError(f"checksum manifest must be a regular file: {manifest}")
    files = {name: directory / name for name in expected}
    (directory / "ARCHIVE_SHA256SUMS").write_text(
        _manifest_text(files, archive_names(tag)), encoding="utf-8", newline="\n"
    )
    (directory / "SHA256SUMS").write_text(
        _manifest_text(files, expected), encoding="utf-8", newline="\n"
    )


def _parse_manifest(path: Path, expected_names: list[str]) -> dict[str, str]:
    if path.is_symlink() or not path.is_file():
        raise ContractError(f"checksum manifest is missing or not a regular file: {path.name}")
    lines = path.read_text(encoding="utf-8").splitlines()
    parsed: dict[str, str] = {}
    ordered_names: list[str] = []
    for number, line in enumerate(lines, start=1):
        match = MANIFEST_LINE.fullmatch(line)
        if match is None:
            raise ContractError(f"{path.name}:{number} is not a strict sha256sum record")
        digest, name = match.groups()
        if name in parsed:
            raise ContractError(f"{path.name} contains duplicate entry {name}")
        parsed[name] = digest
        ordered_names.append(name)
    if ordered_names != sorted(expected_names):
        _expect_names(ordered_names, expected_names, path.name)
        raise ContractError(f"{path.name} entries are not in deterministic C-byte order")
    return parsed


def verify_assets(directory: Path, tag: str, *, include_archive_manifest: bool = False) -> None:
    """Verify exact membership plus both manifests and every recorded digest."""

    expected_payload = payload_names(tag)
    expected_files = [*expected_payload, "SHA256SUMS"]
    if include_archive_manifest:
        expected_files.append("ARCHIVE_SHA256SUMS")
    files = _directory_files(directory)
    _expect_names(files, expected_files, "release assets")

    sha_records = _parse_manifest(directory / "SHA256SUMS", expected_payload)
    for name, expected_digest in sha_records.items():
        actual_digest = _digest(directory / name)
        if actual_digest != expected_digest:
            raise ContractError(
                f"SHA256SUMS digest mismatch for {name}: {actual_digest} != {expected_digest}"
            )
    archive_manifest = directory / "ARCHIVE_SHA256SUMS"
    if include_archive_manifest:
        archive_records = _parse_manifest(archive_manifest, archive_names(tag))
        for name, expected_digest in archive_records.items():
            if sha_records[name] != expected_digest:
                raise ContractError(
                    f"ARCHIVE_SHA256SUMS and SHA256SUMS disagree for {name}"
                )


def verify_asset_names(tag: str, names: Iterable[str]) -> None:
    actual = list(names)
    if any(not isinstance(name, str) or not name for name in actual):
        raise ContractError("remote asset names must be non-empty strings")
    if len(actual) != len(set(actual)):
        raise ContractError("remote release contains duplicate asset names")
    _expect_names(actual, release_asset_names(tag), "remote release assets")


def asset_index(directory: Path, tag: str) -> dict[str, Any]:
    """Build a deterministic local size/digest inventory for all 22 assets."""

    verify_assets(directory, tag)
    assets = []
    for name in release_asset_names(tag):
        path = directory / name
        assets.append(
            {
                "name": name,
                "size": path.stat().st_size,
                "digest": f"sha256:{_digest(path)}",
            }
        )
    return {"schema": 1, "tag": tag, "assets": assets}


def _validated_asset_index(document: Any, tag: str) -> dict[str, dict[str, Any]]:
    data = _object(document, "asset index")
    if set(data) != {"schema", "tag", "assets"}:
        raise ContractError("asset index must contain exactly schema, tag and assets")
    if data["schema"] != 1 or isinstance(data["schema"], bool):
        raise ContractError("asset index schema must be 1")
    if data["tag"] != tag:
        raise ContractError(f"asset index tag is {data['tag']!r}, expected {tag}")
    records: dict[str, dict[str, Any]] = {}
    ordered_names: list[str] = []
    for index, raw in enumerate(_array(data["assets"], "asset index assets")):
        record = _object(raw, f"asset index assets[{index}]")
        if set(record) != {"name", "size", "digest"}:
            raise ContractError(
                f"asset index assets[{index}] must contain exactly name, size and digest"
            )
        name = _string(record["name"], f"asset index assets[{index}].name")
        size = record["size"]
        if isinstance(size, bool) or not isinstance(size, int) or size < 0:
            raise ContractError(f"asset index assets[{index}].size must be a non-negative integer")
        digest = _string(record["digest"], f"asset index assets[{index}].digest")
        if not ASSET_DIGEST.fullmatch(digest):
            raise ContractError(f"asset index digest is malformed for {name}: {digest}")
        if name in records:
            raise ContractError(f"asset index contains duplicate name {name}")
        records[name] = record
        ordered_names.append(name)
    expected_names = release_asset_names(tag)
    _expect_names(ordered_names, expected_names, "asset index")
    if ordered_names != expected_names:
        raise ContractError("asset index entries are not in deterministic C-byte order")
    return records


def verify_remote_assets(
    document: Any,
    tag: str,
    *,
    directory: Path | None = None,
    index_document: Any | None = None,
) -> None:
    """Compare uploaded GitHub asset state, size and digest to local truth."""

    if (directory is None) == (index_document is None):
        raise ContractError("provide exactly one local asset directory or asset index")
    expected_document = (
        asset_index(directory, tag) if directory is not None else index_document
    )
    expected = _validated_asset_index(expected_document, tag)

    remote: dict[str, dict[str, Any]] = {}
    remote_names: list[str] = []
    for index, raw in enumerate(_array(document, "remote assets")):
        record = _object(raw, f"remote assets[{index}]")
        name = _string(record.get("name"), f"remote assets[{index}].name")
        if name in remote:
            raise ContractError(f"remote release contains duplicate asset name {name}")
        state = _string(record.get("state"), f"remote assets[{index}].state")
        if state != "uploaded":
            raise ContractError(f"remote asset {name} is in state {state!r}, expected 'uploaded'")
        size = record.get("size")
        if isinstance(size, bool) or not isinstance(size, int) or size < 0:
            raise ContractError(f"remote asset {name} has a malformed byte size")
        digest = record.get("digest")
        if not isinstance(digest, str) or not ASSET_DIGEST.fullmatch(digest):
            raise ContractError(f"remote asset {name} has a missing or malformed sha256 digest")
        remote[name] = {"name": name, "state": state, "size": size, "digest": digest}
        remote_names.append(name)
    _expect_names(remote_names, expected, "remote release assets")

    for name, local in expected.items():
        uploaded = remote[name]
        if uploaded["size"] != local["size"]:
            raise ContractError(
                f"remote asset {name} byte size is {uploaded['size']}, expected {local['size']}"
            )
        if uploaded["digest"] != local["digest"]:
            raise ContractError(
                f"remote asset {name} digest is {uploaded['digest']}, expected {local['digest']}"
            )


def _load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        raise ContractError(
            f"could not parse JSON from {path}: line {error.lineno}, column {error.colno}: {error.msg}"
        ) from error


def _write_json(value: Any, path: Path | None = None) -> None:
    rendered = json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":")) + "\n"
    if path is None:
        sys.stdout.write(rendered)
    else:
        path.write_text(rendered, encoding="utf-8", newline="\n")


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    for command in ("select-version", "resolve-state", "resolve-publish-state"):
        child = commands.add_parser(command)
        child.add_argument("--input", required=True, type=Path)
        child.add_argument("--output", type=Path)

    payload = commands.add_parser("draft-payload")
    payload.add_argument("--tag", required=True)
    payload.add_argument("--sha", required=True)
    payload.add_argument("--notes", required=True, type=Path)
    payload.add_argument("--output", required=True, type=Path)

    prepare = commands.add_parser("prepare-assets")
    prepare.add_argument("--directory", required=True, type=Path)
    prepare.add_argument("--tag", required=True)

    verify = commands.add_parser("verify-assets")
    verify.add_argument("--directory", required=True, type=Path)
    verify.add_argument("--tag", required=True)
    verify.add_argument("--include-archive-manifest", action="store_true")

    sdk = commands.add_parser("verify-sdk-archives")
    sdk.add_argument("--directory", required=True, type=Path)
    sdk.add_argument("--tag", required=True)

    names = commands.add_parser("verify-asset-names")
    names.add_argument("--tag", required=True)
    names.add_argument("--names-file", required=True, type=Path)

    index = commands.add_parser("asset-index")
    index.add_argument("--directory", required=True, type=Path)
    index.add_argument("--tag", required=True)
    index.add_argument("--output", required=True, type=Path)

    remote = commands.add_parser("verify-remote-assets")
    remote.add_argument("--input", required=True, type=Path)
    remote.add_argument("--tag", required=True)
    source = remote.add_mutually_exclusive_group(required=True)
    source.add_argument("--directory", type=Path)
    source.add_argument("--index", type=Path)

    image = commands.add_parser("verify-image-index")
    image.add_argument("--input", required=True, type=Path)

    image_match = commands.add_parser("verify-image-index-match")
    image_match.add_argument("--expected", required=True, type=Path)
    image_match.add_argument("--actual", required=True, type=Path)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "select-version":
            _write_json(select_version(_load_json(args.input)), args.output)
        elif args.command == "resolve-state":
            _write_json(resolve_release_state(_load_json(args.input)), args.output)
        elif args.command == "resolve-publish-state":
            _write_json(resolve_publish_state(_load_json(args.input)), args.output)
        elif args.command == "draft-payload":
            notes = args.notes.read_text(encoding="utf-8")
            _write_json(draft_payload(args.tag, args.sha, notes), args.output)
        elif args.command == "prepare-assets":
            prepare_assets(args.directory, args.tag)
        elif args.command == "verify-assets":
            verify_assets(
                args.directory,
                args.tag,
                include_archive_manifest=args.include_archive_manifest,
            )
        elif args.command == "verify-sdk-archives":
            verify_sdk_archives(args.directory, args.tag)
        elif args.command == "verify-asset-names":
            names = args.names_file.read_text(encoding="utf-8").splitlines()
            verify_asset_names(args.tag, names)
        elif args.command == "asset-index":
            _write_json(asset_index(args.directory, args.tag), args.output)
        elif args.command == "verify-remote-assets":
            index_document = _load_json(args.index) if args.index is not None else None
            verify_remote_assets(
                _load_json(args.input),
                args.tag,
                directory=args.directory,
                index_document=index_document,
            )
        elif args.command == "verify-image-index":
            verify_image_index(_load_json(args.input))
        elif args.command == "verify-image-index-match":
            verify_image_index_match(
                _load_json(args.expected),
                _load_json(args.actual),
            )
        else:  # pragma: no cover - argparse owns this branch.
            raise ContractError(f"unknown command: {args.command}")
    except (ContractError, OSError, UnicodeError) as error:
        print(f"release contract error: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
