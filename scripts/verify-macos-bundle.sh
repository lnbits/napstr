#!/usr/bin/env bash
set -euo pipefail

app_binary="${1:?Napstr app executable is required}"
tor_binary="${2:?bundled Tor executable is required}"
expected_arch="${3:?expected architecture is required (arm64 or x86_64)}"

for binary in "$app_binary" "$tor_binary"; do
  if ! file "$binary" | grep -q "Mach-O.*$expected_arch"; then
    echo "Unexpected architecture; expected $expected_arch" >&2
    file "$binary" >&2
    exit 1
  fi
  codesign --verify --strict --verbose=2 "$binary"
done

tor_library_dir="$(dirname "$tor_binary")"
if [[ -n "${DYLD_LIBRARY_PATH:-}" ]]; then
  tor_library_dir="$tor_library_dir:$DYLD_LIBRARY_PATH"
fi
DYLD_LIBRARY_PATH="$tor_library_dir" "$tor_binary" --version
