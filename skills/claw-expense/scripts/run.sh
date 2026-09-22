#!/bin/bash
set -euo pipefail
script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
cli=$(/bin/bash "$script_dir/ensure-cli.sh")
exec "$cli" "$@"
