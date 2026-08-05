#!/usr/bin/env bash
set -euo pipefail

repository_root=$(git rev-parse --show-toplevel)
cd "$repository_root"

mapfile -d '' shell_scripts < <(
    git ls-files --cached --others --exclude-standard -z -- '*.sh'
)
if (( ${#shell_scripts[@]} == 0 )); then
    echo 'no shell scripts found to validate' >&2
    exit 1
fi

for script in "${shell_scripts[@]}"; do
    expected_attribute="$script: eol: lf"
    actual_attribute=$(git check-attr eol -- "$script")
    if [[ "$actual_attribute" != "$expected_attribute" ]]; then
        echo "$script must have the Git eol=lf attribute; received: $actual_attribute" >&2
        exit 1
    fi
    if LC_ALL=C grep -q $'\r' "$script"; then
        echo "$script contains a carriage return; shell and container entrypoints require LF" >&2
        exit 1
    fi
    bash -n "$script"
done

command -v python3 >/dev/null 2>&1 || {
    echo 'python3 is required for service-asset contract validation' >&2
    exit 1
}
python3 packaging/validate-service-assets.py

docker_text_files=(Dockerfile .dockerignore compose.yaml compose.totp.yaml)
for file in "${docker_text_files[@]}"; do
    expected_attribute="$file: eol: lf"
    actual_attribute=$(git check-attr eol -- "$file")
    if [[ "$actual_attribute" != "$expected_attribute" ]]; then
        echo "$file must have the Git eol=lf attribute; received: $actual_attribute" >&2
        exit 1
    fi
    if LC_ALL=C grep -q $'\r' "$file"; then
        echo "$file contains a carriage return; Docker packaging inputs require LF" >&2
        exit 1
    fi
done

printf 'validated %d shell scripts and %d Docker packaging files\n' \
    "${#shell_scripts[@]}" "${#docker_text_files[@]}"
