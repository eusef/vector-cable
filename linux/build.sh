#!/bin/bash
# Build the Linux release files: one tarball per architecture plus
# install-linux.sh. macos/pkg/build.sh then lists them in manifest.json, so
# run this first, from the same commit.
#
# Output (target/linux/dist/):
#   vectorcable-<version>-linux-x86_64.tar.gz
#   vectorcable-<version>-linux-arm64.tar.gz
#                      each: vectorcable-<version>-linux-<arch>/ with the
#                      vectorcable binary, vectorcable.desktop, LICENSE, README.md
#   install-linux.sh   copied from linux/
#   COMMIT             the short hash the binaries were built from (not an asset)
#
# The binaries link glibc, built in a rust:1.88-bullseye container (glibc
# 2.31), so they run on Ubuntu 20.04, Debian 11, Fedora 32 and newer. glibc,
# not a static musl build: getaddrinfo then goes through NSS, so
# vectorbridge.local resolves through avahi as it does on the desktop.
# Needs Docker; arm64 builds natively on Apple silicon, x86_64 under emulation.
set -euo pipefail

# 1.88: the oldest Rust the locked ratatui 0.30.2 builds with.
IMAGE="rust:1.88-bullseye"

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
out="$repo/target/linux"
dist="$out/dist"

if [ -n "$(git -C "$repo" status --porcelain)" ] && [ "${VECTORCABLE_ALLOW_DIRTY:-}" != 1 ]; then
    echo "the working tree is dirty: commit first, or set VECTORCABLE_ALLOW_DIRTY=1 for a local check build" >&2
    exit 1
fi
command -v docker >/dev/null || { echo "docker not found: the Linux build runs in a container" >&2; exit 1; }

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -1)"
commit="$(git -C "$repo" rev-parse --short HEAD)"
[ -n "$version" ] || { echo "cannot read the workspace version from Cargo.toml" >&2; exit 1; }

mkdir -p "$out/cargo-home"
rm -rf "$dist"
mkdir -p "$dist"

# <docker platform> <asset arch>
for pair in "linux/amd64 x86_64" "linux/arm64 arm64"; do
    platform="${pair% *}"
    arch="${pair#* }"
    # Run as the caller's user so target/ stays writable from the Mac. Docker
    # Desktop shows the mounted /src as owned by root, so git refuses it as
    # dubious and build.rs would bake no commit hash: trust /src in the
    # container's global git config (git 2.30 reads safe.directory only
    # there). Then the binary must name the commit, as the macOS one does.
    built="$(docker run --rm --platform "$platform" \
        --user "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -e CARGO_HOME=/src/target/linux/cargo-home \
        -e CARGO_TARGET_DIR="/src/target/linux/$arch" \
        -v "$repo:/src" -w /src \
        "$IMAGE" sh -c 'git config --global --add safe.directory /src &&
            cargo clean --quiet --release -p vectorcable &&
            cargo build --quiet --locked --release -p vectorcable &&
            "$CARGO_TARGET_DIR/release/vectorcable" version')"
    case "$built" in
        "vectorcable $version ($commit)"*) ;;
        *) echo "the $arch build says \"$built\", not version $version at $commit" >&2; exit 1 ;;
    esac
    bin="$out/$arch/release/vectorcable"
    file "$bin" | grep -q "ELF 64-bit" || { echo "$bin is not a Linux binary" >&2; exit 1; }

    name="vectorcable-$version-linux-$arch"
    stage="$out/stage/$name"
    rm -rf "$out/stage"
    mkdir -p "$stage"
    install -m 755 "$bin" "$stage/vectorcable"
    install -m 644 "$here/vectorcable.desktop" "$stage/vectorcable.desktop"
    install -m 644 "$repo/LICENSE" "$stage/LICENSE"
    install -m 644 "$repo/README.md" "$stage/README.md"
    # No ._ files or extended attributes from the Mac in a Linux tarball, and
    # no builder's user name: the entries are owned by root (0:0).
    COPYFILE_DISABLE=1 tar -C "$out/stage" --no-xattrs --uid 0 --gid 0 --uname root --gname root \
        -czf "$dist/$name.tar.gz" "$name"
done
rm -rf "$out/stage"
install -m 644 "$here/install-linux.sh" "$dist/install-linux.sh"
printf '%s\n' "$commit" > "$dist/COMMIT"

echo "Built $dist:"
ls -l "$dist"
