from __future__ import annotations

import copy
import importlib.util
import io
import json
import re
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import unittest
import zipfile
from pathlib import Path


TEST_ROOT = Path(__file__).resolve().parent
REPOSITORY_ROOT = TEST_ROOT.parents[2]
SCRIPT = TEST_ROOT.parent / "release-contract.py"
FIXTURES = TEST_ROOT / "fixtures"
NOTICE_SCRIPT = TEST_ROOT.parent / "verify-third-party-notices.py"

SPEC = importlib.util.spec_from_file_location("release_contract", SCRIPT)
if SPEC is None or SPEC.loader is None:  # pragma: no cover - import machinery invariant
    raise RuntimeError(f"could not load {SCRIPT}")
release_contract = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release_contract)

NOTICE_SPEC = importlib.util.spec_from_file_location(
    "verify_third_party_notices", NOTICE_SCRIPT
)
if NOTICE_SPEC is None or NOTICE_SPEC.loader is None:  # pragma: no cover
    raise RuntimeError(f"could not load {NOTICE_SCRIPT}")
verify_third_party_notices = importlib.util.module_from_spec(NOTICE_SPEC)
NOTICE_SPEC.loader.exec_module(verify_third_party_notices)


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

    def test_semantically_identical_indexes_ignore_json_serialization(self):
        expected = self.index()
        actual = json.loads(json.dumps(expected, sort_keys=True, separators=(",", ":")))
        release_contract.verify_image_index_match(expected, actual)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            expected_path = root / "expected.json"
            actual_path = root / "actual.json"
            expected_path.write_text(
                json.dumps(expected, indent=2) + "\n",
                encoding="utf-8",
                newline="\n",
            )
            actual_path.write_text(
                json.dumps(actual, sort_keys=True, separators=(",", ":")),
                encoding="utf-8",
                newline="\n",
            )
            completed = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "verify-image-index-match",
                    "--expected",
                    str(expected_path),
                    "--actual",
                    str(actual_path),
                ],
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_index_match_rejects_any_child_descriptor_change(self):
        expected = self.index()
        cases = {}

        changed_digest = copy.deepcopy(expected)
        changed_digest["manifests"][0]["digest"] = "sha256:" + "d" * 64
        cases["digest"] = changed_digest

        changed_size = copy.deepcopy(expected)
        changed_size["manifests"][0]["size"] += 1
        cases["size"] = changed_size

        changed_annotations = copy.deepcopy(expected)
        changed_annotations["manifests"][1]["annotations"] = {
            "vnd.docker.reference.type": "attestation-manifest"
        }
        cases["annotations"] = changed_annotations

        reordered = copy.deepcopy(expected)
        reordered["manifests"][0], reordered["manifests"][2] = (
            reordered["manifests"][2],
            reordered["manifests"][0],
        )
        cases["order"] = reordered

        for name, actual in cases.items():
            with self.subTest(name=name):
                with self.assertRaisesRegex(
                    release_contract.ContractError,
                    "content differs from the exact expected index",
                ):
                    release_contract.verify_image_index_match(expected, actual)

    def test_index_match_distinguishes_json_boolean_from_integer(self):
        expected = self.index()
        expected["annotations"] = {"test.example/generation": 1}
        actual = copy.deepcopy(expected)
        actual["annotations"]["test.example/generation"] = True

        with self.assertRaisesRegex(
            release_contract.ContractError,
            "content differs from the exact expected index",
        ):
            release_contract.verify_image_index_match(expected, actual)

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
            "synology-drive-sync-26.7-armv7.spk",
            "synology-drive-sync-26.7-armv8.spk",
            "synology-drive-sync-26.7-c-sdk-linux-aarch64.tar.gz",
            "synology-drive-sync-26.7-c-sdk-linux-x86_64.tar.gz",
            "synology-drive-sync-26.7-c-sdk-macos-aarch64.tar.gz",
            "synology-drive-sync-26.7-c-sdk-macos-x86_64.tar.gz",
            "synology-drive-sync-26.7-c-sdk-windows-aarch64.zip",
            "synology-drive-sync-26.7-c-sdk-windows-x86_64.zip",
            "synology-drive-sync-26.7-i686.spk",
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
        self.assertEqual(
            release_contract.SYNOLOGY_ARCHITECTURES,
            ("armv7", "armv8", "i686", "x86_64"),
        )
        self.assertEqual(release_contract.release_asset_names(self.TAG), self.EXACT_ASSETS)
        self.assertEqual(len(release_contract.archive_names(self.TAG)), 17)
        self.assertEqual(len(release_contract.payload_names(self.TAG)), 21)
        self.assertEqual(len(release_contract.release_asset_names(self.TAG)), 22)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.make_payload(root)
            release_contract.prepare_assets(root, self.TAG)
            self.assertEqual(
                len((root / "ARCHIVE_SHA256SUMS").read_text().splitlines()),
                17,
            )
            self.assertEqual(len((root / "SHA256SUMS").read_text().splitlines()), 21)
            release_contract.verify_assets(root, self.TAG, include_archive_manifest=True)
            archive_manifest = (root / "ARCHIVE_SHA256SUMS").read_text(
                encoding="utf-8"
            )
            for architecture in release_contract.SYNOLOGY_ARCHITECTURES:
                self.assertIn(
                    f"  synology-drive-sync-{self.TAG}-{architecture}.spk\n",
                    archive_manifest,
                )
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
    def test_dsm_ui_dependency_audit_is_single_ci_gate(self):
        ci = (REPOSITORY_ROOT / ".github/workflows/ci.yml").read_text(
            encoding="utf-8"
        )
        release = (REPOSITORY_ROOT / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        audit_command = "pnpm audit --audit-level high"
        self.assertEqual(ci.count(audit_command), 1)
        self.assertNotIn(audit_command, release)
        packaging_install = ci.index(
            "pnpm install --frozen-lockfile --ignore-scripts",
            ci.index("Verify deterministic native DSM AppWindow build"),
        )
        packaging_audit = ci.index(audit_command, packaging_install)
        packaging_test = ci.index("pnpm test", packaging_install)
        self.assertLess(packaging_install, packaging_audit)
        self.assertLess(packaging_audit, packaging_test)

        workspace = (
            REPOSITORY_ROOT / "packaging/synology/ui-src/pnpm-workspace.yaml"
        ).read_text(encoding="utf-8")
        lockfile = (
            REPOSITORY_ROOT / "packaging/synology/ui-src/pnpm-lock.yaml"
        ).read_text(encoding="utf-8")
        override = r"(?m)^overrides:\n  postcss: 8\.5\.26$"
        self.assertRegex(workspace, override)
        self.assertRegex(lockfile, override)
        self.assertEqual(
            set(re.findall(r"(?m)^  postcss@([^:\s]+):$", lockfile)),
            {"8.5.26"},
        )

    def test_release_container_bases_are_current_and_immutable(self):
        dockerfile = (REPOSITORY_ROOT / "Dockerfile").read_text(encoding="utf-8")
        self.assertIn(
            "FROM rust:1.88-bookworm@sha256:"
            "af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0 AS builder",
            dockerfile,
        )
        self.assertIn(
            "FROM debian:bookworm-slim@sha256:"
            "88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS runtime",
            dockerfile,
        )
        for instruction in re.findall(r"^FROM\s+([^\s]+)", dockerfile, re.MULTILINE):
            self.assertRegex(instruction, r"@sha256:[0-9a-f]{64}$")

        notice_generator = (
            REPOSITORY_ROOT / ".github/scripts/generate-third-party-notices.sh"
        ).read_text(encoding="utf-8")
        self.assertIn("version=0.9.2", notice_generator)
        self.assertIn(
            "archive_sha256="
            "9099a59e820c38a68b9d65f300662a567d56562f9a10f6aa4c7e86c17c2566af",
            notice_generator,
        )

    def test_actions_node_and_hosted_runners_are_current_and_immutable(self):
        workflow_directory = REPOSITORY_ROOT / ".github/workflows"
        workflow_paths = sorted(
            [*workflow_directory.glob("*.yml"), *workflow_directory.glob("*.yaml")]
        )
        workflows = "\n".join(
            path.read_text(encoding="utf-8") for path in workflow_paths
        )
        expected_actions = {
            "Swatinem/rust-cache": "6323deb102c322ba6fcbdcafc7e3dddab59af2b6",
            "actions/attest": "1e69f48acb82d1966a394da916b4c1698aa569d6",
            "actions/checkout": "3d3c42e5aac5ba805825da76410c181273ba90b1",
            "actions/configure-pages": "45bfe0192ca1faeb007ade9deae92b16b8254a0d",
            "actions/deploy-pages": "cd2ce8fcbc39b97be8ca5fce6e763baed58fa128",
            "actions/download-artifact": "3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c",
            "actions/setup-node": "820762786026740c76f36085b0efc47a31fe5020",
            "actions/upload-artifact": "043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
            "actions/upload-pages-artifact": "fc324d3547104276b827a68afc52ff2a11cc49c9",
            "dtolnay/rust-toolchain": "4360b52568e2003a75bf9bc1d59f33a8e3fc893c",
            "lycheeverse/lychee-action": "e7477775783ea5526144ba13e8db5eec57747ce8",
            "pnpm/action-setup": "0977fd99725f1db4007ccb2928dbb4e90d06cc86",
            "taiki-e/install-action": "0758d235715de2f3551eacc980d9ae8fce9342c3",
        }
        uses_values = re.findall(
            r"^\s*(?:-\s+)?uses:\s+([^#\s]+)(?:\s+#.*)?$",
            workflows,
            re.MULTILINE,
        )
        parsed_actions: list[tuple[str, str]] = []
        for value in uses_values:
            match = re.fullmatch(r"([^@\s]+)@([0-9a-f]{40})", value)
            self.assertIsNotNone(match, f"action is not pinned to an immutable SHA: {value}")
            assert match is not None
            parsed_actions.append((match.group(1), match.group(2)))

        self.assertEqual({name for name, _ in parsed_actions}, set(expected_actions))
        for name, sha in parsed_actions:
            self.assertEqual(sha, expected_actions[name], name)

        self.assertEqual(
            re.findall(r"^\s*node-version:\s*([^#\s]+)", workflows, re.MULTILINE),
            ["24.20.0"] * 4,
        )
        self.assertNotIn("mlugg/setup-zig", workflows)
        self.assertNotIn("ACTIONS_ALLOW_USE_UNSECURE_NODE_VERSION", workflows)
        self.assertEqual(workflows.count("tool: cargo-zigbuild@0.23.3"), 3)

        runner_labels = set(
            re.findall(r"^\s*(?:runs-on:|- runner:)\s+([^$\s][^\s]*)\s*$", workflows, re.MULTILINE)
        )
        self.assertEqual(
            runner_labels,
            {
                "macos-26",
                "macos-26-intel",
                "ubuntu-24.04",
                "ubuntu-24.04-arm",
                "windows-11-vs2026-arm",
                "windows-2025-vs2026",
            },
        )
        self.assertNotIn("ubuntu-26.04", workflows)

    def test_windows_installer_uses_ci_token_only_for_github_api(self):
        ci = (REPOSITORY_ROOT / ".github/workflows/ci.yml").read_text(
            encoding="utf-8"
        )
        installer = (REPOSITORY_ROOT / "packaging/install.ps1").read_text(
            encoding="utf-8"
        )
        step_start = ci.index(
            "      - name: Exercise non-mutating Windows release installer"
        )
        step_end = ci.index("\n\n  coverage:", step_start)
        installer_step = ci[step_start:step_end]

        self.assertIn("github.event_name == 'push'", installer_step)
        self.assertIn(
            "github.event.pull_request.head.repo.full_name == github.repository",
            installer_step,
        )
        self.assertIn("GITHUB_TOKEN: ${{ github.token }}", installer_step)
        self.assertIn(
            "[Environment]::GetEnvironmentVariable('GH_TOKEN')", installer
        )
        self.assertIn(
            "[Environment]::GetEnvironmentVariable('GITHUB_TOKEN')", installer
        )
        self.assertLess(
            installer.index("[Environment]::GetEnvironmentVariable('GH_TOKEN')"),
            installer.index("[Environment]::GetEnvironmentVariable('GITHUB_TOKEN')"),
        )
        self.assertIn("$githubToken.Length -gt 4096", installer)
        self.assertIn("$githubToken -notmatch '^[!-~]+$'", installer)
        self.assertIn(
            '$apiHeaders[\'Authorization\'] = "Bearer $githubToken"', installer
        )
        self.assertIn("$apiHeaders.Remove('Authorization')", installer)
        self.assertIn("$githubToken = $null", installer)
        self.assertIn(
            "Remove-Variable -Name githubToken -ErrorAction SilentlyContinue "
            "-WhatIf:$false -Confirm:$false",
            installer,
        )
        self.assertEqual(installer.count("Invoke-RestMethod -Headers $apiHeaders"), 1)
        self.assertEqual(installer.count("Invoke-WebRequest -Headers $headers"), 2)
        self.assertNotIn("Invoke-WebRequest -Headers $apiHeaders", installer)
        child_start = installer.index("$reportedVersionLines = @(& $candidate --version)")
        child_end = installer.index("$expectedVersion =", child_start)
        for name, previous in (
            ("GH_TOKEN", "$previousGhToken"),
            ("GITHUB_TOKEN", "$previousGithubToken"),
        ):
            clear = (
                f"[Environment]::SetEnvironmentVariable('{name}', $null, 'Process')"
            )
            restore = f"[Environment]::SetEnvironmentVariable('{name}', {previous}"
            self.assertLess(installer.index(clear), child_start)
            self.assertIn(restore, installer[child_start:child_end])

    def test_packaging_checkout_fetches_tagged_dsm_upgrade_fixtures(self):
        workflow = (REPOSITORY_ROOT / ".github/workflows/ci.yml").read_text(
            encoding="utf-8"
        )
        packaging = workflow[workflow.index("  packaging:") :]
        checkout_start = packaging.index("      - uses: actions/checkout@")
        checkout_end = packaging.index("\n      - ", checkout_start + 1)
        checkout = packaging[checkout_start:checkout_end]

        self.assertEqual(checkout.count("fetch-depth: 0"), 1)
        self.assertEqual(checkout.count("persist-credentials: false"), 1)

    def test_documentation_selector_is_a_required_rendered_gate(self):
        workflow = (REPOSITORY_ROOT / ".github/workflows/docs.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("node --check docs/theme/release-selector-data.js", workflow)
        self.assertIn("node --check docs/theme/release-selector.js", workflow)
        self.assertIn("node --test tests/release-selector.test.cjs", workflow)
        self.assertIn("test -s target/site/release-selector.html", workflow)
        self.assertIn(
            "for control in purpose model dsmVersion reportedArch desktopOs desktopCpu; do",
            workflow,
        )
        self.assertIn("grep -Fq 'data-selector-result'", workflow)
        self.assertIn("grep -Fq '&lt;fieldset'", workflow)

    def test_release_notice_targets_cover_every_native_and_dsm_binary(self):
        expected_targets = {
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
            "i686-unknown-linux-musl",
            "armv7-unknown-linux-musleabihf",
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
        }
        with (REPOSITORY_ROOT / "about.toml").open("rb") as config_file:
            configured_targets = set(tomllib.load(config_file)["targets"])
        self.assertEqual(configured_targets, expected_targets)
        self.assertEqual(
            verify_third_party_notices.REQUIRED_TARGETS,
            expected_targets,
        )

    def test_synology_matrix_is_dsm7_complete_pinned_and_emulated(self):
        ci = (REPOSITORY_ROOT / ".github/workflows/ci.yml").read_text(
            encoding="utf-8"
        )
        release = (REPOSITORY_ROOT / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        sections = (
            ci[ci.index("  synology-package:") : ci.index("\n  packaging:")],
            release[
                release.index("  build-synology:") : release.index("\n  sbom:")
            ],
        )
        targets = {
            "x86_64": "x86_64-unknown-linux-musl",
            "armv8": "aarch64-unknown-linux-musl",
            "i686": "i686-unknown-linux-musl",
            "armv7": "armv7-unknown-linux-musleabihf",
        }
        zig_installer = "run: bash .github/scripts/install-zig.sh"
        installer_action = (
            "taiki-e/install-action@0758d235715de2f3551eacc980d9ae8fce9342c3"
        )
        qemu_image = (
            "docker.io/tonistiigi/binfmt:qemu-v10.2.3@sha256:"
            "400a4873b838d1b89194d982c45e5fb3cda4593fbfd7e08a02e76b03b21166f0"
        )
        alpine_image = (
            "docker.io/library/alpine:3.24.1@sha256:"
            "28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b"
        )

        for section in sections:
            for architecture, target in targets.items():
                self.assertEqual(section.count(f"synology_arch: {architecture}"), 1)
                self.assertEqual(section.count(f"rust_target: {target}"), 1)
            self.assertEqual(section.count("cross_compile: true"), 2)
            self.assertEqual(section.count("cross_compile: false"), 2)
            self.assertIn(zig_installer, section)
            self.assertNotIn("setup-zig", section)
            self.assertIn(installer_action, section)
            self.assertIn("tool: cargo-zigbuild@0.23.3", section)
            self.assertIn(qemu_image, section)
            self.assertIn(alpine_image, section)
            self.assertIn("qemu_binary: qemu-i386", section)
            self.assertIn("qemu_binary: qemu-arm", section)
            self.assertIn("qemu_cpu: qemu32", section)
            self.assertIn("qemu_cpu: cortex-a9,neon=off,vfp-d32=off", section)
            self.assertIn("cargo zigbuild --release --locked", section)
            self.assertIn(
                "--bin synology-drive-sync --bin sdsync-dsm-api", section
            )
            self.assertIn(
                'api_binary="target/$RUST_TARGET/release/sdsync-dsm-api"', section
            )
            self.assertEqual(section.count('--api-binary "$api_binary"'), 2)
            self.assertIn("QEMU_CPU=\"$QEMU_CPU\"", section)
            self.assertIn(
                '"$emulator" "$binary" synology-drive-sync --version', section
            )
            self.assertIn(
                'REQUEST_METHOD=GET QUERY_STRING=action=csrf '
                'HTTP_X_SDSYNC_REQUEST=1 QEMU_CPU="$QEMU_CPU" "$emulator" '
                '"$api_binary" sdsync-dsm-api',
                section,
            )
            self.assertIn(
                'REQUEST_METHOD=GET QUERY_STRING=action=csrf '
                'HTTP_X_SDSYNC_REQUEST=1 "$api_binary"',
                section,
            )
            self.assertEqual(section.count('[[ "$bridge_exit" -eq 0 ]]'), 1)
            self.assertNotIn('[[ "$bridge_exit" -ne 0 ]]', section)
            self.assertIn(
                "grep -Fq 'Status: 200 OK' <<<\"$bridge_output\"", section
            )
            self.assertIn('"schema":"sdsync.dsm-error.v1"', section)
            self.assertIn('"status":401', section)
            self.assertIn('"code":"unauthorized"', section)
            self.assertNotIn("Status: 401 Unauthorized", section)
            self.assertIn("Version5 EABI", section)
            self.assertIn("hard-float ABI", section)
            self.assertIn("elf_class: ELF32", section)
            self.assertNotIn("armv5", section.lower())
            self.assertNotIn("powerpc", section.lower())

        self.assertIn(
            "for synology_arch in armv7 armv8 i686 x86_64; do", release
        )

        zig_script = (REPOSITORY_ROOT / ".github/scripts/install-zig.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn('readonly zig_version="0.16.0"', zig_script)
        self.assertIn(
            "70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00",
            zig_script,
        )
        self.assertIn(
            "ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17",
            zig_script,
        )

    def test_ui_computed_browser_gate_is_authoritative_once(self):
        ci = (REPOSITORY_ROOT / ".github/workflows/ci.yml").read_text(
            encoding="utf-8"
        )
        release = (REPOSITORY_ROOT / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        ci_synology = ci[
            ci.index("  synology-package:") : ci.index("\n  packaging:")
        ]
        ci_packaging = ci[ci.index("  packaging:") :]
        release_synology = release[
            release.index("  build-synology:") : release.index("\n  sbom:")
        ]

        reviewed_skip = (
            "SDSYNC_UI_BROWSER_GATE: skip-synology-architecture-matrix"
        )
        required = "SDSYNC_UI_BROWSER_GATE: required"
        self.assertEqual(ci_synology.count(reviewed_skip), 1)
        self.assertEqual(release_synology.count(reviewed_skip), 1)
        self.assertNotIn(required, ci_synology)
        self.assertNotIn(required, release_synology)
        self.assertEqual(ci_packaging.count(required), 1)
        self.assertNotIn(reviewed_skip, ci_packaging)

    def test_release_precedes_ghcr_and_contract_helper_is_wired(self):
        workflow = (REPOSITORY_ROOT / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("workflow_run:\n    workflows: [CI]\n    types: [completed]", workflow)
        self.assertIn("github.event.workflow_run.conclusion == 'success'", workflow)
        self.assertIn("github.event.workflow_run.event == 'push'", workflow)
        self.assertIn("github.event.workflow_run.head_branch == 'main'", workflow)
        self.assertIn("github.event.workflow_run.head_repository.full_name == github.repository", workflow)
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

    def test_release_concurrency_isolates_ineligible_workflow_completions(self):
        workflow = (REPOSITORY_ROOT / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        concurrency = workflow[
            workflow.index("concurrency:\n") : workflow.index("\npermissions:")
        ]

        self.assertIn("github.event_name == 'workflow_dispatch'", concurrency)
        self.assertIn("github.event_name == 'workflow_run'", concurrency)
        self.assertIn(
            "github.event.workflow_run.conclusion == 'success'", concurrency
        )
        self.assertIn("github.event.workflow_run.event == 'push'", concurrency)
        self.assertIn("github.event.workflow_run.head_branch == 'main'", concurrency)
        self.assertIn(
            "github.event.workflow_run.head_repository.full_name == github.repository",
            concurrency,
        )
        self.assertIn("&& 'calendar-release'", concurrency)
        self.assertIn(
            "|| format('calendar-release-ineligible-{0}', github.run_id)",
            concurrency,
        )
        self.assertIn("cancel-in-progress: false", concurrency)
        self.assertNotIn("group: calendar-release", concurrency)

    def test_stage_retries_eventually_consistent_listing_by_immutable_id(self):
        workflow = (REPOSITORY_ROOT / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        start = workflow.index("name: Create or resume the draft release")
        end = workflow.index("name: Upload and verify draft assets", start)
        stage = workflow[start:end]

        self.assertIn("max_visibility_attempts=6", stage)
        self.assertIn(
            "visibility_delay_seconds=$((visibility_delay_seconds * 2))", stage
        )
        self.assertIn("any(.[]; .id == $id)", stage)
        self.assertIn("any(.[]; .tag_name == $tag)", stage)
        self.assertIn("unexpected immutable state $conflicting_releases", stage)
        self.assertIn("expected_release_id: $expected_release_id,", stage)
        self.assertIn("release-contract.py resolve-publish-state", stage)
        self.assertIn("jq -e '.published | type == \"boolean\"'", stage)
        self.assertIn("resolved_published=$(jq -r '.published'", stage)
        self.assertNotIn("resolved_published=$(jq -er '.published'", stage)
        self.assertIn(
            "was still absent from the releases collection after "
            "$max_visibility_attempts attempts",
            stage,
        )

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
        self.assertGreaterEqual(
            workflow.count("release-contract.py verify-image-index-match"), 2
        )
        self.assertIn('[[ "$remote_digest" == "$version_digest" ]]', workflow)
        self.assertIn('"$IMAGE@$version_digest"', workflow)
        self.assertNotIn('sha256sum "$expected_index"', workflow)
        self.assertNotIn('sha256sum "$remote_index"', workflow)
        self.assertNotIn('sha256sum "$version_index"', workflow)
        self.assertNotIn('cmp --silent "$expected_index"', workflow)

        existing_start = workflow.index('if [[ "$version_exists" == true ]]; then')
        fresh_start = workflow.index("\n          else", existing_start)
        existing_branch = workflow[existing_start:fresh_start]
        self.assertLess(
            existing_branch.index("release-contract.py verify-image-index-match"),
            existing_branch.index("docker buildx imagetools create"),
        )
        self.assertNotIn('--tag "$IMAGE:$TAG"', existing_branch)

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
        self.assertEqual(workflow.count('"$TARGET.2.35"'), 2)
        self.assertIn("cargo zigbuild --release --locked", workflow)
        self.assertIn(
            "cargo zigbuild -p synology-drive-sync-ffi --profile ffi-release --locked",
            workflow,
        )
        self.assertEqual(
            workflow.count('export MACOSX_DEPLOYMENT_TARGET="$MACOS_DEPLOYMENT_TARGET"'),
            2,
        )
        self.assertIn('macos_deployment_target: "10.12"', workflow)
        self.assertIn('macos_deployment_target: "11.0"', workflow)
        self.assertEqual(workflow.count("tool: cargo-zigbuild@0.23.3"), 2)
        self.assertIn("name: Verify emitted macOS deployment floors", workflow)
        self.assertIn('build_metadata=$(xcrun vtool -show-build "$artifact")', workflow)
        self.assertIn('[[ "$minos" == "$MACOS_DEPLOYMENT_TARGET" ]]', workflow)
        self.assertIn("--prefix=\"synology-drive-sync-$TAG-rust-sdk/\"", workflow)
        self.assertIn("jq -e --rawfile notes \"$notes\" '.body == $notes'", workflow)
        self.assertNotIn("jq -r '.body'", workflow)
        self.assertNotIn("jq -er '.already_published'", workflow)
        self.assertNotIn("jq -er '.tag_matches'", workflow)

    def test_native_ci_and_coverage_keep_workspace_scope_and_split_thresholds(self):
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
        self.assertIn("Coverage (90% non-DSM / 74% DSM line gates)", ci)
        self.assertIn("Enforce split repository line coverage", ci)
        self.assertIn("Test release and coverage policy contracts", ci)
        self.assertIn("-p 'test_*.py' -v", ci)
        coverage = (REPOSITORY_ROOT / "scripts/coverage.sh").read_text(encoding="utf-8")
        self.assertIn("llvm-cov --locked --workspace --all-targets --no-report", coverage)
        self.assertIn('python3 "$repo_root/scripts/coverage_policy.py"', coverage)
        self.assertIn('--minimum-general "$minimum_lines"', coverage)
        self.assertIn('--minimum-dsm "$dsm_minimum_lines"', coverage)
        self.assertIn("enforcement_arguments=(--enforce)", coverage)
        self.assertIn("validate_coverage_summary true", coverage)
        self.assertIn("Duplicate coverage setting: $key", coverage)
        self.assertNotIn("--ignore-filename-regex", coverage)
        self.assertNotIn("--fail-under-lines", coverage)
        powershell_coverage = (REPOSITORY_ROOT / "scripts/coverage.ps1").read_text(
            encoding="utf-8"
        )
        self.assertIn("'coverage_policy.py'", powershell_coverage)
        self.assertIn("'--minimum-general', $GeneralMinimum", powershell_coverage)
        self.assertIn("'--minimum-dsm', $DsmMinimum", powershell_coverage)
        self.assertIn("$arguments += '--enforce'", powershell_coverage)
        self.assertIn("-EnforceThresholds $true", powershell_coverage)
        self.assertIn("Duplicate coverage setting: $($Matches[1])", powershell_coverage)
        self.assertNotIn("--ignore-filename-regex", powershell_coverage)
        self.assertNotIn("--fail-under-lines", powershell_coverage)
        policy = (REPOSITORY_ROOT / "scripts/coverage_policy.py").read_text(
            encoding="utf-8"
        )
        self.assertIn('Path("src/dsm_api.rs")', policy)
        self.assertIn('Path("src/bin/sdsync-dsm-api.rs")', policy)
        self.assertIn("seen_dsm_paths != expected_dsm_paths", policy)
        self.assertIn("total_count != dsm_count + general_count", policy)
        self.assertIn("total_covered != dsm_covered + general_covered", policy)
        self.assertIn(
            "metrics.general_covered * 100 < general_minimum * metrics.general_count",
            policy,
        )
        self.assertIn(
            "metrics.dsm_covered * 100 < dsm_minimum * metrics.dsm_count",
            policy,
        )
        coverage_env = (REPOSITORY_ROOT / ".config/coverage.env").read_text(
            encoding="utf-8"
        )
        self.assertEqual(
            coverage_env.splitlines(),
            [
                "RUST_TOOLCHAIN=1.88.0",
                "CARGO_LLVM_COV_VERSION=0.9.0",
                "COVERAGE_MIN_LINES=90",
                "COVERAGE_DSM_MIN_LINES=74",
            ],
        )
        coverage_docs = (REPOSITORY_ROOT / "docs/testing.md").read_text(
            encoding="utf-8"
        )
        self.assertIn("**90% minimum**", coverage_docs)
        self.assertIn("**74% minimum**", coverage_docs)
        self.assertIn("src/dsm_api.rs", coverage_docs)
        self.assertIn("src/bin/sdsync-dsm-api.rs", coverage_docs)


if __name__ == "__main__":
    unittest.main()
