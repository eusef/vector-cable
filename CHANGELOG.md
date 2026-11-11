# Changelog

## 0.3.0 (2026-11-11, beta)

- Install, upgrade and remove: the README and the macOS and Linux guides give each GitHub
  command as one line to copy, and say how to upgrade (run the install line again; settings and
  the bridge log-in are kept).

- Radio monitor: press `m` to hear the radio on the computer's sound output (speakers or
  headphones) while the apps decode, and `m` again to stop. Off at each start. It plays on the
  output chosen when `m` is pressed and refuses a VectorBridge device. macOS and Linux.
- Rig control: when an app keys the radio while VectorCable is Listen Only, VectorCable says so
  (the radio transmits but no audio is sent). The key still goes to the radio, as before.
- Linux: the bridge PIN is typed once per bridge. VectorCable keeps the bridge's app token (never
  the PIN) in `~/.local/state/vectorcable/tokens`, readable only by you, as the macOS version does
  in the Keychain. If that file cannot be written, the PIN is asked for on each start, as before.
- Bridge PIN: the Status tab shows whether the bridge PIN is entered, and until it is, the Check
  tab says where it is set and reset. When something needs the PIN (a level change, or an app's
  rig control, CAT or PTT), VectorCable asks for it with `p` and says where it is set and reset.
  On a CAT radio whose bridge requires a login for rig control, Transmit shows Not Ready until the
  PIN is entered, since apps cannot key the radio before.

## 0.2.1 (2026-11-04, beta)

- Rig control relay: a single failed check of the bridge's rig control settings (a short Wi-Fi
  dropout, for example) no longer makes the relay skip its log-in, so WSJT-X, JS8Call and FLDIGI
  keep rig control instead of being refused until the next good check.

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
