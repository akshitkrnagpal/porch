#!/bin/sh
# Install the latest Porch release for this user, without sudo or Python.
#
#   curl -fsSL https://raw.githubusercontent.com/akshitkrnagpal/porch/main/scripts/install.sh | sh
#
# Downloads the release archive for this OS and CPU and checks it against the
# release's SHA256SUMS. That file comes from the same release, so it shows the
# download is intact, not who built it. If the GitHub CLI (gh 2.49 or newer,
# logged in) is available, the script also checks the archive's signed build
# attestation, which shows Porch's release workflow built it from the release
# tag; otherwise it says the check was skipped. Then it installs `porch` (plus
# the menu bar app on macOS) into ~/.local/bin and ~/Applications. Nothing
# else on the system is changed.
#
# Environment:
#   PORCH_VERSION=0.5.0   install this version instead of the latest
#   PORCH_PREFIX=DIR      install into DIR/bin (default ~/.local)
#   PORCH_TRAY=1          on x86_64 Linux, install the glibc build with the tray
#                         app (needs glibc 2.39+, GTK 3). The default is the
#                         static CLI, unless a tray from an earlier PORCH_TRAY=1
#                         install is found; PORCH_TRAY=0 removes that tray.
#   PORCH_UPDATE_VERIFY=require   refuse to install unless the attestation
#                         verifies (default: auto, check it when gh can)
#
# Everything runs from main, called on the last line, so a download cut short
# runs nothing.
set -eu

repo="akshitkrnagpal/porch"

say() { printf '%s\n' "$*"; }
warn() { printf 'porch install: %s\n' "$*" >&2; }
fail() { printf 'porch install: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || fail "needs $1"; }

setup_tools() {
  for tool in uname tar mktemp grep sed awk; do need "$tool"; done
  if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$2" "$1"; }
    # The tag the /releases/latest page redirects to; no API rate limit.
    latest_url() { curl -fsSLI --proto '=https' --tlsv1.2 --retry 3 -o /dev/null -w '%{url_effective}' "$1"; }
  elif command -v wget >/dev/null 2>&1; then
    if wget --help 2>&1 | grep -q -- '--https-only'; then
      wget_get() { wget -q --https-only "$@"; }
    else
      wget_get() { wget -q "$@"; } # BusyBox wget
    fi
    fetch() { wget_get -O "$2" "$1"; }
    latest_url() { wget_get -S --spider "$1" 2>&1 | sed -n 's/^ *[Ll]ocation: *//p' | tail -n 1; }
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
}

verify_mode() {
  mode=$(printf '%s' "${PORCH_UPDATE_VERIFY:-}" | tr '[:upper:]' '[:lower:]')
  case "$mode" in
    ''|auto) verify=auto ;;
    require) verify=require ;;
    *) fail "PORCH_UPDATE_VERIFY must be 'require' or 'auto' (the default), got '${PORCH_UPDATE_VERIFY}'" ;;
  esac
}

check_user() {
  [ "$(id -u 2>/dev/null || echo 1)" = 0 ] || return 0
  if [ -n "${SUDO_USER:-}" ]; then
    fail "don't run this with sudo: Porch installs for one user, into your home folder. Run it as $SUDO_USER."
  fi
  warn "running as root: Porch will be installed for root, in $prefix/bin."
}

