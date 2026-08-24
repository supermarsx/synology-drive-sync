from __future__ import annotations

import copy
import importlib.util
import io
import json
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path


TEST_ROOT = Path(__file__).resolve().parent
REPOSITORY_ROOT = TEST_ROOT.parents[2]
SCRIPT = TEST_ROOT.parent / "release-contract.py"
FIXTURES = TEST_ROOT / "fixtures"

SPEC = importlib.util.spec_from_file_location("release_contract", SCRIPT)
if SPEC is None or SPEC.loader is None:  # pragma: no cover - import machinery invariant
    raise RuntimeError(f"could not load {SCRIPT}")
release_contract = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release_contract)


class FixtureContractTests(unittest.TestCase):
    def fixture(self, name: str):
        return json.loads((FIXTURES / name).read_text(encoding="utf-8"))

    def test_version_selection_fixtures(self):
        for case in self.fixture("version-cases.json"):
            with self.subTest(case=case["name"]):
                if "expected" in case:
                    result = release_contract.select_version(case["input"])
                    for key, value in case["expected"].items():
                        self.assertEqual(result[key], value)
                    self.assertEqual(result["sha"], case["input"]["sha"])
                    self.assertEqual(
                        result["image"],
                        "ghcr.io/" + case["input"]["repository"].lower(),
                    )
                else:
                    with self.assertRaisesRegex(
                        release_contract.ContractError,
                        case["error_contains"],
                    ) as raised:
                        release_contract.select_version(case["input"])
                    for release_id in case.get("error_ids", []):
                        self.assertIn(f"/{release_id}", str(raised.exception))

    def test_release_state_fixtures(self):
        for case in self.fixture("release-state-cases.json"):
            with self.subTest(case=case["name"]):
                if "expected" in case:
                    self.assertEqual(
                        release_contract.resolve_release_state(case["input"]),
                        case["expected"],
                    )
                else:
                    with self.assertRaisesRegex(
                        release_contract.ContractError,
                        case["error_contains"],
                    ) as raised:
                        release_contract.resolve_release_state(case["input"])
                    for release_id in case.get("error_ids", []):
                        self.assertIn(f"/{release_id}", str(raised.exception))

    def test_publish_state_fixtures(self):
        for case in self.fixture("publish-state-cases.json"):
            with self.subTest(case=case["name"]):
                if "expected" in case:
                    self.assertEqual(
                        release_contract.resolve_publish_state(case["input"]),
                        case["expected"],
                    )
                else:
                    with self.assertRaisesRegex(
                        release_contract.ContractError,
                        case["error_contains"],
                    ) as raised:
                        release_contract.resolve_publish_state(case["input"])
                    for release_id in case.get("error_ids", []):
                        self.assertIn(f"/{release_id}", str(raised.exception))

    def test_publish_state_requires_a_positive_integer_staged_id(self):
        base = self.fixture("publish-state-cases.json")[0]["input"]
        for invalid in (None, True, 0, -1, "4242"):
            with self.subTest(expected_release_id=invalid):
                document = {**base, "expected_release_id": invalid}
                with self.assertRaisesRegex(
                    release_contract.ContractError,
                    "expected_release_id must be a positive integer",
                ):
                    release_contract.resolve_publish_state(document)

    def test_malformed_api_json_fails_closed_at_cli_boundary(self):
        completed = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "select-version",
                "--input",
                str(FIXTURES / "malformed-api.json"),
            ],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 2)
        self.assertIn("could not parse JSON", completed.stderr)
        self.assertEqual(completed.stdout, "")


