#!/bin/bash
# Build VectorCable.pkg (unsigned in the beta; Developer ID is D3) and the
# files the bridge serves from /downloads/, for a local or bridge install.
#
# Output (target/macos/dist/):
#   vectorcable-<version>-macos-arm64.pkg
#                      /usr/local/bin/vectorcable (release, arm64),
#                      /Applications/VectorCable.command and both HAL plug-ins;
#                      its postinstall restarts Core Audio
#   install-mac.sh     copied from macos/
#   LICENSE, the bare binary and a GPL source tarball (this repo plus the
#                      BlackHole checkout)
#   vectorcable-<version>-linux-{x86_64,arm64}.tar.gz and install-linux.sh
#                      copied from target/linux/dist/ (linux/build.sh)
#   manifest.json      the bridge's /downloads/ contract (BRIDGE, 2026-10-27):
#                      name, version, commit, released, protocol, release_url,
#                      notes, and per file name, kind, os, arch, min_os, size, sha256
# Every file is a GitHub Release asset, tag vectorcable-v<version>; the bridge
# image build fetches them by that tag and checks the sha256s.
#
# Needs the plug-ins from macos/driver/build.sh and the Linux files from
# linux/build.sh (same commit) first. No sudo.
set -euo pipefail

IDENTIFIER="io.github.eusef.vectorcable"

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
out="$repo/target/macos"
stage="$out/pkg"
dist="$out/dist"

for d in VectorBridgeRX.driver VectorBridgeTX.driver; do
    [ -d "$out/$d" ] || { echo "$out/$d missing: run macos/driver/build.sh first" >&2; exit 1; }
done
linux_dist="$repo/target/linux/dist"
[ -f "$linux_dist/COMMIT" ] || { echo "$linux_dist missing: run linux/build.sh first" >&2; exit 1; }
[ "$(cat "$linux_dist/COMMIT")" = "$(git -C "$repo" rev-parse --short HEAD)" ] ||
    { echo "$linux_dist was built from $(cat "$linux_dist/COMMIT"), not HEAD: run linux/build.sh again" >&2; exit 1; }

# No builder's paths in the shipped binary: panic locations name the cargo
# registry and the repo. rustc uses the last matching prefix, so the most
# specific comes last.
RUSTFLAGS="--remap-path-prefix=$HOME=/home --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=$repo=/vectorcable" \
    cargo build --quiet --release -p vectorcable --target aarch64-apple-darwin --manifest-path "$repo/Cargo.toml"
bin="$repo/target/aarch64-apple-darwin/release/vectorcable"
# "vectorcable 0.1.0 (abc1234) (VectorBridge protocol 1)"
line="$("$bin" version)"
version="$(printf '%s\n' "$line" | sed -n 's/^vectorcable \([^ ]*\).*/\1/p')"
protocol="$(printf '%s\n' "$line" | sed -n 's/.*VectorBridge protocol \([0-9]*\).*/\1/p')"
commit="$(git -C "$repo" rev-parse --short HEAD)"
[ -n "$version" ] && [ -n "$protocol" ] || { echo "cannot read the version from: $line" >&2; exit 1; }

rm -rf "$stage" "$dist"
root="$stage/root"
mkdir -p "$root/usr/local/bin" "$root/Applications" "$root/Library/Audio/Plug-Ins/HAL" "$stage/scripts" "$dist"
install -m 755 "$bin" "$root/usr/local/bin/vectorcable"
install -m 755 "$repo/macos/VectorCable.command" "$root/Applications/VectorCable.command"
for d in VectorBridgeRX.driver VectorBridgeTX.driver; do
    ditto "$out/$d" "$root/Library/Audio/Plug-Ins/HAL/$d"
    codesign --verify --strict "$root/Library/Audio/Plug-Ins/HAL/$d"
done
leaks="$(grep -r -a -l -F "$HOME" "$root" || true)"
[ -z "$leaks" ] || { echo "the builder's home path is in: $leaks" >&2; exit 1; }

