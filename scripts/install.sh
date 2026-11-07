#!/bin/sh
# Install the latest Porch release for this user, without sudo or Python.
#
#   curl -fsSL https://raw.githubusercontent.com/akshitkrnagpal/porch/main/scripts/install.sh | sh
#
# Downloads the release archive for this OS and CPU, checks it against the
# release's SHA256SUMS and, when the GitHub CLI is installed, its signed build
# attestation. Then installs `porch` (plus the menu bar app on macOS) into
# ~/.local/bin and ~/Applications. Nothing else on the system is changed.
#
# Environment:
#   PORCH_VERSION=0.5.0   install this version instead of the latest
#   PORCH_PREFIX=DIR      install into DIR/bin (default ~/.local)
#   PORCH_TRAY=1          on Linux, install the glibc build with the tray app
#                         (needs glibc 2.39+, GTK 3); default is the static CLI
#   PORCH_UPDATE_VERIFY=require   fail unless the attestation verifies
set -eu

repo="akshitkrnagpal/porch"
prefix="${PORCH_PREFIX:-$HOME/.local}"

say() { printf '%s\n' "$*"; }
fail() { printf 'porch install: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || fail "needs $1"; }

need uname
need tar
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  fail "needs curl or wget"
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  fail "needs sha256sum or shasum"
fi

os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
  Darwin/arm64) target=aarch64-apple-darwin ;;
  Darwin/x86_64) target=x86_64-apple-darwin ;;
  Linux/x86_64|Linux/amd64)
    if [ "${PORCH_TRAY:-0}" = 1 ]; then target=x86_64-unknown-linux-gnu; else target=x86_64-unknown-linux-musl; fi ;;
  Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-musl ;;
  *) fail "no prebuilt release for $os $arch; build from source: cargo install --git https://github.com/$repo porch" ;;
esac

if [ -n "${PORCH_VERSION:-}" ]; then
  version="${PORCH_VERSION#v}"
else
  tmp_json=$(mktemp)
  fetch "https://api.github.com/repos/$repo/releases/latest" "$tmp_json" || fail "cannot reach GitHub"
  version=$(sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' "$tmp_json" | head -n 1)
  rm -f "$tmp_json"
  [ -n "$version" ] || fail "cannot find the latest release"
fi

name="porch-$version-$target"
base="https://github.com/$repo/releases/download/v$version"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

say "Downloading Porch $version for $target"
fetch "$base/$name.tar.gz" "$work/$name.tar.gz" || fail "download failed: $base/$name.tar.gz"
fetch "$base/SHA256SUMS" "$work/SHA256SUMS" || fail "download failed: $base/SHA256SUMS"

expected=$(awk -v f="$name.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$work/SHA256SUMS")
actual=$(sha256 "$work/$name.tar.gz")
[ -n "$expected" ] || fail "SHA256SUMS has no entry for $name.tar.gz"
[ "$expected" = "$actual" ] || fail "checksum mismatch for $name.tar.gz"
say "Verified SHA-256 $actual"

if command -v gh >/dev/null 2>&1; then
  gh attestation verify "$work/$name.tar.gz" --repo "$repo" \
    --signer-workflow "$repo/.github/workflows/release.yml" >/dev/null 2>&1 \
    || fail "the build attestation did not verify; not installing"
  say "Verified build attestation"
elif [ "${PORCH_UPDATE_VERIFY:-}" = require ]; then
  fail "PORCH_UPDATE_VERIFY=require needs the GitHub CLI (gh)"
fi

tar -xzf "$work/$name.tar.gz" -C "$work"
mkdir -p "$prefix/bin"
for exe in porch porch-tray porch-login; do
  if [ -f "$work/$name/$exe" ]; then
    # Write beside the target and rename, so a running porch keeps working.
    cp "$work/$name/$exe" "$prefix/bin/.$exe.new"
    chmod 755 "$prefix/bin/.$exe.new"
    mv -f "$prefix/bin/.$exe.new" "$prefix/bin/$exe"
    say "Installed $prefix/bin/$exe"
  fi
done
if [ "$os" = Darwin ] && [ -d "$work/$name/Porch.app" ]; then
  apps="${PORCH_APP_DIR:-$HOME/Applications}"
  mkdir -p "$apps"
  rm -rf "$apps/Porch.app.new"
  cp -R "$work/$name/Porch.app" "$apps/Porch.app.new"
  rm -rf "$apps/Porch.app"
  mv "$apps/Porch.app.new" "$apps/Porch.app"
  say "Installed $apps/Porch.app"
fi

case ":$PATH:" in
  *":$prefix/bin:"*) ;;
  *) say ""; say "Add Porch to your PATH:"; say "  export PATH=\"$prefix/bin:\$PATH\"" ;;
esac
say ""
say "Next: run \`porch setup\`, or start an app with \`porch run <your dev command>\`."
