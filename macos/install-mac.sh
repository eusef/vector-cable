#!/bin/sh
# VectorCable macOS installer.
#
#   curl -fsSL http://10.42.0.1/downloads/install-mac.sh | sh
#   curl -fsSL http://10.42.0.1/downloads/install-mac.sh | sh -s -- --uninstall
#
# Downloads manifest.json and the macOS package it lists from the same place, checks the
# package's size and sha256 against the manifest, and installs it: the
# vectorcable command in /usr/local/bin, the VectorCable launcher in
# /Applications and the VectorBridge RX and TX audio plug-ins in
# /Library/Audio/Plug-Ins/HAL. Asks for the admin password once, then Core
# Audio restarts, which interrupts every app's audio for a few seconds.
# Set VECTORCABLE_BASE to download from somewhere else.
#
# Touches only VectorCable's own files. GPL-3.0.
set -eu

BASE="${VECTORCABLE_BASE:-http://10.42.0.1/downloads}"
HAL="/Library/Audio/Plug-Ins/HAL"
RX_DRIVER="$HAL/VectorBridgeRX.driver"
TX_DRIVER="$HAL/VectorBridgeTX.driver"
BIN="/usr/local/bin/vectorcable"
LAUNCHER="/Applications/VectorCable.command"
PKG_ID="io.github.eusef.vectorcable"

say() { printf '%s\n' "$*"; }
die() { printf 'install-mac.sh: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || die "this installer is for macOS"
[ "$(uname -m)" = "arm64" ] || die "this beta is built for Apple silicon only"

restart_core_audio() {
    say "Restarting Core Audio (audio in every app pauses for a few seconds)."
    sudo killall coreaudiod || die "could not restart Core Audio; restart the Mac to finish"
}

# Wait up to 15 s for both devices to appear (or disappear, with "gone").
wait_for_devices() {
    want="$1"
    i=0
    while [ "$i" -lt 15 ]; do
        list="$(system_profiler SPAudioDataType 2>/dev/null || true)"
        rx=no; tx=no
        case "$list" in *"VectorBridge RX:"*) rx=yes ;; esac
        case "$list" in *"VectorBridge TX:"*) tx=yes ;; esac
        if [ "$want" = present ] && [ "$rx" = yes ] && [ "$tx" = yes ]; then return 0; fi
        if [ "$want" = gone ] && [ "$rx" = no ] && [ "$tx" = no ]; then return 0; fi
        sleep 1
        i=$((i + 1))
    done
    return 1
}

# Prints "<name> <size> <sha256>" for the macOS arm64 .pkg in manifest $1
# (kind installer, os macos, arch arm64), or fails. The name carries the
# version, so it is read from the manifest rather than fixed here.
manifest_pkg() {
    i=0
    while name="$(plutil -extract "files.$i.name" raw -o - "$1" 2>/dev/null)"; do
        kind="$(plutil -extract "files.$i.kind" raw -o - "$1" 2>/dev/null || true)"
        os="$(plutil -extract "files.$i.os" raw -o - "$1" 2>/dev/null || true)"
        arch="$(plutil -extract "files.$i.arch" raw -o - "$1" 2>/dev/null || true)"
        case "$name" in
            *.pkg)
                if [ "$kind" = installer ] && [ "$os" = macos ] && [ "$arch" = arm64 ]; then
                    size="$(plutil -extract "files.$i.size" raw -o - "$1" 2>/dev/null)" || return 1
                    sha="$(plutil -extract "files.$i.sha256" raw -o - "$1" 2>/dev/null)" || return 1
                    case "$name" in */*|*' '*) return 1 ;; esac
                    printf '%s %s %s\n' "$name" "$size" "$sha"
                    return 0
                fi ;;
        esac
        i=$((i + 1))
    done
    return 1
}

uninstall() {
    if [ ! -e "$RX_DRIVER" ] && [ ! -e "$TX_DRIVER" ] && [ ! -e "$BIN" ] && [ ! -e "$LAUNCHER" ]; then
        say "VectorCable is not installed. Nothing to do."
        return 0
    fi
    say "Removing VectorCable (admin password needed):"
    for f in "$BIN" "$LAUNCHER" "$RX_DRIVER" "$TX_DRIVER"; do say "  $f"; done
    sudo rm -rf "$RX_DRIVER" "$TX_DRIVER"
    sudo rm -f "$BIN" "$LAUNCHER"
    sudo pkgutil --forget "$PKG_ID" >/dev/null 2>&1 || true
    restart_core_audio
    if wait_for_devices gone; then
        say "Removed. VectorBridge RX and TX are gone."
    else
        die "files removed, but Core Audio still lists a VectorBridge device; restart the Mac"
    fi
}

install() {
    work="$(mktemp -d -t vectorcable)"
    trap 'rm -rf "$work"' EXIT

    say "Downloading VectorCable from $BASE"
    curl -fsSL "$BASE/manifest.json" -o "$work/manifest.json" || die "download failed: $BASE/manifest.json"
    entry="$(manifest_pkg "$work/manifest.json")" || die "manifest.json lists no macOS (Apple silicon) package"
    PKG="${entry%% *}"
    rest="${entry#* }"
    want_size="${rest% *}"
    want_sha="${rest#* }"
    curl -fsSL "$BASE/$PKG" -o "$work/$PKG" || die "download failed: $BASE/$PKG"
    size="$(stat -f %z "$work/$PKG")"
    [ "$size" = "$want_size" ] || die "size mismatch: $PKG is $size bytes, manifest.json says $want_size"
    sha="$(shasum -a 256 "$work/$PKG" | cut -d ' ' -f 1)"
    [ "$sha" = "$want_sha" ] || die "checksum mismatch: $PKG is damaged or not the published file"

    # Spike 0a records whether curl-downloaded files carry a quarantine flag.
    if xattr -p com.apple.quarantine "$work/$PKG" >/dev/null 2>&1; then
        say "Note: the download carries com.apple.quarantine."
    else
        say "Note: the download carries no com.apple.quarantine."
    fi

    say "Installing (admin password needed)."
    sudo installer -pkg "$work/$PKG" -target / || die "the installer failed; see /var/log/install.log"
    # The package's postinstall restarted Core Audio.
    if wait_for_devices present; then
        say "Installed. VectorBridge RX and VectorBridge TX are ready."
        say "Start VectorCable from Applications, or run: vectorcable"
    else
        die "installed, but Core Audio has not listed VectorBridge RX and TX; run 'vectorcable doctor'"
    fi
}

case "${1:-}" in
    "") install ;;
    --uninstall) uninstall ;;
    *) die "unknown option: $1 (use --uninstall to remove)" ;;
esac
