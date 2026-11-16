from __future__ import annotations

import importlib.util
import re
import shutil
import tempfile
import tomllib
import unittest
from pathlib import Path


TEST_ROOT = Path(__file__).resolve().parent
REPOSITORY_ROOT = TEST_ROOT.parents[2]
SCRIPT = TEST_ROOT.parent / "crate-version.py"

SPEC = importlib.util.spec_from_file_location("crate_version", SCRIPT)
if SPEC is None or SPEC.loader is None:  # pragma: no cover - import machinery invariant
    raise RuntimeError(f"could not load {SCRIPT}")
crate_version = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(crate_version)

TRACKED = (
    crate_version.ROOT_MANIFEST,
    crate_version.FFI_MANIFEST,
    crate_version.LOCKFILE,
    crate_version.HEADER,
    crate_version.FFI_HEADER,
    crate_version.LICENSE,
    crate_version.FFI_LICENSE,
    Path("crates-io.md"),
    Path("crates/synology-drive-sync-ffi/crates-io.md"),
)


class CalendarMappingTests(unittest.TestCase):
    def test_calendar_tags_map_to_zero_year_number(self):
        for tag, expected in (
            ("26.66", "0.26.66"),
            ("26.1", "0.26.1"),
            ("27.1", "0.27.1"),
            ("26.100", "0.26.100"),
        ):
            with self.subTest(tag=tag):
                self.assertEqual(crate_version.crate_version(tag), expected)

    def test_non_calendar_tags_are_refused(self):
        for tag in ("v26.66", "26.0", "26", "2026.1", "26.01", "26.1.0", "26.66-rc1", "", "0.26.66"):
            with self.subTest(tag=tag):
                with self.assertRaises(crate_version.VersionError):
                    crate_version.crate_version(tag)


