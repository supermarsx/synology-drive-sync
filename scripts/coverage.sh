#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: bash scripts/coverage.sh [check|report|html]

  check   Run all test targets and enforce the configured line threshold (default).
  report  Run all test targets and print the full repository coverage baseline.
  html    Run all test targets and write an HTML report under target/llvm-cov/html.

Every mode writes target/llvm-cov/coverage-summary.json.
EOF
}

mode="${1:-check}"
if [[ $# -gt 1 ]]; then
  usage >&2
  exit 2
fi

case "$mode" in
  check|report|html) ;;
  -h|--help)
    usage
    exit 0
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
config_path="$repo_root/.config/coverage.env"

rust_toolchain=""
tool_version=""
minimum_lines=""
while IFS='=' read -r key value; do
  case "$key" in
    RUST_TOOLCHAIN) rust_toolchain="$value" ;;
    CARGO_LLVM_COV_VERSION) tool_version="$value" ;;
    COVERAGE_MIN_LINES) minimum_lines="$value" ;;
    ''|'#'*) ;;
    *)
      echo "Unknown coverage setting: $key" >&2
      exit 2
      ;;
  esac
done < "$config_path"

if [[ ! "$rust_toolchain" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
   [[ ! "$tool_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
   [[ ! "$minimum_lines" =~ ^([1-9][0-9]?|100)$ ]]; then
  echo "Invalid coverage configuration in $config_path" >&2
  exit 2
fi

cd "$repo_root"

expected_version="cargo-llvm-cov $tool_version"
actual_version="$(cargo "+$rust_toolchain" llvm-cov --version 2>/dev/null || true)"
if [[ "$actual_version" != "$expected_version" ]]; then
  cat >&2 <<EOF
Expected $expected_version for Rust $rust_toolchain, found: ${actual_version:-not installed}
Install it with:
  cargo +$rust_toolchain install cargo-llvm-cov --version $tool_version --locked
EOF
  exit 2
fi

if ! rustup component list --toolchain "$rust_toolchain" --installed |
  grep -q '^llvm-tools'; then
  cat >&2 <<EOF
The llvm-tools-preview component is required for Rust $rust_toolchain.
Install it with:
  rustup component add --toolchain $rust_toolchain llvm-tools-preview
EOF
  exit 2
fi

output_dir="$repo_root/target/llvm-cov"
summary_path="$output_dir/coverage-summary.json"

cargo "+$rust_toolchain" llvm-cov clean --workspace
cargo "+$rust_toolchain" llvm-cov --locked --workspace --all-targets --no-report
mkdir -p "$output_dir"
cargo "+$rust_toolchain" llvm-cov report \
  --json \
  --summary-only \
  --output-path "$summary_path"

case "$mode" in
  check)
    cargo "+$rust_toolchain" llvm-cov report --fail-under-lines "$minimum_lines"
    ;;
  report)
    cargo "+$rust_toolchain" llvm-cov report
    ;;
  html)
    cargo "+$rust_toolchain" llvm-cov report --html --output-dir "$output_dir/html"
    echo "HTML coverage report: $output_dir/html/index.html"
    ;;
esac

echo "Coverage summary: $summary_path"
