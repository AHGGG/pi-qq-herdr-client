#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$project_root"
target="${1:?Pass a native Rust target (aarch64-apple-darwin, x86_64-apple-darwin or x86_64-unknown-linux-gnu)}"
case "$target" in
  aarch64-apple-darwin) platform=macos; arch=arm64 ;;
  x86_64-apple-darwin) platform=macos; arch=x64 ;;
  x86_64-unknown-linux-gnu) platform=linux; arch=x64 ;;
  *) echo "Unsupported release target: $target" >&2; exit 1 ;;
esac
cargo build --locked --release -p pi-qq-herdr-client --target "$target" -j "${CARGO_BUILD_JOBS:-4}"
version="$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')"
name="pi-qq-herdr-client-$platform-$arch"
binary="$project_root/target/$target/release/pi-qq-herdr-client"
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
mkdir -p dist

if [[ "$platform" == macos ]]; then
  app="$staging/Pi QQ Herdr Client.app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  install -m 755 "$binary" "$app/Contents/MacOS/pi-qq-herdr-client"
  python3 - "$app/Contents/Info.plist" "$version" <<'PY'
import plistlib
import sys
with open(sys.argv[1], "wb") as output:
    plistlib.dump({
        "CFBundleIdentifier": "dev.ahggg.pi-qq-herdr-client",
        "CFBundleName": "Pi QQ Herdr Client",
        "CFBundleDisplayName": "Pi QQ Herdr Client",
        "CFBundleExecutable": "pi-qq-herdr-client",
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": sys.argv[2],
        "CFBundleVersion": sys.argv[2],
        "CFBundleIconFile": "pi-qq.icns",
        "LSMinimumSystemVersion": "12.0",
        "NSHighResolutionCapable": True,
    }, output)
PY
  iconset="$staging/pi-qq.iconset"
  mkdir -p "$iconset"
  for size in 16 32 128 256 512; do
    sips -z "$size" "$size" crates/desktop/assets/pi-qq.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" crates/desktop/assets/pi-qq.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
  done
  iconutil -c icns "$iconset" -o "$app/Contents/Resources/pi-qq.icns"
  codesign --force --sign - "$app"
  codesign --verify --strict "$app"
  ln -s /Applications "$staging/Applications"
  # Only include the app and Applications shortcut in the disk image.
  rm -rf "$iconset"
  artifact="dist/$name.dmg"
  hdiutil create -volname 'Pi QQ Herdr Client' -srcfolder "$staging" -ov -format UDZO "$artifact"
  shasum -a 256 "$artifact" | sed 's|  dist/|  |' > "$artifact.sha256"
else
  package="$staging/package"
  mkdir -p "$package/DEBIAN" "$package/usr/bin" "$package/usr/share/applications" \
    "$package/usr/share/icons/hicolor/256x256/apps" "$package/usr/share/doc/pi-qq-herdr-client"
  install -m 755 "$binary" "$package/usr/bin/pi-qq-herdr-client"
  install -m 644 crates/desktop/assets/pi-qq.png "$package/usr/share/icons/hicolor/256x256/apps/pi-qq-herdr-client.png"
  install -m 644 README.md "$package/usr/share/doc/pi-qq-herdr-client/README.md"
  cat > "$package/usr/share/applications/pi-qq-herdr-client.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Pi QQ Herdr Client
Comment=Use Pi from QQ
Exec=pi-qq-herdr-client
Icon=pi-qq-herdr-client
Terminal=false
Categories=Development;Utility;
DESKTOP
  mkdir -p "$staging/debian"
  cat > "$staging/debian/control" <<'CONTROL'
Source: pi-qq-herdr-client
Section: utils
Priority: optional
Maintainer: AHGGG <noreply@github.com>

Package: pi-qq-herdr-client
Architecture: amd64
Description: Use Pi from QQ
CONTROL
  # Resolve the actual shared libraries used by this build, including GPUI/GTK.
  dependencies="$(cd "$staging" && dpkg-shlibdeps -O -e"$binary" | sed -n 's/^shlibs:Depends=//p')"
  [[ -n "$dependencies" ]] || { echo 'Could not determine Linux dependencies.' >&2; exit 1; }
  cat > "$package/DEBIAN/control" <<CONTROL
Package: pi-qq-herdr-client
Version: $version
Architecture: amd64
Section: utils
Priority: optional
Maintainer: AHGGG <noreply@github.com>
Depends: $dependencies, libayatana-appindicator3-1, gnome-keyring
Description: Use Pi from QQ
 Send tasks, receive replies and manage Pi sessions from QQ.
CONTROL
  artifact="dist/$name.deb"
  dpkg-deb --root-owner-group --build "$package" "$artifact"
  dpkg-deb --info "$artifact"
  sha256sum "$artifact" | sed 's|  dist/|  |' > "$artifact.sha256"
fi
printf '%s\n' "$artifact"