class DraftPayloadTests(unittest.TestCase):
    def test_retry_payload_is_byte_for_byte_deterministic(self):
        sha = "a" * 40
        notes = "# Changes\n\n- deterministic retry\n"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            notes_path = root / "notes.md"
            first = root / "first.json"
            second = root / "second.json"
            notes_path.write_text(notes, encoding="utf-8", newline="\n")
            for output in (first, second):
                completed = subprocess.run(
                    [
                        sys.executable,
                        str(SCRIPT),
                        "draft-payload",
                        "--tag",
                        "26.7",
                        "--sha",
                        sha,
                        "--notes",
                        str(notes_path),
                        "--output",
                        str(output),
                    ],
                    text=True,
                    capture_output=True,
                    check=False,
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            self.assertEqual(
                json.loads(first.read_text(encoding="utf-8")),
                {
                    "tag_name": "26.7",
                    "target_commitish": sha,
                    "name": "synology-drive-sync 26.7",
                    "body": notes,
                    "draft": True,
                    "prerelease": False,
                },
            )

    def test_terminal_newline_is_preserved_once_without_masking_body_changes(self):
        notes = "# Notes\n\nExact body.\n"
        payload = release_contract.draft_payload("26.7", "a" * 40, notes)
        self.assertEqual(payload["body"], notes)
        self.assertNotEqual(payload["body"], notes + "\n")


class ImageIndexContractTests(unittest.TestCase):
    def index(self):
        return {
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "sha256:" + "a" * 64,
                    "size": 123,
                    "platform": {"os": "linux", "architecture": "amd64"},
                },
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "sha256:" + "b" * 64,
                    "size": 45,
                    "platform": {"os": "unknown", "architecture": "unknown"},
                },
                {
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": "sha256:" + "c" * 64,
                    "size": 124,
                    "platform": {"os": "linux", "architecture": "arm64"},
                },
            ],
        }

    def test_exact_runtime_platform_set_accepts_attestation_descriptors(self):
        release_contract.verify_image_index(self.index())

    def test_wrong_duplicate_or_half_unknown_platforms_fail_closed(self):
        cases = {}

        missing = self.index()
        missing["manifests"].pop()
        cases["missing runtime"] = (missing, "runtime platforms")

        duplicate = self.index()
        duplicate["manifests"][2]["digest"] = duplicate["manifests"][0]["digest"]
        cases["duplicate digest"] = (duplicate, "duplicate digest")

        half_unknown = self.index()
        half_unknown["manifests"][1]["platform"]["architecture"] = "amd64"
        cases["half unknown"] = (half_unknown, "runtime pair or unknown/unknown")

        for name, (document, error) in cases.items():
            with self.subTest(name=name):
                with self.assertRaisesRegex(release_contract.ContractError, error):
                    release_contract.verify_image_index(document)


