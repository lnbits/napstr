#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 4 ]]; then
  echo 'Usage: package.sh BINARY VERSION ARCH OUTPUT_DIRECTORY' >&2
  exit 1
fi
binary="$1"
version="$2"
arch="$3"
output="$4"
if [[ ! "$version" =~ ^[0-9A-Za-z][0-9A-Za-z.-]*$ ]] || [[ "$arch" != x86_64 && "$arch" != aarch64 ]]; then
  echo 'Invalid version or Linux architecture' >&2
  exit 1
fi
source_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
staging="$(mktemp -d)"
trap 'rm -rf -- "$staging"' EXIT
name="napstrd_${version}_linux_${arch}"
mkdir -p "$staging/$name" "$output"
install -m 755 "$binary" "$staging/$name/napstrd"
install -m 644 "$source_dir/README.md" "$source_dir/napstrd.service" "$source_dir/napstrd.env.example" "$source_dir/../LICENSE" "$staging/$name/"
tar -czf "$output/$name.tar.gz" -C "$staging" "$name"
(cd "$output" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