cat > "$stage/scripts/postinstall" <<'EOF'
#!/bin/sh
# Core Audio loads HAL plug-ins only at start; audio in every app pauses briefly.
killall coreaudiod 2>/dev/null || true
exit 0
EOF
chmod 755 "$stage/scripts/postinstall"
# The staged files carry com.apple.provenance, which cannot be removed, so
# pkgutil --payload-files lists ._ entries. FACT (pkgutil --expand-full,
# 2026-10-07): they unpack as extended attributes, not as files.

# Never relocate the plug-ins to a copy elsewhere on the disk, and always
# replace them, so a reinstall of the same version is a real reinstall.
plist="$stage/component.plist"
pkgbuild --quiet --analyze --root "$root" "$plist"
i=0
while plutil -extract "$i" xml1 -o /dev/null "$plist" 2>/dev/null; do
    plutil -replace "$i.BundleIsRelocatable" -bool NO "$plist"
    plutil -replace "$i.BundleIsVersionChecked" -bool NO "$plist"
    i=$((i + 1))
done

pkgbuild --quiet \
    --root "$root" \
    --component-plist "$plist" \
    --scripts "$stage/scripts" \
    --identifier "$IDENTIFIER" \
    --version "$version" \
    --install-location / \
    --ownership recommended \
    "$dist/VectorCable.pkg"
# Release assets, named as the bridge's /downloads/ contract asks (lowercase
# product, version, os, arch). install-mac.sh keeps its name: the curl line in
# the docs and the launcher must not change from release to release.
pkg_name="vectorcable-$version-macos-arm64.pkg"
bin_name="vectorcable-$version-macos-arm64"
src_name="vectorcable-$version-src.tar.gz"
mv "$dist/VectorCable.pkg" "$dist/$pkg_name"
install -m 755 "$bin" "$dist/$bin_name"
install -m 644 "$repo/macos/install-mac.sh" "$dist/install-mac.sh"
install -m 644 "$repo/LICENSE" "$dist/LICENSE"
linux_x86_name="vectorcable-$version-linux-x86_64.tar.gz"
linux_arm_name="vectorcable-$version-linux-arm64.tar.gz"
for f in "$linux_x86_name" "$linux_arm_name" install-linux.sh; do
    [ -f "$linux_dist/$f" ] || { echo "$linux_dist/$f missing: run linux/build.sh again" >&2; exit 1; }
    install -m 644 "$linux_dist/$f" "$dist/$f"
done

# GPL source for the shipped version (D6): this repo at HEAD plus the BlackHole
# checkout the driver was built from, so the card carries the driver's source too.
srcstage="$stage/src"
mkdir -p "$srcstage/vectorcable-$version"
git -C "$repo" archive --format=tar HEAD | tar -x -C "$srcstage/vectorcable-$version"
# The shipped source is the public repo's file set (neutral wording rule): the
# working records, session files and dev tooling stay out, by the same
# EXCLUDE pattern scripts/dev/export-public.sh uses. The guides, LICENSE,
# README and notices ship. A public checkout has no scripts/dev/ and is
# already that file set, so it ships as it is.
export_script="$repo/scripts/dev/export-public.sh"
if [ -f "$export_script" ]; then
    EXCLUDE="$(sed -n "s/^EXCLUDE='\(.*\)'\$/\1/p" "$export_script")"
    [ -n "$EXCLUDE" ] || { echo "cannot read EXCLUDE from $export_script" >&2; exit 1; }
    git -C "$repo" ls-tree -r --name-only HEAD | { grep -E "$EXCLUDE" || true; } | while IFS= read -r drop; do
        rm -f "${srcstage:?}/vectorcable-$version/$drop"
    done
    find "$srcstage/vectorcable-$version" -type d -empty -delete
    left="$(cd "$srcstage/vectorcable-$version" && find . -type f | sed 's|^\./||' | grep -E "$EXCLUDE" || true)"
    [ -z "$left" ] || { echo "excluded files left in the source tarball: $left" >&2; exit 1; }