pick_target() {
  os=$(uname -s)
  arch=$(uname -m)
  if [ "$os" = Darwin ] && [ "$arch" = x86_64 ]; then
    # A shell running under Rosetta reports x86_64 on Apple Silicon.
    # sysctl lives in /usr/sbin, which a minimal PATH may lack.
    if [ "$(/usr/sbin/sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ] \
      || [ "$(/usr/sbin/sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = 1 ]; then
      say "This shell runs under Rosetta; installing the native Apple Silicon build."
      arch=arm64
    fi
  fi
  tray="${PORCH_TRAY:-}"
  case "$os/$arch" in
    Darwin/arm64) target=aarch64-apple-darwin ;;
    Darwin/x86_64) target=x86_64-apple-darwin ;;
    Linux/x86_64|Linux/amd64)
      if [ -z "$tray" ] && [ -f "$prefix/bin/porch-tray" ]; then
        say "Found the tray app from an earlier PORCH_TRAY=1 install in $prefix/bin; updating it too."
        say "(Set PORCH_TRAY=0 to switch to the static CLI-only build and remove it.)"
        tray=1
      fi
      if [ "$tray" = 1 ]; then target=x86_64-unknown-linux-gnu; else target=x86_64-unknown-linux-musl; fi ;;
    Linux/aarch64|Linux/arm64)
      if [ "$tray" = 1 ]; then
        say "PORCH_TRAY=1 is ignored: there is no tray build for ARM64 Linux, so this installs the CLI only."
      fi
      target=aarch64-unknown-linux-musl ;;
    *) fail "no prebuilt release for $os $arch; build from source: cargo install --locked --git https://github.com/$repo porch" ;;
  esac
}

pick_version() {
  if [ -n "${PORCH_VERSION:-}" ]; then
    version="${PORCH_VERSION#v}"
  else
    url=$(latest_url "https://github.com/$repo/releases/latest") \
      || fail "cannot reach github.com to find the latest release"
    case "$url" in
      */releases/tag/*) version="${url##*/releases/tag/}"; version="${version#v}" ;;
      *) fail "cannot find the latest release (github.com/$repo/releases/latest went to '$url')" ;;
    esac
  fi
  case "$version" in
    ''|*[!0-9A-Za-z.+-]*) fail "'$version' is not a release version" ;;
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) fail "'$version' is not a release version" ;;
  esac
}

# Say the attestation could not be checked, or stop when it is required.
cannot_check() {
  if [ "$verify" = require ]; then
    fail "PORCH_UPDATE_VERIFY=require: cannot check the build attestation: $1"
  fi
  say "Skipped the build attestation check: $1."
  say "  Only the SHA-256 checksum was checked. It comes from the same release, so it shows"
  say "  the download is intact, not that Porch's release workflow built it. To check that,"
  say "  install the GitHub CLI (2.49 or newer) and run 'gh auth login' (or set GH_TOKEN),"
  say "  then rerun this script. PORCH_UPDATE_VERIFY=require refuses to install without it."
}

check_attestation() {
  archive="$1"
  if ! command -v gh >/dev/null 2>&1; then
    cannot_check "the GitHub CLI (gh) is not installed"
    return 0
  fi
  if ! gh attestation verify --help >"$work/gh-help" 2>&1; then
    cannot_check "this gh has no 'attestation' command (it needs gh 2.49 or newer)"
    return 0
  fi
  workflow="$repo/.github/workflows/release.yml"
  status=0
  if grep -q -- '--source-ref' "$work/gh-help"; then
    pinned=1
    gh attestation verify "$archive" --repo "$repo" --signer-workflow "$workflow" \
      --source-ref "refs/tags/v$version" >/dev/null 2>"$work/gh-stderr" || status=$?
  else
    [ "$verify" = require ] && fail "PORCH_UPDATE_VERIFY=require: this gh can't check which tag the release was built from (no --source-ref); update the GitHub CLI"
    pinned=0
    gh attestation verify "$archive" --repo "$repo" --signer-workflow "$workflow" \
      >/dev/null 2>"$work/gh-stderr" || status=$?
  fi
  if [ "$status" = 0 ]; then
    if [ "$pinned" = 1 ]; then
      say "Verified build attestation: built by $workflow from refs/tags/v$version"
    else
      say "Verified build attestation: built by $workflow (update gh to also check the release tag)"
    fi
    return 0
  fi
  sed 's/^/  gh: /' "$work/gh-stderr" >&2
  if [ "$status" = 4 ]; then
    cannot_check "gh is not logged in (run 'gh auth login', or set GH_TOKEN)"
  elif gh api --silent rate_limit >/dev/null 2>&1; then
    # GitHub answered, so the archive really has no matching attestation.
    fail "the build attestation did not verify; not installing"
  else
    cannot_check "gh could not reach the GitHub API, or its token was rejected"
  fi
}

