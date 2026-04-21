#!/usr/bin/env bash
# Build demo/initrd.cpio containing a single statically-linked /init.

set -euo pipefail

cd "$(dirname "$0")"

ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT

gcc -static -Os hello.c -o "$ROOT/init"
chmod +x "$ROOT/init"

(cd "$ROOT" && find . -print0 | cpio --null -H newc -o --quiet) > initrd.cpio

ls -la initrd.cpio
echo "built initrd.cpio ($(stat -c%s initrd.cpio) bytes)"