fi
mkdir -p "$srcstage/vectorcable-$version/third_party/blackhole"
git -C "$out/blackhole" archive --format=tar HEAD | tar -x -C "$srcstage/vectorcable-$version/third_party/blackhole"
# No builder's user name: the entries are owned by root (0:0), as in the
# Linux tarballs.
COPYFILE_DISABLE=1 tar -C "$srcstage" --no-xattrs --uid 0 --gid 0 --uname root --gname root \
    -czf "$dist/$src_name" "vectorcable-$version"
# A release is built from a clean tree, so the tarball, the binary and the
# manifest's commit agree. VECTORCABLE_ALLOW_DIRTY=1 is for local checks only.
if [ -n "$(git -C "$repo" status --porcelain)" ] && [ "${VECTORCABLE_ALLOW_DIRTY:-}" != 1 ]; then
    echo "the working tree is dirty: commit first, or set VECTORCABLE_ALLOW_DIRTY=1 for a local check build" >&2
    exit 1
fi

# One manifest entry: name, kind, os, arch, min_os, size, sha256.
# min_os 12.0 is the driver's deployment target (macos/driver/build.sh).
entry() {
    name="$1"; kind="$2"; os="$3"; arch="$4"; min_os="$5"
    size="$(stat -f %z "$dist/$name")"
    sha="$(shasum -a 256 "$dist/$name" | cut -d ' ' -f 1)"
    printf '    {"name": "%s", "kind": "%s", "os": "%s", "arch": "%s"' "$name" "$kind" "$os" "$arch"
    [ -z "$min_os" ] || printf ', "min_os": "%s"' "$min_os"
    printf ', "size": %s, "sha256": "%s"}' "$size" "$sha"
}
release_url="https://github.com/eusef/vector-cable/releases/tag/vectorcable-v$version"
{
    printf '{\n  "name": "VectorCable",\n  "version": "%s",\n  "commit": "%s",\n  "released": "%s",\n  "protocol": %s,\n' \
        "$version" "$commit" "$(date -u +%F)" "$protocol"
    printf '  "release_url": "%s",\n' "$release_url"
    printf '  "notes": "Audio cable and rig control relay for WSJT-X, JS8Call and FLDIGI on a Mac (Apple silicon, macOS 12 or newer) or a Linux desktop with PipeWire (x86_64 or arm64).",\n'
    printf '  "files": [\n'
    entry "$pkg_name" installer macos arm64 12.0; printf ',\n'
    entry "$bin_name" binary macos arm64 12.0; printf ',\n'
    entry install-mac.sh installer macos any ""; printf ',\n'
    entry "$linux_x86_name" binary linux x86_64 ""; printf ',\n'
    entry "$linux_arm_name" binary linux arm64 ""; printf ',\n'
    entry install-linux.sh installer linux any ""; printf ',\n'
    entry LICENSE docs any any ""; printf ',\n'
    entry "$src_name" source any any ""
    printf '\n  ]\n}\n'
} > "$dist/manifest.json"
# plutil reads JSON (as install-mac.sh does) but -lint does not.
plutil -convert xml1 -o /dev/null "$dist/manifest.json"
for f in "$dist"/*; do
    [ "$(stat -f %z "$f")" -le 26214400 ] || { echo "$f is over the bridge's 25 MB per-file cap" >&2; exit 1; }
done
# A .sha256 next to each asset for people (BRIDGE's release rule, 2026-10-27);
# manifest.json is the one machines read. "shasum -a 256 -c" format.
for f in "$pkg_name" "$bin_name" install-mac.sh "$linux_x86_name" "$linux_arm_name" install-linux.sh LICENSE "$src_name"; do
    (cd "$dist" && shasum -a 256 "$f" > "$f.sha256")
done
total="$(cat "$dist"/* | wc -c | tr -d ' ')"
[ "$total" -le 52428800 ] || { echo "the release is $total bytes, over the bridge's 50 MB total cap" >&2; exit 1; }

echo "Built $dist:"
ls -l "$dist"
