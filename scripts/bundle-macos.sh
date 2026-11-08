#!/bin/zsh
# Build Tachyon.app for Apple silicon.
#
#   scripts/bundle-macos.sh             build into dist/Tachyon.app
#   scripts/bundle-macos.sh --install   ...and copy it to /Applications
#
# The icon is rendered from assets/icon/*.svg at build time, so no binary
# artwork is checked in (the same rule build.rs follows for the Windows .ico).
set -euo pipefail

root=${0:A:h:h}
cd "$root"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cargo build --release
bin=$(cargo build --release --message-format=json 2>/dev/null \
    | sed -n 's/.*"executable":"\([^"]*\/tachyon\)".*/\1/p' | tail -1)
[[ -x "$bin" ]] || { echo "release binary not found" >&2; exit 1; }

app=dist/Tachyon.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/tachyon"

# --- icon: render the SVG masters with Quick Look, then build an .icns ---
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
qlmanage -t -s 1024 -o "$work" assets/icon/tachyon.svg assets/icon/tachyon-small.svg >/dev/null 2>&1
big=$work/tachyon.svg.png
small=$work/tachyon-small.svg.png
[[ -f "$big" && -f "$small" ]] || { echo "could not render the icon SVGs" >&2; exit 1; }
set_dir=$work/Tachyon.iconset
mkdir -p "$set_dir"
# 16 and 32 pt use the simplified mark: the trail and glow turn to mush there.
for spec in 16:icon_16x16:small 32:icon_16x16@2x:small 32:icon_32x32:small \
            64:icon_32x32@2x:big 128:icon_128x128:big 256:icon_128x128@2x:big \
            256:icon_256x256:big 512:icon_256x256@2x:big 512:icon_512x512:big \
            1024:icon_512x512@2x:big; do
    size=${spec%%:*}; rest=${spec#*:}; name=${rest%%:*}; which=${rest#*:}
    src=$big; [[ $which == small ]] && src=$small
    sips -z $size $size "$src" --out "$set_dir/$name.png" >/dev/null
done
iconutil -c icns "$set_dir" -o "$app/Contents/Resources/Tachyon.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Tachyon</string>
    <key>CFBundleDisplayName</key><string>Tachyon</string>
    <key>CFBundleIdentifier</key><string>dev.tachyon.terminal</string>
    <key>CFBundleExecutable</key><string>tachyon</string>
    <key>CFBundleIconFile</key><string>Tachyon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>14.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
PLIST

# Ad-hoc signature: Apple silicon will not run unsigned code, and a signed
# bundle keeps Gatekeeper and TCC prompts tied to one stable identity.
codesign --force --deep --sign - "$app"
echo "built $app ($version)"

if [[ ${1:-} == --install ]]; then
    dest=/Applications/Tachyon.app
    rm -rf "$dest"
    ditto "$app" "$dest"
    echo "installed $dest"
fi
