# Changelog

## 0.2.0 (2026-10-29, beta)

Adds Linux. macOS stays Apple silicon, unsigned.

- Linux: a tarball per architecture (x86_64 and arm64) with the `vectorcable` binary and a
  VectorCable launcher, and `install-linux.sh`, which installs both for the current user in
  `~/.local` (no root) after checking the download against `manifest.json`. Needs a desktop with
  PipeWire and glibc 2.31 or newer. Checked running on Ubuntu 26.04; the VectorBridge RX and TX
  devices exist while VectorCable runs.
  `doctor` and the Check tab name `pulseaudio-utils` when `pactl`, `pacat` or `parec` is missing.
- Rig control relay: follows the bridge's rule that every app off the Pi logs in for port 4532.
  A revoked token is forgotten and the PIN asked for again; a refused or closed connection is
  retried twice; `--rig-port N` (or `rig_port` in the config file) moves the relay off 4532.
- With a bridge that requires log-in for 4532, the Check tab shows rig control Good in every
  access mode.
- `vectorcable version` names the exact commit the binary was built from.

## 0.1.0 (2026-10-27, beta)

First release for the VectorBridge card's `/downloads/` page. macOS, Apple silicon, unsigned.

- VectorBridge RX and TX audio devices (Core Audio HAL plug-ins) and a terminal UI with
  Status, Setup, Levels and Check tabs, plus `setup`, `doctor`, `soak` and `version`.
- RX jitter buffer with drift trim; TX silence gate and transmit time cap.
- Bridge PIN log-in with a per-device app token kept in the login Keychain; the PIN is never stored.
- Local rig control relay on 127.0.0.1:4532 so WSJT-X, JS8Call and FLDIGI reach the bridge's
  Hamlib proxy, including over a tailnet.
- Follows a bridge whose address moves; names a stopped Tailscale as the fix for a lost tailnet link.
- `install-mac.sh` verifies the package against the bridge's `manifest.json` before installing.
