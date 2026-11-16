#!/usr/bin/env python3
"""Validate the repository's exact split line-coverage policy."""

from __future__ import annotations

import argparse
import json
import os
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple


DSM_BOUNDARY = (
    Path("src/dsm_api.rs"),
    Path("src/bin/sdsync-dsm-api.rs"),
)


class CoveragePolicyError(ValueError):
    """The llvm-cov JSON does not satisfy the fail-closed input contract."""


@dataclass(frozen=True)
class CoverageMetrics:
    general_covered: int
    general_count: int
    dsm_covered: int
    dsm_count: int
    # (spelling summed, spelling it was summed into), both as llvm-cov wrote them.
    merged: Tuple[Tuple[str, str], ...] = ()


def _unique_object(pairs: Sequence[Tuple[str, Any]]) -> Dict[str, Any]:
    result: Dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise CoveragePolicyError(f"duplicate JSON property: {key}")
        result[key] = value
    return result


def _required_property(value: Any, name: str, label: str) -> Any:
    if not isinstance(value, dict):
        raise CoveragePolicyError(f"{label} must be an object")
    case_matches = [key for key in value if key.casefold() == name.casefold()]
    if case_matches != [name]:
        raise CoveragePolicyError(
            f"{label} must contain exactly one case-sensitive {name!r} property"
        )
    return value[name]


