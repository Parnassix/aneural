#!/usr/bin/env bash
#
# Build Aneural.app and install it, so the graph opens from the Dock like
# anything else rather than from a checkout with a cargo command.
#
#   scripts/bundle-macos.sh              # build and install to /Applications
#   scripts/bundle-macos.sh --no-install # leave it in target/ and stop
#   scripts/bundle-macos.sh --to ~/Applications
#
# Two things here are load-bearing rather than incidental:
#
#   * It builds **without** the `dev` feature. That feature turns on Bevy's
#     dynamic linking, which leaves the binary pointing at a `libbevy_dylib`
#     inside `target/` — fine from a checkout, a crash the moment the app is
#     opened from the Dock. A plain release build links nothing outside
#     `/usr/lib` and `/System`, so the bundle is self-contained.
#   * It signs the bundle ad hoc. An unsigned app built locally is not
#     quarantined and will open, but macOS caches an app's identity by
#     signature, and an unsigned binary that changes underneath a bundle of
#     the same name confuses that cache — keychain prompts and lost window
#     state. `codesign -s -` costs nothing and makes replacing the app clean.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo="${CARGO:-$HOME/.cargo/bin/cargo}"
command -v "$cargo" >/dev/null 2>&1 || cargo=cargo

install_to="/Applications"
do_install=1
while [ $# -gt 0 ]; do
  case "$1" in
    --no-install) do_install=0; shift ;;
    --to) install_to="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

version="$(awk '/^\[workspace.package\]/{f=1} f && /^version/{gsub(/[",]/,"");print $3;exit}' "$root/Cargo.toml")"
app="$root/target/Aneural.app"
contents="$app/Contents"

echo "==> building aneural-gui $version (release, no dynamic linking)"
"$cargo" build --release -p aneural-gui --manifest-path "$root/Cargo.toml"

echo "==> assembling $app"
rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Resources"
cp "$root/target/release/aneural-gui" "$contents/MacOS/aneural-gui"

echo "==> rendering the icon"
icon_bin="$(mktemp -t aneural-icon)"
swiftc -O "$root/scripts/icon.swift" -o "$icon_bin"
iconset="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$iconset"
# Each size is rendered rather than downscaled from one big one: the mark is
# thin, and a 16-point strand that has been resampled is a grey smudge.
for pair in "16 icon_16x16" "32 icon_16x16@2x" "32 icon_32x32" "64 icon_32x32@2x" \
            "128 icon_128x128" "256 icon_128x128@2x" "256 icon_256x256" \
            "512 icon_256x256@2x" "512 icon_512x512" "1024 icon_512x512@2x"; do
  set -- $pair
  "$icon_bin" "$iconset/$2.png" "$1" >/dev/null
done
iconutil -c icns "$iconset" -o "$contents/Resources/AppIcon.icns"
cp "$iconset/icon_512x512@2x.png" "$root/target/aneural-icon.png"

echo "==> writing Info.plist"
cat > "$contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>              <string>Aneural</string>
  <key>CFBundleDisplayName</key>       <string>Aneural</string>
  <key>CFBundleExecutable</key>        <string>aneural-gui</string>
  <key>CFBundleIdentifier</key>        <string>dev.aneural.gui</string>
  <key>CFBundleIconFile</key>          <string>AppIcon</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key> <string>6.0</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key>           <string>$version</string>
  <key>LSMinimumSystemVersion</key>    <string>13.0</string>
  <key>LSApplicationCategoryType</key> <string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key>   <true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key> <true/>
  <!-- Opened with no argument it asks which folder to grow; the picker is a
       native panel, which is the one thing a bundle buys that a bare binary
       does not. -->
  <key>NSHumanReadableCopyright</key>  <string>MIT OR Apache-2.0</string>
</dict>
</plist>
PLIST

echo "==> signing (ad hoc)"
codesign --force --deep --sign - "$app"

if [ "$do_install" -eq 1 ]; then
  dest="$install_to/Aneural.app"
  echo "==> installing to $dest"
  mkdir -p "$install_to"
  rm -rf "$dest"
  cp -R "$app" "$dest"
  # Nudge Launch Services so the icon and the name are right immediately
  # rather than whenever it next rebuilds its database.
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
    -f "$dest" >/dev/null 2>&1 || true
  echo
  echo "Installed. Open it from the Dock or Spotlight, or point it at a folder:"
  echo "  open -a Aneural --args ~/Code/my-project"
else
  echo
  echo "Built at $app (not installed)."
fi
