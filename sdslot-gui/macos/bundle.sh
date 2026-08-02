#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Assemble sdslot-gui.app from already-built binaries.
#
# The bundle exists so macOS has something to hang a TCC grant on: raw
# /dev/rdiskN access needs Full Disk Access, and TCC can only authorize an
# app with a bundle identifier and a code signature. A bare binary run from a
# terminal has neither, so its grant has to land on the terminal instead.
#
# The CLI ships inside Contents/MacOS alongside the GUI because the GUI locates
# it as a sibling of its own executable (sdslot-gui/src/backend.rs::cli_path).
#
# Usage:
#   sdslot-gui/macos/bundle.sh [--bin-dir DIR] [--out DIR]
#                              [--version V] [--sign IDENTITY]
#
#   --bin-dir  where sdslot and sdslot-gui were built (default target/release)
#   --out      where to write sdslot-gui.app       (default same as --bin-dir)
#   --version  CFBundleVersion       (default: workspace version from Cargo.toml)
#   --sign     codesign identity, or "-" for ad-hoc (default: $CODESIGN_IDENTITY,
#              else ad-hoc)

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"

bin_dir="$root/target/release"
out_dir=""
version=""
identity="${CODESIGN_IDENTITY:--}"

while [ $# -gt 0 ]; do
  case "$1" in
    --bin-dir) bin_dir="$2"; shift 2 ;;
    --out)     out_dir="$2"; shift 2 ;;
    --version) version="$2"; shift 2 ;;
    --sign)    identity="$2"; shift 2 ;;
    -h|--help) sed -n '4,22p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "bundle.sh: unknown argument $1" >&2; exit 2 ;;
  esac
done

[ -n "$out_dir" ] || out_dir="$bin_dir"

if [ -z "$version" ]; then
  # [workspace.package] version = "0.1.0" — first `version = ` in the manifest.
  version="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' \
             "$root/Cargo.toml" | head -1)"
  [ -n "$version" ] || { echo "bundle.sh: cannot read version from Cargo.toml" >&2; exit 1; }
fi
# CFBundleVersion must be numeric-dotted; a git-hash build name is not.
case "$version" in
  *[!0-9.]*) short_version="$version"; version="0.0.0" ;;
  *)         short_version="$version" ;;
esac

for b in sdslot sdslot-gui; do
  [ -x "$bin_dir/$b" ] || {
    echo "bundle.sh: $bin_dir/$b not found — build first:" >&2
    echo "  cargo build --workspace --release" >&2
    exit 1
  }
done

app="$out_dir/sdslot-gui.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

cp "$bin_dir/sdslot-gui" "$bin_dir/sdslot" "$app/Contents/MacOS/"
cp "$root/sdslot-gui/assets/sdslot.icns" "$app/Contents/Resources/"
sed -e "s/@VERSION@/$short_version/g" "$here/Info.plist.in" \
  >"$app/Contents/Info.plist"
# CFBundleVersion separately: it must stay numeric even for git-hash builds.
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" \
  "$app/Contents/Info.plist" >/dev/null
printf 'APPL????' >"$app/Contents/PkgInfo"

plutil -lint "$app/Contents/Info.plist" >/dev/null

# Nested executables first, then the bundle — `codesign --deep` is deprecated
# and does not sign inner Mach-O binaries correctly for notarization.
codesign --force --timestamp=none --sign "$identity" "$app/Contents/MacOS/sdslot"
codesign --force --timestamp=none --sign "$identity" "$app"
codesign --verify --strict "$app"

echo "built $app (version $short_version, signed with '$identity')"
if [ "$identity" = "-" ]; then
  cat >&2 <<'EOF'

note: ad-hoc signature. macOS keys the Full Disk Access grant to the code
      signature, so every rebuild invalidates it and the app must be removed
      and re-added in System Settings. Set CODESIGN_IDENTITY to a Developer ID
      Application certificate for a grant that survives rebuilds and updates.
EOF
fi