def _exact_integer(value: Any, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise CoveragePolicyError(f"{label} must be a non-negative integer")
    return value


def _canonical_input_path(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise CoveragePolicyError(f"{label} must be a non-empty path")
    if not os.path.isabs(value):
        raise CoveragePolicyError(f"{label} must be absolute")
    # Since Rust 1.100, llvm-cov names a module that a binary includes with `#[path = "../x.rs"]`
    # as written (`src/bin/../dsm_api.rs`). Resolve `.` and `..` lexically, then through the
    # filesystem, so every spelling of a file is judged as that one file; containment in the
    # repository and the exact DSM set are checked on the result.
    absolute = os.path.normpath(value)
    return os.path.normcase(os.path.realpath(absolute))


def _has_traversal(value: str) -> bool:
    return any(part in {".", ".."} for part in value.replace("\\", "/").split("/"))


def _canonical_source_path(value: Any, label: str) -> str:
    path = _canonical_input_path(value, label)
    if not os.path.isfile(path) or Path(path).suffix != ".rs":
        raise CoveragePolicyError(
            f"{label} must resolve to an existing regular Rust source file"
        )
    return path


def _line_counts(entry: Any, label: str) -> Tuple[int, int]:
    summary = _required_property(entry, "summary", label)
    lines = _required_property(summary, "lines", f"{label}.summary")
    count = _exact_integer(
        _required_property(lines, "count", f"{label}.summary.lines"),
        f"{label} line count",
    )
    covered = _exact_integer(
        _required_property(lines, "covered", f"{label}.summary.lines"),
        f"{label} covered lines",
    )
    if covered > count:
        raise CoveragePolicyError(f"covered lines exceed executable lines for {label}")
    return covered, count


def validate_summary(summary_path: Path, repository_root: Path) -> CoverageMetrics:
    """Parse an unfiltered llvm-cov summary and return its exact partitions."""

    try:
        with summary_path.open("r", encoding="utf-8") as summary_file:
            document = json.load(summary_file, object_pairs_hook=_unique_object)
    except CoveragePolicyError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise CoveragePolicyError(f"could not read canonical JSON: {error}") from error

    export_type = _required_property(document, "type", "top-level export")
    if export_type != "llvm.coverage.json.export":
        raise CoveragePolicyError("the llvm-cov export type is unsupported")
    data = _required_property(document, "data", "top-level export")
    if not isinstance(data, list) or len(data) != 1:
        raise CoveragePolicyError(
            "exactly one llvm-cov data-set array entry is required"
        )
    data_set = data[0]
    if not isinstance(data_set, dict):
        raise CoveragePolicyError("the llvm-cov data set must be an object")

    repository = _canonical_input_path(
        str(repository_root.resolve(strict=True)), "repository root"
    )
    expected_dsm_paths = {
        _canonical_input_path(
            str((repository_root / relative).resolve(strict=True)),
            f"DSM boundary path {relative.as_posix()}",
        )
        for relative in DSM_BOUNDARY
    }
    seen_dsm_paths = set()
    seen_filenames = set()
    first_spelling: Dict[str, str] = {}
    merged: List[Tuple[str, str]] = []
    dsm_covered = 0
    dsm_count = 0
    general_covered = 0
    general_count = 0

    files = _required_property(data_set, "files", "llvm-cov data set")
    if not isinstance(files, list) or not files:
        raise CoveragePolicyError("the files property must be a non-empty array")
    for index, entry in enumerate(files):
        label = f"files[{index}]"
        if not isinstance(entry, dict):
            raise CoveragePolicyError(f"{label} must be an object")
        filename = _required_property(entry, "filename", label)
        path = _canonical_source_path(filename, f"{label}.filename")
        try:
            inside_repository = os.path.commonpath((repository, path)) == repository
        except ValueError:
            inside_repository = False
        if not inside_repository:
            raise CoveragePolicyError(
                f"instrumented path is outside the repository: {path}"
            )
        # The same spelling twice is a malformed export. Two spellings of one file (a module the
        # library compiles and a binary includes through `#[path]`) are two llvm-cov entries that
        # its unfiltered aggregate counts separately, so they are summed here as well.
        if filename in seen_filenames:
            raise CoveragePolicyError(f"duplicate instrumented path: {filename}")
        seen_filenames.add(filename)
        if path in first_spelling:
            earlier = first_spelling[path]
            if _has_traversal(earlier) and not _has_traversal(filename):
                # Name the clean spelling as the file the other was summed into.
                merged.append((earlier, filename))
                first_spelling[path] = filename
            else:
                merged.append((filename, earlier))
        else:
            first_spelling[path] = filename

        covered, count = _line_counts(entry, label)
        if path in expected_dsm_paths:
            seen_dsm_paths.add(path)
            dsm_covered += covered
            dsm_count += count
        else:
            general_covered += covered
            general_count += count

    if seen_dsm_paths != expected_dsm_paths:
        missing = sorted(expected_dsm_paths - seen_dsm_paths)
        raise CoveragePolicyError(
            f"the exact DSM boundary is incomplete: missing {missing}"
        )
    if dsm_count == 0 or general_count == 0:
        raise CoveragePolicyError(
            "both coverage partitions must contain executable lines"
        )

    totals = _required_property(data_set, "totals", "llvm-cov data set")
    total_lines = _required_property(totals, "lines", "llvm-cov totals")
    total_count = _exact_integer(
        _required_property(total_lines, "count", "llvm-cov total lines"),
        "aggregate line count",
    )
    total_covered = _exact_integer(
        _required_property(total_lines, "covered", "llvm-cov total lines"),
        "aggregate covered lines",
    )
    if total_count != dsm_count + general_count:
        raise CoveragePolicyError(
            "per-file executable-line counts do not match the unfiltered aggregate"
        )
    if total_covered != dsm_covered + general_covered:
        raise CoveragePolicyError(
            "per-file covered-line counts do not match the unfiltered aggregate"
        )

    return CoverageMetrics(
        general_covered=general_covered,
        general_count=general_count,
        dsm_covered=dsm_covered,
        dsm_count=dsm_count,
        merged=tuple(merged),
    )


def merge_notes(metrics: CoverageMetrics, repository_root: Path) -> List[str]:
    """One line per file whose second spelling was summed into its first."""

    root = str(repository_root.resolve()).replace("\\", "/").rstrip("/") + "/"

    def shown(filename: str) -> str:
        spelled = filename.replace("\\", "/")
        return spelled[len(root) :] if spelled.startswith(root) else filename

    return [
        f"note: merged {shown(summed)} into {shown(into)}"
        for summed, into in metrics.merged
    ]


def enforce_thresholds(
    metrics: CoverageMetrics, general_minimum: int, dsm_minimum: int
) -> List[str]:
    """Return every failed threshold using exact integer arithmetic."""

    failures = []
    if metrics.general_covered * 100 < general_minimum * metrics.general_count:
        failures.append("non-DSM line coverage is below its configured minimum")
    if metrics.dsm_covered * 100 < dsm_minimum * metrics.dsm_count:
        failures.append("DSM boundary line coverage is below its configured minimum")
    return failures


def _threshold(value: str) -> int:
    try:
        parsed = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("threshold must be an integer") from error
    if not 1 <= parsed <= 100 or str(parsed) != value:
        raise argparse.ArgumentTypeError(
            "threshold must be a canonical integer from 1 to 100"
        )
    return parsed


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--summary", required=True, type=Path)
    parser.add_argument("--repository", required=True, type=Path)
    parser.add_argument("--minimum-general", required=True, type=_threshold)
    parser.add_argument("--minimum-dsm", required=True, type=_threshold)
    parser.add_argument("--enforce", action="store_true")
    args = parser.parse_args(argv)

    try:
        metrics = validate_summary(args.summary, args.repository)
    except CoveragePolicyError as error:
        print(f"Invalid coverage summary: {error}", file=sys.stderr)
        return 2

    for note in merge_notes(metrics, args.repository):
        print(note)
    print(
        "Non-DSM line coverage: "
        f"{metrics.general_covered}/{metrics.general_count} = "
        f"{metrics.general_covered * 100.0 / metrics.general_count:.4f}% "
        f"(minimum {args.minimum_general}%)"
    )
    print(
        "DSM boundary line coverage: "
        f"{metrics.dsm_covered}/{metrics.dsm_count} = "
        f"{metrics.dsm_covered * 100.0 / metrics.dsm_count:.4f}% "
        f"(minimum {args.minimum_dsm}%)"
    )
    if args.enforce:
        failures = enforce_thresholds(
            metrics, args.minimum_general, args.minimum_dsm
        )
        if failures:
            print("Coverage gate failed: " + "; ".join(failures), file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
