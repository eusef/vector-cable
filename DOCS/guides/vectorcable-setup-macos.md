# VectorCable on a Mac

VectorCable turns a Mac into the audio and rig-control link for a VectorBridge. It adds two
audio devices, **VectorBridge RX** (what the radio hears) and **VectorBridge TX** (what you send),
and relays Hamlib rig control so WSJT-X, JS8Call and FLDIGI work as if the radio were plugged in.

Needs: a Mac with Apple silicon, macOS 12 or newer, on the bridge's Wi-Fi.

## Install

1. Open Terminal (Applications, Utilities).
2. Paste this line and press Return:

   ```
   curl -fsSL http://10.42.0.1/downloads/install-mac.sh | sh
   ```

   If the bridge is on your home Wi-Fi instead of its own access point, replace `10.42.0.1`
   with the address shown under Connect on the bridge's home page.
3. Type your Mac password when asked. Audio in every app pauses for a few seconds while
   Core Audio restarts.
4. The installer ends with "Installed. VectorBridge RX and VectorBridge TX are ready."

What it installs: the `vectorcable` command, **VectorCable** in Applications, and the two
audio plug-ins. The installer checks the package against the bridge's manifest before running it.

With internet instead of the bridge, the same installer runs from GitHub:

```
B=https://github.com/eusef/vector-cable/releases/latest/download
curl -fsSL $B/install-mac.sh | VECTORCABLE_BASE=$B sh
```

Prefer a download? The Downloads page has the `.pkg`. Because the beta package is not notarized,
macOS blocks the first open: go to System Settings, Privacy and Security, and press
**Open Anyway**, then run it again.

## First run

1. Double-click **VectorCable** in Applications. Terminal opens showing the Status tab.
2. macOS asks whether Terminal may use the microphone. Allow it: that is how your transmit
   audio reaches the bridge. If you pressed Don't Allow, turn it on under System Settings,
   Privacy and Security, Microphone, then restart Terminal.
3. Status shows the bridge, the link and the audio buffer. The first run starts in
   **Listen Only**; press the key the screen names to switch to Transmit once the bridge
   shows the radio is ready.
4. Open the **Setup** tab (or run `vectorcable setup wsjt-x`, `js8call` or `fldigi`) and
   copy the settings into your app. The per-app guides below walk through each screen.

## Check it works

`vectorcable doctor` tests the driver, the Microphone permission and the bridge, and prints a
fix for each row that is not Good.

## Remove

```
curl -fsSL http://10.42.0.1/downloads/install-mac.sh | sh -s -- --uninstall
```

## Next

- [WSJT-X with VectorCable](vectorcable-wsjtx)
- [JS8Call with VectorCable](vectorcable-js8call)
- [FLDIGI with VectorCable](vectorcable-fldigi)
- [Troubleshooting](vectorcable-troubleshooting)
- [First 10 minutes at a shelter](vectorcable-shelter-checklist)