install_file() {
  # Write beside the target and rename, so a running porch keeps working.
  cp "$1" "$prefix/bin/.$2.new"
  chmod 755 "$prefix/bin/.$2.new"
  mv -f "$prefix/bin/.$2.new" "$prefix/bin/$2"
  say "Installed $prefix/bin/$2"
}

check_app() {
  apps="${PORCH_APP_DIR:-$HOME/Applications}"
  app="$apps/Porch.app"
  if [ -e "$app" ] && ! grep -q '<string>dev.porch.tray</string>' "$app/Contents/Info.plist" 2>/dev/null; then
    fail "refusing to replace $app: it is not Porch's app (set PORCH_APP_DIR to install elsewhere)"
  fi
}

install_app() {
  mkdir -p "$apps"
  staged="$apps/.Porch.app.new.$$"
  old="$apps/.Porch.app.old.$$"
  rm -rf "$staged" "$old"
  cp -R "$work/$name/Porch.app" "$staged"
  if [ -e "$app" ]; then
    mv "$app" "$old"
  fi
  if ! mv "$staged" "$app"; then
    [ -e "$old" ] && mv "$old" "$app"
    rm -rf "$staged"
    fail "cannot replace $app"
  fi
  rm -rf "$old"
  say "Installed $app"
}

main() {
  prefix="${PORCH_PREFIX:-$HOME/.local}"
  setup_tools
  verify_mode
  check_user
  pick_target
  pick_version

  name="porch-$version-$target"
  base="https://github.com/$repo/releases/download/v$version"
  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT
  trap 'rm -rf "$work"; exit 130' INT
  trap 'rm -rf "$work"; exit 143' TERM

  say "Downloading Porch $version for $target"
  fetch "$base/$name.tar.gz" "$work/$name.tar.gz" || fail "download failed: $base/$name.tar.gz"
  fetch "$base/SHA256SUMS" "$work/SHA256SUMS" || fail "download failed: $base/SHA256SUMS"

  expected=$(awk -v f="$name.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$work/SHA256SUMS")
  actual=$(sha256 "$work/$name.tar.gz")
  [ -n "$expected" ] || fail "SHA256SUMS has no entry for $name.tar.gz"
  [ "$expected" = "$actual" ] || fail "checksum mismatch for $name.tar.gz"
  say "Verified SHA-256 $actual"

  check_attestation "$work/$name.tar.gz"

  tar -xzf "$work/$name.tar.gz" -C "$work" || fail "cannot extract $name.tar.gz"
  [ -f "$work/$name/porch" ] || fail "$name.tar.gz does not contain porch"
  with_app=0
  if [ "$os" = Darwin ] && [ -d "$work/$name/Porch.app" ]; then
    with_app=1
    check_app # before anything is replaced
  fi
  mkdir -p "$prefix/bin"
  for exe in porch porch-tray porch-login; do
    if [ -f "$work/$name/$exe" ]; then
      install_file "$work/$name/$exe" "$exe"
    elif [ -f "$prefix/bin/$exe" ] && [ "$exe" != porch ]; then
      # A CLI-only build (PORCH_TRAY=0) over an earlier tray install.
      rm -f "$prefix/bin/$exe"
      say "Removed $prefix/bin/$exe (left from an earlier PORCH_TRAY=1 install; this build has no tray)"
    fi
  done
  if [ "$with_app" = 1 ]; then
    install_app
  fi

  case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) say ""; say "Add Porch to your PATH:"; say "  export PATH=\"$prefix/bin:\$PATH\"" ;;
  esac
  say ""
  say "Next: run \`porch setup\`, or start an app with \`porch run <your dev command>\`."
}

main "$@"
