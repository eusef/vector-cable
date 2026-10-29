#!/bin/bash
# Build the two VectorBridge HAL plug-ins from a pinned BlackHole commit,
# ad-hoc signed. macos/pkg/build.sh then packages them into VectorCable.pkg.
#
# Output (target/macos/):
#   VectorBridgeRX.driver, VectorBridgeTX.driver   ad-hoc signed bundles
#
# Needs Xcode and network access to github.com on the first run. No sudo.
set -euo pipefail

BLACKHOLE_URL="https://github.com/ExistentialAudio/BlackHole.git"
# v0.7.1 plus the kLatency_Frame_Size #ifndef guard and the deployment-target
# bump to 12.0 that Xcode 27 needs.
BLACKHOLE_COMMIT="62953f570329839e6188f02cc28934904147f617"

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
out="$repo/target/macos"
src="$out/blackhole"

mkdir -p "$out"
if [ ! -d "$src/.git" ]; then
    git clone --quiet "$BLACKHOLE_URL" "$src"
fi
if ! git -C "$src" cat-file -e "$BLACKHOLE_COMMIT^{commit}" 2>/dev/null; then
    git -C "$src" fetch --quiet origin
fi
git -C "$src" checkout --quiet --detach "$BLACKHOLE_COMMIT"

# name  config  factory UUID (fixed, so a reinstall replaces the same plug-in)
build_one() {
    local name="$1" config="$2" uuid="$3"
    local bundle_id
    bundle_id="$(sed -n 's/^#define kPlugIn_BundleID *"\(.*\)"$/\1/p' "$config")"
    [ -n "$bundle_id" ] || { echo "no kPlugIn_BundleID in $config" >&2; exit 1; }

    rm -rf "$out/build-$name" "$out/$name.driver"
    xcodebuild \
        -project "$src/BlackHole.xcodeproj" \
        -configuration Release \
        -target BlackHole \
        CONFIGURATION_BUILD_DIR="$out/build-$name" \
        SYMROOT="$out/xcode-$name" \
        OBJROOT="$out/xcode-$name/obj" \
        PRODUCT_NAME="$name" \
        PRODUCT_BUNDLE_IDENTIFIER="$bundle_id" \
        OTHER_CFLAGS="-include $config" \
        CODE_SIGN_IDENTITY="-" \
        CODE_SIGN_STYLE=Manual \
        DEVELOPMENT_TEAM="" \
        -quiet

    mv "$out/build-$name/$name.driver" "$out/$name.driver"
    rm -rf "$out/build-$name"

    # Upstream's factory UUID, replaced per plug-in (as upstream's installer does).
    local plist="$out/$name.driver/Contents/Info.plist"
    grep -q e395c745-4eea-4d94-bb92-46224221047c "$plist" \
        || { echo "upstream factory UUID not found in $plist" >&2; exit 1; }
    sed -i '' "s/e395c745-4eea-4d94-bb92-46224221047c/$uuid/g" "$plist"

    # Upstream's target already copies its GPL-3.0 LICENSE into Resources.
    [ -f "$out/$name.driver/Contents/Resources/LICENSE" ] \
        || { echo "LICENSE missing from $name.driver" >&2; exit 1; }

    # Debug symbols name the builder's source and build paths; ship none.
    strip -S "$out/$name.driver/Contents/MacOS/$name"

    # Sign after the plist edit and the strip, which would otherwise break the signature.
    codesign --force --sign - "$out/$name.driver"
    codesign --verify --strict "$out/$name.driver"
}

build_one VectorBridgeRX "$here/config/rx.h" E3DD71B8-7DAB-46DA-B533-DC33F7BC4ACF
build_one VectorBridgeTX "$here/config/tx.h" 71F10108-D490-4BBA-8B52-B9EE129BBE72

echo "Built:"
for name in VectorBridgeRX VectorBridgeTX; do
    echo "  $out/$name.driver ($(lipo -archs "$out/$name.driver/Contents/MacOS/$name"))"
done
