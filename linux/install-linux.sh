#!/bin/sh
# VectorCable Linux installer.
#
#   curl -fsSL http://10.42.0.1/downloads/install-linux.sh | sh
#   curl -fsSL http://10.42.0.1/downloads/install-linux.sh | sh -s -- --uninstall
#
# Downloads manifest.json and the tarball it lists for this machine's
# architecture (x86_64 or arm64) from the same place, checks the tarball's
# size and sha256 against the manifest, and installs for the current user, no
# root: the vectorcable command in ~/.local/bin and the VectorCable launcher in
# ~/.local/share/applications. There is no driver: vectorcable makes the
# VectorBridge RX and TX devices in PipeWire while it runs.
# Set VECTORCABLE_BASE to download from somewhere else.
#
# Touches only VectorCable's own files. GPL-3.0.
set -eu

BASE="${VECTORCABLE_BASE:-http://10.42.0.1/downloads}"
BIN_DIR="$HOME/.local/bin"
BIN="$BIN_DIR/vectorcable"
APPS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
LAUNCHER="$APPS_DIR/vectorcable.desktop"

say() { printf '%s\n' "$*"; }
die() { printf 'install-linux.sh: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Linux" ] || die "this installer is for Linux; on a Mac use install-mac.sh"
[ "$(id -u)" != 0 ] || die "run it as your own user, without sudo: VectorCable installs into your home folder"

case "$(uname -m)" in
    x86_64 | amd64) ARCH=x86_64 ;;
    aarch64 | arm64) ARCH=arm64 ;;
    *) die "no build for $(uname -m); VectorCable is built for x86_64 and arm64" ;;
esac

# The value of "key" in one manifest entry ($1), string or number.
field() {
    printf '%s\n' "$1" | sed -n "s/.*\"$2\": *\"\{0,1\}\([^\",}]*\).*/\1/p"
}

# Prints "<name> <size> <sha256>" for the Linux tarball in manifest $1 for
# architecture $2 (kind binary, os linux, a .tar.gz), or nothing. The name
# carries the version, so it is read from the manifest rather than fixed here.
manifest_tarball() {
    # One entry per line, however the manifest was laid out.
    tr -d '\r\n' < "$1" | tr '{}' '\n\n' | while IFS= read -r entry; do
        [ "$(field "$entry" os)" = linux ] || continue
        [ "$(field "$entry" arch)" = "$2" ] || continue
        [ "$(field "$entry" kind)" = binary ] || continue
        name="$(field "$entry" name)"
        case "$name" in */* | *' '* | '') continue ;; *.tar.gz) ;; *) continue ;; esac
        printf '%s %s %s\n' "$name" "$(field "$entry" size)" "$(field "$entry" sha256)"
        break
    done
}

# pactl, pacat and parec make and feed the devices; a plain PipeWire install
# can lack them.
check_audio_tools() {
    for tool in pactl pacat parec; do
        command -v "$tool" >/dev/null 2>&1 && continue
        say "Note: $tool is missing, so VectorCable cannot make its audio devices yet. Install it:"
        say "  Debian, Ubuntu, Mint:  sudo apt install pulseaudio-utils"
        say "  Fedora:                sudo dnf install pulseaudio-utils"
        say "  Arch:                  sudo pacman -S libpulse"
        return
    done
}

uninstall() {
    if [ ! -e "$BIN" ] && [ ! -e "$LAUNCHER" ]; then
        say "VectorCable is not installed. Nothing to do."
        return 0
    fi
    say "Removing VectorCable:"
    for f in "$BIN" "$LAUNCHER"; do say "  $f"; done
    rm -f "$BIN" "$LAUNCHER"
    say "Removed. Settings in ~/.config/vectorcable are kept; delete that folder to remove them too."
}

# Not named install: the body runs the install command.
do_install() {
    command -v curl >/dev/null 2>&1 || die "curl not found"
    command -v sha256sum >/dev/null 2>&1 || die "sha256sum not found"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT

    say "Downloading VectorCable from $BASE"
    curl -fsSL "$BASE/manifest.json" -o "$work/manifest.json" || die "download failed: $BASE/manifest.json"
    entry="$(manifest_tarball "$work/manifest.json" "$ARCH")"
    [ -n "$entry" ] || die "manifest.json lists no Linux ($ARCH) build"
    TARBALL="${entry%% *}"
    rest="${entry#* }"
    want_size="${rest% *}"
    want_sha="${rest#* }"
    curl -fsSL "$BASE/$TARBALL" -o "$work/$TARBALL" || die "download failed: $BASE/$TARBALL"
    size="$(wc -c < "$work/$TARBALL" | tr -d ' ')"
    [ "$size" = "$want_size" ] || die "size mismatch: $TARBALL is $size bytes, manifest.json says $want_size"
    sha="$(sha256sum "$work/$TARBALL" | cut -d ' ' -f 1)"
    [ "$sha" = "$want_sha" ] || die "checksum mismatch: $TARBALL is damaged or not the published file"

    tar -xzf "$work/$TARBALL" -C "$work" || die "could not unpack $TARBALL"
    dir="$work/${TARBALL%.tar.gz}"
    [ -f "$dir/vectorcable" ] && [ -f "$dir/vectorcable.desktop" ] || die "$TARBALL is missing vectorcable or vectorcable.desktop"

    mkdir -p "$BIN_DIR" "$APPS_DIR"
    # install replaces the file, so a copy that is running keeps working.
    install -m 755 "$dir/vectorcable" "$BIN"
    # The launcher names the full path: a desktop session may not have
    # ~/.local/bin on its PATH.
    {
        grep -v '^Exec=' "$dir/vectorcable.desktop"
        printf 'Exec="%s"\n' "$BIN"
    } > "$work/vectorcable.desktop"
    install -m 644 "$work/vectorcable.desktop" "$LAUNCHER"
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$APPS_DIR" >/dev/null 2>&1 || true
    fi

    version="$("$BIN" version)" || die "installed $BIN, but it does not run on this system"
    say "Installed $version"
    say "  $BIN"
    say "  $LAUNCHER"
    check_audio_tools
    case ":$PATH:" in
        *":$BIN_DIR:"*) say "Start VectorCable from your applications menu, or run: vectorcable" ;;
        *) say "Start VectorCable from your applications menu, or run: $BIN"
           say "($BIN_DIR is not on your PATH in this shell; it usually is after you log in again.)" ;;
    esac
}

case "${1:-}" in
    "") do_install ;;
    --uninstall) uninstall ;;
    *) die "unknown option: $1 (use --uninstall to remove)" ;;
esac
