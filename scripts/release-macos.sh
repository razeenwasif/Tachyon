#!/bin/zsh
# Build a Developer ID-signed (and optionally notarized) macOS release DMG.
#
#   scripts/release-macos.sh
#   scripts/release-macos.sh --identity "Developer ID Application: ..."
#   scripts/release-macos.sh --notarize <notarytool-keychain-profile>
set -euo pipefail

root=${0:A:h:h}
cd "$root"

notarize_profile=""
sign_id="${TACHYON_CODESIGN_ID:-}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --notarize)
            [[ $# -ge 2 ]] || { echo "--notarize requires a notarytool keychain profile name" >&2; exit 1; }
            notarize_profile=$2
            shift 2
            ;;
        --identity)
            [[ $# -ge 2 ]] || { echo "--identity requires a codesigning identity name or hash" >&2; exit 1; }
            sign_id=$2
            shift 2
            ;;
        -h|--help)
            cat <<'EOF'
Usage: scripts/release-macos.sh [--identity <id>] [--notarize <keychain-profile>]

Builds dist/Tachyon.app via scripts/bundle-macos.sh, re-signs with a
Developer ID Application identity (hardened runtime + timestamp), packages
dist/Tachyon-<version>-arm64.dmg with an /Applications drop link, and
optionally submits + staples via `xcrun notarytool`.
EOF
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            exit 1
            ;;
    esac
done

if [[ -z "$sign_id" ]]; then
    sign_id=$(security find-identity -v -p codesigning \
        | awk '/"Developer ID Application:/ { match($0, /"[^"]+"/); print substr($0, RSTART + 1, RLENGTH - 2); exit }')
fi
[[ -n "$sign_id" ]] || {
    echo "no 'Developer ID Application' identity found (set TACHYON_CODESIGN_ID or pass --identity)" >&2
    exit 1
}

"$root/scripts/bundle-macos.sh"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
app=dist/Tachyon.app
dmg="dist/Tachyon-${version}-arm64.dmg"

echo "Signing $app with: $sign_id"
codesign --force --deep --options runtime --timestamp --sign "$sign_id" "$app"
codesign --verify --deep --strict --verbose=2 "$app"

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
ditto "$app" "$stage/Tachyon.app"
ln -s /Applications "$stage/Applications"

rm -f "$dmg"
hdiutil create -volname Tachyon -srcfolder "$stage" -ov -format UDZO "$dmg"
codesign --force --timestamp --sign "$sign_id" "$dmg"
hdiutil verify "$dmg"

if [[ -n "$notarize_profile" ]]; then
    xcrun notarytool submit "$dmg" --keychain-profile "$notarize_profile" --wait
    xcrun stapler staple "$dmg"
    xcrun stapler validate "$dmg"
fi

shasum -a 256 "$dmg"
echo "created $dmg"
