#!/usr/bin/env bash
# Prints the orbd commit Cargo.lock pins: the guest kernel and the guest agent library
# come from the same revision.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
rev="$(sed -n 's|^source = "git+https://github.com/samcm/orbd[^#]*#\([0-9a-f]\{40\}\)"$|\1|p' \
    "$here/../../Cargo.lock" | sort -u)"
[ "$(printf '%s\n' "$rev" | grep -c .)" = 1 ] || {
    echo "expected one orbd revision in Cargo.lock, got: ${rev:-none}" >&2
    exit 1
}
echo "$rev"
