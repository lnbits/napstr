#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
node scripts/ios-release.mjs version
suffix=''
# Match Anagram: no iOS signing material produces an explicitly unsigned IPA.
if [[ "$#" == 0 && -z "${IOS_CERTIFICATE:-}${IOS_CERTIFICATE_PASSWORD:-}${IOS_MOBILE_PROVISION:-}" ]]; then
  set -- --unsigned
fi
if [[ "${1:-}" == '--unsigned' && "$#" == 1 ]]; then
  unset IOS_CERTIFICATE IOS_CERTIFICATE_PASSWORD IOS_MOBILE_PROVISION APPLE_DEVELOPMENT_TEAM
  npm run tauri -- ios build --ci --target aarch64 --no-sign --config src-tauri/tauri.ios.ci.conf.json -- --locked
  suffix='-unsigned'
elif [[ "$#" == 0 ]]; then
  : "${APPLE_DEVELOPMENT_TEAM:?Missing APPLE_TEAM_ID}"
  : "${IOS_CERTIFICATE:?Missing IOS_CERTIFICATE}"
  : "${IOS_CERTIFICATE_PASSWORD:?Missing IOS_CERTIFICATE_PASSWORD}"
  : "${IOS_MOBILE_PROVISION:?Missing IOS_MOBILE_PROVISION}"
  export IOS_EXPORT_METHOD="${IOS_EXPORT_METHOD:-app-store-connect}"
  python3 scripts/verify-ios.py profile
  npm run tauri -- ios build --ci --target aarch64 --export-method "$IOS_EXPORT_METHOD" --config src-tauri/tauri.ios.ci.conf.json -- --locked
else
  echo 'Usage: package-ios.sh [--unsigned]' >&2
  exit 1
fi
count=$(find src-tauri/gen/apple/build -type f -name '*.ipa' | wc -l | tr -d ' ')
[[ "$count" == 1 ]] || { echo 'Expected exactly one iPhone IPA' >&2; exit 1; }
ipa=$(find src-tauri/gen/apple/build -type f -name '*.ipa')
python3 scripts/verify-ios.py ipa "$ipa" "$suffix"
version="${RELEASE_TAG:-}"
version="${version#v}"
if [[ -z "$version" ]]; then version=$(node -p 'require("./package.json").version'); fi
mkdir -p ios-installers
asset="Napstrfy_${version}_ios${suffix}.ipa"
cp "$ipa" "ios-installers/$asset"
(cd ios-installers && shasum -a 256 "$asset" > "$asset.sha256")
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  if [[ -n "$suffix" ]]; then
    echo 'Unsigned iOS validation package: requires signing before installation.' >> "$GITHUB_STEP_SUMMARY"
  else
    printf 'Signed iOS IPA (%s). Installation depends on provisioning; no App Store/TestFlight submission was performed.\n' "$IOS_EXPORT_METHOD" >> "$GITHUB_STEP_SUMMARY"
  fi
fi
