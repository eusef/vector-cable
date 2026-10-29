# VectorCable on Linux

VectorCable turns a Linux laptop into the audio and rig-control link for a VectorBridge. While it
runs it adds two audio devices, **VectorBridge RX** (what the radio hears) and **VectorBridge TX**
(what you send), and relays Hamlib rig control so WSJT-X, JS8Call and FLDIGI work as if the radio
were plugged in.

Needs: a desktop with PipeWire (the default on Ubuntu 22.10, Fedora 34, Debian 12, Mint 22 and
later), an x86_64 or arm64 machine, on the bridge's Wi-Fi. No root and no driver.

## Install

1. Open a terminal.
2. Install the three PipeWire helpers VectorCable drives, if they are missing (Ubuntu's PipeWire
   leaves them out):

   ```
   sudo apt install pulseaudio-utils
   ```

   On Fedora: `sudo dnf install pulseaudio-utils`. On Arch: `sudo pacman -S libpulse`.
3. Paste this line and press Enter. Do not put `sudo` in front of it:

   ```
   curl -fsSL http://10.42.0.1/downloads/install-linux.sh | sh
   ```

   If the bridge is on your home Wi-Fi instead of its own access point, replace `10.42.0.1`
   with the address shown under Connect on the bridge's home page.
4. The installer ends with "Installed vectorcable" and the version.

What it installs, for your user only: the `vectorcable` command in `~/.local/bin` and
**VectorCable** in your applications menu. The installer picks the build for your machine and
checks it against the bridge's manifest before installing.

With internet instead of the bridge, the same installer runs from GitHub:

```
B=https://github.com/eusef/vector-cable/releases/latest/download
curl -fsSL $B/install-linux.sh | VECTORCABLE_BASE=$B sh
```

Prefer a download? The Downloads page has `vectorcable-<version>-linux-x86_64.tar.gz` and
`-linux-arm64.tar.gz`, each holding the `vectorcable` program and the menu launcher.

## First run

1. Open **VectorCable** from the applications menu (or run `vectorcable` in a terminal). A
   terminal window opens showing the Status tab. While it runs, **VectorBridge RX** and
   **VectorBridge TX** appear in your sound settings; they go away when you quit.
2. If the bridge has an admin PIN, Status says "press p". Press p and type the PIN. On Linux
   VectorCable asks for it each time it starts.
3. Status shows the bridge, the link and the audio buffer. The first run starts in
   **Listen Only**; press the key the screen names to switch to Transmit once the bridge
   shows the radio is ready.
4. Open the **Setup** tab (or run `vectorcable setup wsjt-x`, `js8call` or `fldigi`) and
   copy the settings into your app. The per-app guides below walk through each screen.

Start VectorCable before the radio app, so the app finds the two devices when it opens.

## Check it works

`vectorcable doctor` checks that PipeWire can make the two devices and that the bridge answers,
and prints a fix for each row that is not Good.

## Remove

```
curl -fsSL http://10.42.0.1/downloads/install-linux.sh | sh -s -- --uninstall
```

Settings in `~/.config/vectorcable` are kept; delete that folder to remove them too.

## Next

- [WSJT-X with VectorCable](vectorcable-wsjtx)
- [JS8Call with VectorCable](vectorcable-js8call)
- [FLDIGI with VectorCable](vectorcable-fldigi)
- [Troubleshooting](vectorcable-troubleshooting)
- [First 10 minutes at a shelter](vectorcable-shelter-checklist)
