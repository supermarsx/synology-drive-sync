#!/bin/sh
set -eu

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)

python_command=${PYTHON:-}
if [ -z "$python_command" ]; then
    if command -v python3 >/dev/null 2>&1; then
        python_command=python3
    elif command -v python >/dev/null 2>&1; then
        python_command=python
    else
        echo "python3 is required to assemble the deterministic SPK" >&2
        exit 69
    fi
fi

exec "$python_command" "$script_dir/build_spk.py" "$@"
