import copy
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[3]
POLICY_PATH = REPOSITORY_ROOT / "scripts" / "coverage_policy.py"
SPEC = importlib.util.spec_from_file_location("coverage_policy", POLICY_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load coverage policy module")
coverage_policy = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = coverage_policy
SPEC.loader.exec_module(coverage_policy)


def coverage_file(path: Path, covered: object, count: object) -> dict:
    return {
        "filename": str(path.resolve()),
        "summary": {"lines": {"covered": covered, "count": count}},
    }


def valid_document() -> dict:
    files = [
        coverage_file(REPOSITORY_ROOT / "src" / "dsm_api.rs", 74, 99),
        coverage_file(
            REPOSITORY_ROOT / "src" / "bin" / "sdsync-dsm-api.rs", 0, 1
        ),
        coverage_file(REPOSITORY_ROOT / "src" / "api.rs", 90, 100),
    ]
    return {
        "type": "llvm.coverage.json.export",
        "data": [
            {
                "files": files,
                "totals": {"lines": {"covered": 164, "count": 200}},
            }
        ],
    }


class CoveragePolicyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary_directory.cleanup)
        self.summary_path = Path(self.temporary_directory.name) / "coverage.json"

    def validate(self, document: object):
        self.summary_path.write_text(json.dumps(document), encoding="utf-8")
        return coverage_policy.validate_summary(self.summary_path, REPOSITORY_ROOT)

    def assert_invalid(self, document: object) -> None:
        with self.assertRaises(coverage_policy.CoveragePolicyError):
            self.validate(document)

    def test_exact_partitions_pass_and_each_floor_is_enforced(self):
        metrics = self.validate(valid_document())
        self.assertEqual((metrics.general_covered, metrics.general_count), (90, 100))
        self.assertEqual((metrics.dsm_covered, metrics.dsm_count), (74, 100))
        self.assertEqual(coverage_policy.enforce_thresholds(metrics, 90, 74), [])

        general_below = valid_document()
        general_below["data"][0]["files"][2]["summary"]["lines"]["covered"] = 89
        general_below["data"][0]["totals"]["lines"]["covered"] = 163
        failures = coverage_policy.enforce_thresholds(
            self.validate(general_below), 90, 74
        )
        self.assertEqual(
            failures, ["non-DSM line coverage is below its configured minimum"]
        )

        dsm_below = valid_document()
        dsm_below["data"][0]["files"][0]["summary"]["lines"]["covered"] = 73
        dsm_below["data"][0]["totals"]["lines"]["covered"] = 163
        failures = coverage_policy.enforce_thresholds(self.validate(dsm_below), 90, 74)
        self.assertEqual(
            failures, ["DSM boundary line coverage is below its configured minimum"]
        )

    def test_json_container_shapes_and_property_case_are_exact(self):
        self.assert_invalid([valid_document()])

        scalar_data = valid_document()
        scalar_data["data"] = scalar_data["data"][0]
        self.assert_invalid(scalar_data)

        scalar_files = valid_document()
        scalar_files["data"][0]["files"] = scalar_files["data"][0]["files"][0]
        self.assert_invalid(scalar_files)

        changed_case = valid_document()
        changed_case["data"][0]["Files"] = changed_case["data"][0].pop("files")
        self.assert_invalid(changed_case)

        self.summary_path.write_text(
            '{"type":"llvm.coverage.json.export","type":"other","data":[]}',
            encoding="utf-8",
        )
        with self.assertRaises(coverage_policy.CoveragePolicyError):
            coverage_policy.validate_summary(self.summary_path, REPOSITORY_ROOT)

    def test_exact_dsm_file_set_and_canonical_paths_are_required(self):
        for missing_index in (0, 1):
            with self.subTest(missing_index=missing_index):
                missing = valid_document()
                removed = missing["data"][0]["files"].pop(missing_index)
                missing["data"][0]["totals"]["lines"]["covered"] -= removed[
                    "summary"
                ]["lines"]["covered"]
                missing["data"][0]["totals"]["lines"]["count"] -= removed[
                    "summary"
                ]["lines"]["count"]
                self.assert_invalid(missing)

        duplicate_dsm = valid_document()
        duplicate_dsm["data"][0]["files"].append(
            copy.deepcopy(duplicate_dsm["data"][0]["files"][0])
        )
        self.assert_invalid(duplicate_dsm)

        duplicate_general = valid_document()
        duplicate_general["data"][0]["files"].append(
            copy.deepcopy(duplicate_general["data"][0]["files"][2])
        )
        self.assert_invalid(duplicate_general)

        traversal = valid_document()
        traversal["data"][0]["files"][2]["filename"] = str(
            REPOSITORY_ROOT / "src" / ".." / "src" / "api.rs"
        )
        self.assert_invalid(traversal)

        outside = valid_document()
        outside["data"][0]["files"][2]["filename"] = str(
            self.summary_path.parent / "outside.rs"
        )
        self.assert_invalid(outside)

        nonexistent_inside = valid_document()
        nonexistent_inside["data"][0]["files"][2]["filename"] = str(
            REPOSITORY_ROOT / "src" / "forged-coverage-source.rs"
        )
        self.assert_invalid(nonexistent_inside)

    def test_counts_and_unfiltered_aggregate_fail_closed(self):
        for invalid_count in (True, 1.0, -1):
            with self.subTest(invalid_count=invalid_count):
                invalid = valid_document()
                invalid["data"][0]["files"][2]["summary"]["lines"][
                    "count"
                ] = invalid_count
                self.assert_invalid(invalid)

        mismatched_total = valid_document()
        mismatched_total["data"][0]["totals"]["lines"]["covered"] += 1
        self.assert_invalid(mismatched_total)

    def duplicate_config_fixture(self, script_name: str) -> Path:
        root = Path(self.temporary_directory.name)
        scripts = root / "scripts"
        scripts.mkdir(exist_ok=True)
        script = scripts / script_name
        shutil.copy2(REPOSITORY_ROOT / "scripts" / script_name, script)
        config = root / ".config"
        config.mkdir(exist_ok=True)
        (config / "coverage.env").write_text(
            "RUST_TOOLCHAIN=1.88.0\n"
            "CARGO_LLVM_COV_VERSION=0.9.0\n"
            "COVERAGE_MIN_LINES=90\n"
            "COVERAGE_MIN_LINES=1\n"
            "COVERAGE_DSM_MIN_LINES=74\n",
            encoding="utf-8",
        )
        return script

    @unittest.skipIf(os.name == "nt", "POSIX Bash execution is covered in Linux CI")
    def test_bash_entrypoint_rejects_duplicate_configuration_keys(self):
        script = self.duplicate_config_fixture("coverage.sh")
        result = subprocess.run(
            ["bash", str(script), "check"],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 2, result)
        self.assertIn("Duplicate coverage setting: COVERAGE_MIN_LINES", result.stderr)

    @unittest.skipUnless(shutil.which("pwsh"), "PowerShell is unavailable")
    def test_powershell_entrypoint_rejects_duplicate_configuration_keys(self):
        script = self.duplicate_config_fixture("coverage.ps1")
        result = subprocess.run(
            ["pwsh", "-NoProfile", "-File", str(script), "-Mode", "Check"],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(result.returncode, 0, result)
        self.assertIn("Duplicate coverage setting: COVERAGE_MIN_LINES", result.stderr)


if __name__ == "__main__":
    unittest.main()
