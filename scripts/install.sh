#!/usr/bin/env bash
# Install mu from the latest GitHub release.
#
# Requires a checksums.txt asset next to the binary (GNU sha256sum format):
#   <sha256>  mu
#
# Fail closed: missing or mismatched checksum aborts before install.
set -euo pipefail

REPO="baroitrung/mu"
INSTALL_DIR="${MU_INSTALL_DIR:-/usr/local/bin}"
BINARY="mu"
TMP_DIR=""

trap '[[ -n "$TMP_DIR" ]] && rm -rf "$TMP_DIR"' EXIT

main() {
  echo "→ Fetching latest release..."
  local latest
  latest="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | cut -d'"' -f4)"

  if [[ -z "$latest" ]]; then
    echo "Error: could not determine latest release" >&2
    exit 1
  fi

  TMP_DIR="$(mktemp -d)"
  local base_url="https://github.com/${REPO}/releases/download/${latest}"

  echo "→ Downloading ${BINARY} ${latest}..."
  curl -fsSL -o "${TMP_DIR}/${BINARY}" "${base_url}/${BINARY}"
  chmod +x "${TMP_DIR}/${BINARY}"

  echo "→ Verifying SHA-256 against checksums.txt..."
  if ! curl -fsSL -o "${TMP_DIR}/checksums.txt" "${base_url}/checksums.txt"; then
    echo "Error: could not download checksums.txt for ${latest}" >&2
    echo "Releases must publish a checksums.txt asset (sha256sum format: '<hash>  mu')." >&2
    exit 1
  fi

  local expected actual
  # Accept "hash  mu" or "hash *mu" (binary mode)
  expected="$(awk -v b="$BINARY" '
    $2 == b || $2 == ("*" b) { print $1; exit }
  ' "${TMP_DIR}/checksums.txt")"

  if [[ -z "$expected" ]]; then
    echo "Error: checksums.txt has no entry for ${BINARY}" >&2
    exit 1
  fi

  actual="$(sha256sum "${TMP_DIR}/${BINARY}" | awk '{print $1}')"
  if [[ "$expected" != "$actual" ]]; then
    echo "Error: checksum mismatch for ${BINARY}" >&2
    echo "  expected: ${expected}" >&2
    echo "  actual:   ${actual}" >&2
    exit 1
  fi
  echo "→ Checksum OK"

  echo "→ Installing to ${INSTALL_DIR}/${BINARY}..."
  sudo install -Dm755 "${TMP_DIR}/${BINARY}" "${INSTALL_DIR}/${BINARY}"

  echo "✨ mu ${latest} installed! Run: mu --help"
}

main "$@"
