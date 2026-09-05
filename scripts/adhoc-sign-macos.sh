#!/usr/bin/env bash
set -euo pipefail

runtime_root="${1:?macOS Tor runtime directory is required}"
expected_arch="${2:?expected architecture is required (arm64 or x86_64)}"
tor_binary="$runtime_root/tor/tor"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "macOS ad-hoc signing must run on macOS" >&2
  exit 1
fi
if [[ ! -f "$tor_binary" ]]; then
  echo "Bundled Tor executable is missing: $tor_binary" >&2
  exit 1
fi
if ! file "$tor_binary" | grep -q "Mach-O.*$expected_arch"; then
  echo "Bundled Tor does not match the expected $expected_arch architecture" >&2
  file "$tor_binary" >&2
  exit 1
fi

while IFS= read -r -d '' candidate; do
  if file -b "$candidate" | grep -q 'Mach-O'; then
    codesign --force --sign - --timestamp=none "$candidate"
    codesign --verify --strict --verbose=2 "$candidate"
  fi
done < <(find "$runtime_root" -type f -print0)
