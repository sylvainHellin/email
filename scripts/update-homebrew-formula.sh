#!/usr/bin/env bash
set -euo pipefail

# Render the Homebrew formula for a tagged release (ticket #0013).
#
# Usage: scripts/update-homebrew-formula.sh <version>
#   e.g. scripts/update-homebrew-formula.sh 0.9.0
#
# Downloads the .sha256 checksum files that the release workflow attached
# to the GitHub release v<version>, substitutes them into
# packaging/homebrew/mailypoppins.rb.tmpl, and prints the rendered formula
# to stdout. Called by the `homebrew-tap` job in
# .github/workflows/release.yml; can also be run locally.

version="${1:?Usage: $0 <version> (e.g. 0.9.0)}"
repo_root="$(git rev-parse --show-toplevel)"
template="${repo_root}/packaging/homebrew/mailypoppins.rb.tmpl"
base_url="https://github.com/sylvainHellin/mailypoppins/releases/download/v${version}"

checksum() {
  local target="$1"
  # Checksum files contain "<sha256>  <filename>".
  # Asset name: the workflow uploads "mailypoppins-<target>.sha256"
  # (not ".tar.gz.sha256").
  curl -fsSL "${base_url}/mailypoppins-${target}.sha256" | awk '{print $1}'
}

sha_macos_arm64="$(checksum aarch64-apple-darwin)"
sha_macos_x86_64="$(checksum x86_64-apple-darwin)"
sha_linux_x86_64="$(checksum x86_64-unknown-linux-musl)"

sed \
  -e "s/__VERSION__/${version}/" \
  -e "s/__SHA256_MACOS_ARM64__/${sha_macos_arm64}/" \
  -e "s/__SHA256_MACOS_X86_64__/${sha_macos_x86_64}/" \
  -e "s/__SHA256_LINUX_X86_64__/${sha_linux_x86_64}/" \
  "${template}"
