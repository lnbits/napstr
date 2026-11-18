These crates preserve Napstrfy's locked versions. Upstream source and licenses
are retained; the following patches enable the iOS build without changing Iroh's
protocol or importing macOS-only private APIs on iOS.

- `netwatch 0.19.3`: copied from Anagram's vendored crates.io release. Only the
  manifest differs from upstream: iOS disables netdev's default metadata features
  and retains gateway support. Other targets keep their original features.
- `netdev 0.46.1`: crates.io source. iOS alone uses the `in6_ifreq` ABI definition
  from upstream commit `1327c36c32027d7252093cc1b58d9abd5a351496`, also used by
  Anagram. macOS retains its original libc type. Source:
  https://github.com/shellrow/netdev/commit/1327c36c32027d7252093cc1b58d9abd5a351496
- `hickory-resolver 0.26.1`: crates.io source. iOS uses a public libresolv reader
  adapted from Anagram's `n0-dns-resolver` iOS bridge. Non-iOS behavior is unchanged.
  The build script compiles the bridge only on iOS with `system-config` enabled.
  Hickory's nameserver type cannot retain IPv6 scope IDs, so scoped nameservers
  are skipped. If none remain, Iroh's existing resolver fallback applies.

The bridge test runs with the Apple SDK in the iOS workflow. No split-DNS
per-domain enumeration is added. Remove these patches when the upstream
versions used by Napstrfy support iOS through public APIs.