class AssetContractTests(unittest.TestCase):
    TAG = "26.7"

    EXACT_ASSETS = sorted(
        [
            "SHA256SUMS",
            "THIRD_PARTY_LICENSES.html",
            "install.ps1",
            "install.sh",
            "synology-drive-sync-26.7-armv8.spk",
            "synology-drive-sync-26.7-c-sdk-linux-aarch64.tar.gz",
            "synology-drive-sync-26.7-c-sdk-linux-x86_64.tar.gz",
            "synology-drive-sync-26.7-c-sdk-macos-aarch64.tar.gz",
            "synology-drive-sync-26.7-c-sdk-macos-x86_64.tar.gz",
            "synology-drive-sync-26.7-c-sdk-windows-aarch64.zip",
            "synology-drive-sync-26.7-c-sdk-windows-x86_64.zip",
            "synology-drive-sync-26.7-linux-aarch64.tar.gz",
            "synology-drive-sync-26.7-linux-x86_64.tar.gz",
            "synology-drive-sync-26.7-macos-aarch64.tar.gz",
            "synology-drive-sync-26.7-macos-x86_64.tar.gz",
            "synology-drive-sync-26.7-rust-sdk.tar.gz",
            "synology-drive-sync-26.7-windows-aarch64.zip",
            "synology-drive-sync-26.7-windows-x86_64.zip",
            "synology-drive-sync-26.7-x86_64.spk",
            "synology-drive-sync-26.7.cdx.json",
        ]
    )

    def make_payload(self, root: Path):
        for name in release_contract.payload_names(self.TAG):
            (root / name).write_bytes(f"fixture:{name}\n".encode())

    def make_sdk_archives(self, root: Path):
        for archive_name, members in release_contract.sdk_archive_specs(self.TAG).items():
            path = root / archive_name
            if archive_name.endswith(".zip"):
                with zipfile.ZipFile(path, "w") as archive:
                    for member in members:
                        archive.writestr(member, f"fixture:{member}\n")
            else:
                with tarfile.open(path, "w:gz") as archive:
                    for member in members:
                        content = f"fixture:{member}\n".encode()
                        info = tarfile.TarInfo(member)
                        info.size = len(content)
                        archive.addfile(info, io.BytesIO(content))

    def test_exact_assets_and_checksum_membership(self):
        self.assertEqual(release_contract.release_asset_names(self.TAG), self.EXACT_ASSETS)
        self.assertEqual(len(release_contract.archive_names(self.TAG)), 15)
        self.assertEqual(len(release_contract.payload_names(self.TAG)), 19)
        self.assertEqual(len(release_contract.release_asset_names(self.TAG)), 20)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.make_payload(root)
            release_contract.prepare_assets(root, self.TAG)
            self.assertEqual(
                len((root / "ARCHIVE_SHA256SUMS").read_text().splitlines()),
                15,
            )
            self.assertEqual(len((root / "SHA256SUMS").read_text().splitlines()), 19)
            release_contract.verify_assets(root, self.TAG, include_archive_manifest=True)
            (root / "ARCHIVE_SHA256SUMS").unlink()
            release_contract.verify_assets(root, self.TAG)
            release_contract.verify_asset_names(self.TAG, self.EXACT_ASSETS)

    def test_missing_extra_and_tampered_assets_fail_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.make_payload(root)
            missing = root / release_contract.payload_names(self.TAG)[0]
            missing.unlink()
            with self.assertRaisesRegex(release_contract.ContractError, "missing"):
                release_contract.prepare_assets(root, self.TAG)

            missing.write_bytes(b"restored")
            (root / "unexpected.bin").write_bytes(b"unexpected")
            with self.assertRaisesRegex(release_contract.ContractError, "unexpected"):
                release_contract.prepare_assets(root, self.TAG)
            (root / "unexpected.bin").unlink()

            release_contract.prepare_assets(root, self.TAG)
            (root / "ARCHIVE_SHA256SUMS").unlink()
            (root / release_contract.payload_names(self.TAG)[0]).write_bytes(b"tampered")
            with self.assertRaisesRegex(release_contract.ContractError, "digest mismatch"):
                release_contract.verify_assets(root, self.TAG)

    def test_duplicate_remote_name_fails_closed(self):
        names = [*self.EXACT_ASSETS, self.EXACT_ASSETS[0]]
        with self.assertRaisesRegex(release_contract.ContractError, "duplicate asset names"):
            release_contract.verify_asset_names(self.TAG, names)

    def test_remote_assets_match_uploaded_state_size_and_sha256(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.make_payload(root)
            release_contract.prepare_assets(root, self.TAG)
            (root / "ARCHIVE_SHA256SUMS").unlink()
            local_index = release_contract.asset_index(root, self.TAG)
            remote = [
                {
                    "name": record["name"],
                    "state": "uploaded",
                    "size": record["size"],
                    "digest": record["digest"],
                    "id": index + 1,
                }
                for index, record in enumerate(local_index["assets"])
            ]
            release_contract.verify_remote_assets(remote, self.TAG, directory=root)
            release_contract.verify_remote_assets(
                remote,
                self.TAG,
                index_document=local_index,
            )

            mutations = {
                "missing digest": ("digest", None, "missing or malformed"),
                "malformed digest": ("digest", "sha256:nope", "missing or malformed"),
                "wrong digest": ("digest", "sha256:" + "0" * 64, "digest is"),
                "wrong size": ("size", remote[0]["size"] + 1, "byte size is"),
                "not uploaded": ("state", "new", "expected 'uploaded'"),
            }
            for name, (field, value, error) in mutations.items():
                with self.subTest(name=name):
                    changed = copy.deepcopy(remote)
                    changed[0][field] = value
                    with self.assertRaisesRegex(release_contract.ContractError, error):
                        release_contract.verify_remote_assets(
                            changed,
                            self.TAG,
                            index_document=local_index,
                        )

    def test_all_six_c_sdk_archives_have_exact_internal_membership(self):
        specs = release_contract.sdk_archive_specs(self.TAG)
        self.assertEqual(len(specs), 6)
        for members in specs.values():
            self.assertIn(
                next(name for name in members if name.endswith("/examples/ffi/basic.c")),
                members,
            )
            self.assertTrue(any(name.endswith("/include/sdsync.h") for name in members))
            self.assertTrue(any("/lib/" in name for name in members))

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.make_sdk_archives(root)
            release_contract.verify_sdk_archives(root, self.TAG)

            missing = root / sorted(specs)[0]
            missing.unlink()
            with self.assertRaisesRegex(release_contract.ContractError, "archive is missing"):
                release_contract.verify_sdk_archives(root, self.TAG)

            self.make_sdk_archives(root)
            windows_archive = root / next(
                name for name in specs if name.endswith("windows-x86_64.zip")
            )
            with zipfile.ZipFile(windows_archive, "a") as archive:
                archive.writestr("unexpected.pdb", b"must not ship")
            with self.assertRaisesRegex(release_contract.ContractError, "unexpected"):
                release_contract.verify_sdk_archives(root, self.TAG)


class WorkflowWiringTests(unittest.TestCase):
    def test_release_precedes_ghcr_and_contract_helper_is_wired(self):
        workflow = (REPOSITORY_ROOT / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("workflow_run:\n    workflows: [CI]\n    types: [completed]", workflow)
        self.assertIn("github.event.workflow_run.conclusion == 'success'", workflow)
        self.assertIn("github.event.workflow_run.event == 'push'", workflow)
        self.assertIn("github.event.workflow_run.head_branch == 'main'", workflow)
        self.assertIn("github.event.workflow_run.head_repository.full_name == github.repository", workflow)
        self.assertIn("group: calendar-release", workflow)
        self.assertIn("cancel-in-progress: false", workflow)
        publish_release = workflow.index("\n  publish-release:")
        publish_image = workflow.index("\n  publish-image:")
        self.assertLess(publish_release, publish_image)
        image_block = workflow[publish_image:]
        self.assertRegex(
            image_block,
            r"needs: \[[^\]]*publish-release[^\]]*\]",
        )
        self.assertIn("release-contract.py select-version", workflow)
        self.assertIn("release-contract.py resolve-state", workflow)
        self.assertIn("release-contract.py resolve-publish-state", workflow)
        self.assertIn("expected_release_id: $expected_release_id", workflow)
        self.assertIn('if [[ "$published" == false ]]; then', workflow)
        self.assertIn("release-contract.py prepare-assets", workflow)
        self.assertIn("release-contract.py verify-sdk-archives", workflow)
        self.assertIn("release-contract.py verify-remote-assets", workflow)
        self.assertGreaterEqual(workflow.count("verify-remote-assets"), 2)
        self.assertIn("name: staged-release-contract", workflow)

    def test_workspace_sbom_is_hierarchically_merged_and_validated(self):
        workflow = (REPOSITORY_ROOT / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn('root_source_sbom="synology-drive-sync.cdx.json"', workflow)
        self.assertIn(
            'ffi_source_sbom="crates/synology-drive-sync-ffi/'
            'synology-drive-sync-ffi.cdx.json"',
            workflow,
        )
        self.assertIn("cyclonedx-cli/releases/download/v0.33.1/cyclonedx-linux-x64", workflow)
        self.assertIn(
            "bfc8b2538da86fe239bc53658bbb63c1c8c510a293c1e6891aa5bea5d3c58746",
            workflow,
        )
        self.assertIn('"$cyclonedx_cli" merge', workflow)
        self.assertIn("--hierarchical", workflow)
        self.assertIn("--output-version v1_5", workflow)
        self.assertIn('"$cyclonedx_cli" validate', workflow)
        self.assertIn("--fail-on-errors", workflow)
        self.assertIn('any($components[]; .name == "synology-drive-sync-ffi")', workflow)
        self.assertNotIn("source_sbom=$(find", workflow)

    def test_ghcr_version_tag_is_immutable_and_both_tags_are_exact(self):
        workflow = (REPOSITORY_ROOT / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("imagetools create --dry-run", workflow)
        self.assertIn('imagetools inspect --raw "$IMAGE:$TAG"', workflow)
        self.assertIn("refusing to overwrite the immutable version tag", workflow)
        self.assertIn('for remote_tag in "$TAG" latest; do', workflow)
        self.assertIn("--format '{{.Manifest.Digest}}'", workflow)
        self.assertGreaterEqual(workflow.count("verify-image-index"), 3)
        self.assertGreaterEqual(workflow.count('cmp --silent "$expected_index"'), 2)

    def test_sdk_profiles_paths_and_retry_body_comparison_are_exact(self):
        workflow = (REPOSITORY_ROOT / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        self.assertGreaterEqual(
            workflow.count(
                "cargo build -p synology-drive-sync-ffi --profile ffi-release --locked"
            ),
            2,
        )
        self.assertNotIn("cargo build -p synology-drive-sync-ffi --release", workflow)
        self.assertIn("target/$TARGET/ffi-release/libsdsync.so", workflow)
        self.assertIn("target/$TARGET/ffi-release/libsdsync.dylib", workflow)
        self.assertIn('target/$env:TARGET/ffi-release', workflow)
        self.assertIn("sdsync.dll.lib", workflow)
        self.assertIn('$sdk_package/examples/ffi/basic.c', workflow)
        self.assertIn("Join-Path $sdkPackage 'examples/ffi'", workflow)
        self.assertEqual(workflow.count("name: Compile, link and load the C SDK example"), 2)
        self.assertIn("LD_LIBRARY_PATH=\"$library_dir\"", workflow)
        self.assertIn("DYLD_LIBRARY_PATH=\"$library_dir\"", workflow)
        self.assertIn("& cl.exe @compileArgs", workflow)
        self.assertIn('target/$TARGET/ffi-release/libsdsync.so', workflow)
        self.assertIn("release ceiling is GLIBC_2.35", workflow)
        self.assertIn("--prefix=\"synology-drive-sync-$TAG-rust-sdk/\"", workflow)
        self.assertIn("jq -e --rawfile notes \"$notes\" '.body == $notes'", workflow)
        self.assertNotIn("jq -r '.body'", workflow)
        self.assertNotIn("jq -er '.already_published'", workflow)
        self.assertNotIn("jq -er '.tag_matches'", workflow)

    def test_native_ci_and_coverage_keep_workspace_scope_and_threshold(self):
        ci = (REPOSITORY_ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn("cargo clippy --locked --workspace --all-targets", ci)
        self.assertIn("cargo test --locked --workspace --all-targets", ci)
        self.assertIn("cargo build --release --locked -p synology-drive-sync", ci)
        self.assertIn(
            "cargo build --profile ffi-release --locked\n"
            "          -p synology-drive-sync-ffi",
            ci,
        )
        self.assertNotIn("cargo build --release --locked --workspace", ci)
        coverage = (REPOSITORY_ROOT / "scripts/coverage.sh").read_text(encoding="utf-8")
        self.assertIn("llvm-cov --locked --workspace --all-targets --no-report", coverage)
        coverage_env = (REPOSITORY_ROOT / ".config/coverage.env").read_text(
            encoding="utf-8"
        )
        self.assertIn("COVERAGE_MIN_LINES=90", coverage_env.splitlines())


if __name__ == "__main__":
    unittest.main()
