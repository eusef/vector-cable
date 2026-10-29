#!/bin/sh
# VectorCable launcher: double-click it in Finder and Terminal opens running
# vectorcable (Terminal.app is exempt from Local Network privacy, TN3179).
#
# Runs the vectorcable next to this file (an unpacked folder or a USB stick),
# else /usr/local/bin/vectorcable (where VectorCable.pkg installs it).
# Arguments pass through, for example: VectorCable.command --listen
#
# GPL-3.0.

here="$(cd "$(dirname "$0")" && pwd)"
for bin in "$here/vectorcable" /usr/local/bin/vectorcable; do
    if [ -f "$bin" ] && [ -x "$bin" ]; then
        exec "$bin" "$@"
    fi
done

printf '%s\n' \
    "VectorCable is not installed: there is no vectorcable next to this" \
    "launcher or in /usr/local/bin. On the bridge's Wi-Fi, install it with:" \
    "" \
    "  curl -fsSL http://10.42.0.1/downloads/install-mac.sh | sh" \
    ""
printf 'Press Enter to close. '
read -r _ || true
exit 1