class RepositoryStateTests(unittest.TestCase):
    def test_committed_tree_is_consistent(self):
        version = crate_version.check(REPOSITORY_ROOT)
        self.assertRegex(version, crate_version.CRATE_VERSION)

    def test_published_metadata_is_what_the_crates_need(self):
        root = tomllib.loads((REPOSITORY_ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]
        ffi = tomllib.loads(
            (REPOSITORY_ROOT / crate_version.FFI_MANIFEST).read_text(encoding="utf-8")
        )["package"]
        for package in (root, ffi):
            with self.subTest(crate=package["name"]):
                self.assertLessEqual(len(package["description"]), 200)
                self.assertLessEqual(len(package["keywords"]), 5)
                self.assertEqual(package["rust-version"], "1.90")
                self.assertEqual(package["license"], "MIT")
                self.assertEqual(
                    package["repository"], "https://github.com/supermarsx/synology-drive-sync"
                )
                self.assertTrue(package["homepage"].startswith("https://supermarsx.github.io/synology-drive-sync/"))
                self.assertEqual(package["documentation"], f"https://docs.rs/{package['name']}")
                self.assertEqual(package["readme"], "crates-io.md")
                self.assertNotIn("publish", package)
                self.assertTrue(package["categories"])
        # The FFI crate must publish second, so it names the root crate by exact version.
        ffi_manifest = tomllib.loads(
            (REPOSITORY_ROOT / crate_version.FFI_MANIFEST).read_text(encoding="utf-8")
        )
        dependency = ffi_manifest["dependencies"]["synology-drive-sync"]
        self.assertEqual(dependency["version"], f"={root['version']}")
        self.assertEqual(dependency["path"], "../..")

    def test_crates_io_readmes_use_absolute_links_and_state_the_limits(self):
        for relative in (Path("crates-io.md"), Path("crates/synology-drive-sync-ffi/crates-io.md")):
            text = (REPOSITORY_ROOT / relative).read_text(encoding="utf-8")
            with self.subTest(readme=str(relative)):
                for target in re.findall(r"\]\(([^)\s]+)\)", text):
                    self.assertTrue(
                        target.startswith("https://"),
                        f"{relative} links to {target!r}, which breaks on crates.io",
                    )
                self.assertNotIn("<img", text)
                self.assertIn("not crash-atomic", text)
                self.assertIn("not affiliated with, endorsed by or supported by", text.replace("\n", " ").replace("  ", " "))
                self.assertNotIn("crash-safe", text)
                self.assertNotIn("atomic uploads", text)

    def test_docs_rs_builds_the_linux_target(self):
        for manifest in (crate_version.ROOT_MANIFEST, crate_version.FFI_MANIFEST):
            with self.subTest(manifest=str(manifest)):
                metadata = tomllib.loads((REPOSITORY_ROOT / manifest).read_text(encoding="utf-8"))
                self.assertEqual(
                    metadata["package"]["metadata"]["docs"]["rs"]["targets"],
                    ["x86_64-unknown-linux-gnu"],
                )


class StampTests(unittest.TestCase):
    def setUp(self):
        self._directory = tempfile.TemporaryDirectory()
        self.addCleanup(self._directory.cleanup)
        self.root = Path(self._directory.name)
        for relative in TRACKED:
            target = self.root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(REPOSITORY_ROOT / relative, target)

    def read(self, relative: Path) -> str:
        return (self.root / relative).read_text(encoding="utf-8")

    def test_stamp_rewrites_every_place_and_check_agrees(self):
        version = crate_version.stamp(self.root, "27.3")
        self.assertEqual(version, "0.27.3")
        self.assertEqual(crate_version.check(self.root), "0.27.3")
        root_manifest = tomllib.loads(self.read(crate_version.ROOT_MANIFEST))
        ffi_manifest = tomllib.loads(self.read(crate_version.FFI_MANIFEST))
        lock = tomllib.loads(self.read(crate_version.LOCKFILE))
        self.assertEqual(root_manifest["package"]["version"], "0.27.3")
        self.assertEqual(ffi_manifest["package"]["version"], "0.27.3")
        self.assertEqual(ffi_manifest["dependencies"]["synology-drive-sync"]["version"], "=0.27.3")
        self.assertEqual(
            {entry["name"]: entry["version"] for entry in lock["package"] if entry["name"].startswith("synology-drive-sync")},
            {"synology-drive-sync": "0.27.3", "synology-drive-sync-ffi": "0.27.3"},
        )

    def test_stamp_changes_only_the_version_text(self):
        before = {relative: self.read(relative) for relative in TRACKED}
        crate_version.stamp(self.root, "26.99")
        for relative in TRACKED:
            if relative in (
                crate_version.ROOT_MANIFEST,
                crate_version.FFI_MANIFEST,
                crate_version.LOCKFILE,
            ):
                continue
            self.assertEqual(self.read(relative), before[relative], str(relative))
        # Replacing the stamped number with the original restores every byte.
        for relative in (
            crate_version.ROOT_MANIFEST,
            crate_version.FFI_MANIFEST,
            crate_version.LOCKFILE,
        ):
            original = crate_version.check(REPOSITORY_ROOT)
            self.assertEqual(self.read(relative).replace("0.26.99", original), before[relative])

    def test_stamp_is_idempotent(self):
        crate_version.stamp(self.root, "26.99")
        first = {relative: self.read(relative) for relative in TRACKED}
        crate_version.stamp(self.root, "26.99")
        self.assertEqual({relative: self.read(relative) for relative in TRACKED}, first)

    def test_stamp_refuses_a_malformed_tag_without_writing(self):
        before = {relative: self.read(relative) for relative in TRACKED}
        with self.assertRaises(crate_version.VersionError):
            crate_version.stamp(self.root, "26.0")
        self.assertEqual({relative: self.read(relative) for relative in TRACKED}, before)

    def test_check_names_every_place_when_the_versions_disagree(self):
        lock = self.read(crate_version.LOCKFILE)
        current = crate_version.check(self.root)
        (self.root / crate_version.LOCKFILE).write_text(
            lock.replace(
                f'name = "synology-drive-sync-ffi"\nversion = "{current}"',
                'name = "synology-drive-sync-ffi"\nversion = "0.1.0"',
            ),
            encoding="utf-8",
        )
        with self.assertRaises(crate_version.VersionError) as raised:
            crate_version.check(self.root)
        message = str(raised.exception)
        self.assertIn("Cargo.lock synology-drive-sync-ffi: 0.1.0", message)
        self.assertIn("crate-version.py stamp", message)

    def test_check_refuses_an_unpinned_ffi_dependency(self):
        manifest = self.read(crate_version.FFI_MANIFEST)
        current = crate_version.check(self.root)
        (self.root / crate_version.FFI_MANIFEST).write_text(
            manifest.replace(f'version = "={current}", path', f'version = "{current}", path'),
            encoding="utf-8",
        )
        with self.assertRaises(crate_version.VersionError):
            crate_version.check(self.root)

    def test_check_refuses_a_non_calendar_crate_version(self):
        crate_version_text = crate_version.check(self.root)
        for relative in (crate_version.ROOT_MANIFEST, crate_version.FFI_MANIFEST, crate_version.LOCKFILE):
            path = self.root / relative
            path.write_text(path.read_text(encoding="utf-8").replace(crate_version_text, "0.1.0"), encoding="utf-8")
        with self.assertRaisesRegex(crate_version.VersionError, "expected 0.YY.N"):
            crate_version.check(self.root)

    def test_check_refuses_a_header_or_license_copy_that_drifted(self):
        for original, copy in (
            (crate_version.HEADER, crate_version.FFI_HEADER),
            (crate_version.LICENSE, crate_version.FFI_LICENSE),
        ):
            with self.subTest(copy=str(copy)):
                path = self.root / copy
                saved = path.read_bytes()
                path.write_bytes(saved + b"\n/* drift */\n")
                with self.assertRaisesRegex(crate_version.VersionError, "differs from"):
                    crate_version.check(self.root)
                path.write_bytes(saved)
                crate_version.check(self.root)

    def test_check_refuses_publish_false_and_oversized_metadata(self):
        path = self.root / crate_version.FFI_MANIFEST
        saved = path.read_text(encoding="utf-8")
        path.write_text(saved.replace('license = "MIT"', 'license = "MIT"\npublish = false', 1), encoding="utf-8")
        with self.assertRaisesRegex(crate_version.VersionError, "publish = false"):
            crate_version.check(self.root)
        path.write_text(
            re.sub(r'(?m)^description = "[^"]*"', 'description = "' + "x" * 201 + '"', saved, count=1),
            encoding="utf-8",
        )
        with self.assertRaisesRegex(crate_version.VersionError, "201 characters"):
            crate_version.check(self.root)


if __name__ == "__main__":
    unittest.main()
