#!/usr/bin/env bash
# Builds AscFS.app: the host app with the asc FSKit extension embedded, signed for an app group.
#
#   scripts/build-app.sh --team TEAMID --sign "Developer ID Application: ..." [--out DIR] [--universal]
#   scripts/build-app.sh --adhoc [--out DIR]     # a build check only: macOS will not load it
#
# The app group is <TEAMID>.dev.asc.fskit; the daemon finds its container by that suffix
# (crates/asc-vfs/src/fskit/mod.rs). The FSKit entitlement must be granted to the team's
# provisioning profile: pass it with --profile for the extension when the identity needs one.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
out="$here/.build/app"
team=""
identity=""
profile=""
adhoc=0
universal=0
version="${ASC_VERSION:-0.1.0}"

while [ $# -gt 0 ]; do
  case "$1" in
    --team) team="$2"; shift 2 ;;
    --sign) identity="$2"; shift 2 ;;
    --profile) profile="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    --adhoc) adhoc=1; shift ;;
    --universal) universal=1; shift ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "build-app.sh: unknown argument $1" >&2; exit 2 ;;
  esac
done

if [ "$adhoc" = 1 ]; then
  team="${team:-ADHOC00000}"
  identity="-"
elif [ -z "$team" ] || [ -z "$identity" ]; then
  echo "build-app.sh: pass --team and --sign, or --adhoc for a build check" >&2
  exit 2
fi
case "$team" in
  *[!A-Z0-9]*|"") echo "build-app.sh: --team must be a team id (letters and digits)" >&2; exit 2 ;;
esac

group="$team.dev.asc.fskit"
bundle="dev.asc.fskit"

args=(-c release --package-path "$here")
if [ "$universal" = 1 ]; then
  args+=(--arch arm64 --arch x86_64)
fi
swift build "${args[@]}" --product AscFSExtension
swift build "${args[@]}" --product AscFS
bin="$(swift build "${args[@]}" --show-bin-path)"

app="$out/AscFS.app"
appex="$app/Contents/Extensions/AscFSExtension.appex"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$appex/Contents/MacOS"
cp "$bin/AscFS" "$app/Contents/MacOS/AscFS"
cp "$bin/AscFSExtension" "$appex/Contents/MacOS/AscFSExtension"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>$bundle</string>
  <key>CFBundleName</key><string>AscFS</string>
  <key>CFBundleDisplayName</key><string>asc file system</string>
  <key>CFBundleExecutable</key><string>AscFS</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>26.0</string>
  <key>LSUIElement</key><true/>
  <key>AscAppGroup</key><string>$group</string>
</dict>
</plist>
PLIST

cat > "$appex/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>$bundle.extension</string>
  <key>CFBundleName</key><string>AscFSExtension</string>
  <key>CFBundleDisplayName</key><string>asc</string>
  <key>CFBundleExecutable</key><string>AscFSExtension</string>
  <key>CFBundlePackageType</key><string>XPC!</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>26.0</string>
  <key>AscAppGroup</key><string>$group</string>
  <key>EXAppExtensionAttributes</key>
  <dict>
    <key>EXExtensionPointIdentifier</key><string>com.apple.fskit.fsmodule</string>
    <key>FSShortName</key><string>ascfs</string>
    <key>FSSupportsGenericURLResources</key><true/>
    <key>FSSupportedSchemes</key><array><string>ascfs</string></array>
    <key>FSPersonalities</key>
    <dict>
      <key>ascfs</key>
      <dict><key>FSName</key><string>asc work tree</string></dict>
    </dict>
  </dict>
</dict>
</plist>
PLIST

ents="$out/entitlements"
mkdir -p "$ents"
cat > "$ents/extension.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.security.app-sandbox</key><true/>
  <key>com.apple.security.application-groups</key><array><string>$group</string></array>
  <key>com.apple.developer.fskit.fsmodule</key><true/>
</dict>
</plist>
PLIST
cat > "$ents/app.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.security.app-sandbox</key><true/>
  <key>com.apple.security.application-groups</key><array><string>$group</string></array>
</dict>
</plist>
PLIST

if [ -n "$profile" ]; then
  cp "$profile" "$appex/Contents/embedded.provisionprofile"
fi
plutil -lint "$app/Contents/Info.plist" "$appex/Contents/Info.plist" "$ents/extension.plist" "$ents/app.plist"

sign=(codesign --force --sign "$identity")
if [ "$identity" != "-" ]; then
  sign+=(--options runtime --timestamp)
fi
"${sign[@]}" --entitlements "$ents/extension.plist" "$appex"
"${sign[@]}" --entitlements "$ents/app.plist" "$app"
codesign --verify --deep --strict "$app"
echo "built $app (app group $group)"
