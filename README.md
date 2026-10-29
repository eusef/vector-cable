# VectorCable

Part of the Vector family: [VectorBridge](https://github.com/eusef/vector-bridge) is the Raspberry
Pi radio bridge, VectorCable is its desktop audio cable. GPL-3.0-only, see LICENSE.

A virtual audio cable between desktop digital-mode apps (WSJT-X, JS8Call, FLDIGI) and a
VectorBridge radio bridge. The bridge sits next to the
radio on a Raspberry Pi; VectorCable runs on the laptop, creates the **VectorBridge RX** and
**VectorBridge TX** audio devices, carries audio both ways over Wi-Fi, and relays Hamlib rig
control (CAT and PTT) so the apps work as if the radio were plugged in.

This beta runs on macOS (Apple silicon, macOS 12 or newer) and on Linux desktops with PipeWire,
x86_64 or arm64 (PipeWire is the default on Ubuntu 22.10, Fedora 34, Debian 12, Mint 22 and later;
the program itself needs glibc 2.31 or newer). Windows builds compile but are not shipped.

Each platform installs in one of three ways: one line from GitHub (needs internet), one line from
the bridge (works on the bridge's own Wi-Fi with no internet), or a manual download. Every
installer checks the download's size and sha256 against the release's `manifest.json` before it
installs anything.

## Install on macOS

**Quick install from GitHub**, in Terminal:

```sh
B=https://github.com/eusef/vector-cable/releases/latest/download
curl -fsSL $B/install-mac.sh | VECTORCABLE_BASE=$B sh
```

**From the bridge**, on its Wi-Fi (on home Wi-Fi, use the bridge's address instead of
`10.42.0.1`):

```sh
curl -fsSL http://10.42.0.1/downloads/install-mac.sh | sh
```

Either way the script installs the `vectorcable` command in `/usr/local/bin`, the `VectorCable`
launcher in Applications and the two audio plug-ins. It asks for the admin password once and
restarts Core Audio, which pauses audio in every app for a few seconds. It ends with "Installed.
VectorBridge RX and VectorBridge TX are ready."

**Manual:** download `vectorcable-<version>-macos-arm64.pkg` from the
[latest release](https://github.com/eusef/vector-cable/releases/latest) and open it. The beta
package is not notarized, so macOS blocks the first open: go to System Settings, Privacy and
Security, press **Open Anyway**, and open it again.

**Remove:** the quick-install line with `-s -- --uninstall` after `sh`:

```sh
curl -fsSL $B/install-mac.sh | VECTORCABLE_BASE=$B sh -s -- --uninstall
```

## Install on Linux

First, the three PipeWire helpers VectorCable drives (`pactl`, `pacat`, `parec`). Many desktops
already have them; Ubuntu's PipeWire does not:

```sh
sudo apt install pulseaudio-utils     # Debian, Ubuntu, Mint
sudo dnf install pulseaudio-utils     # Fedora
sudo pacman -S libpulse               # Arch
```

**Quick install from GitHub**, as your own user (no `sudo`):

```sh
B=https://github.com/eusef/vector-cable/releases/latest/download
curl -fsSL $B/install-linux.sh | VECTORCABLE_BASE=$B sh
```

**From the bridge**, on its Wi-Fi:

```sh
curl -fsSL http://10.42.0.1/downloads/install-linux.sh | sh
```

The script picks the build for your machine (x86_64 or arm64) and installs, for your user only,
`vectorcable` in `~/.local/bin` and the **VectorCable** launcher in
`~/.local/share/applications`, so it shows in your applications menu. It needs no root and no
driver: `vectorcable` makes the **VectorBridge RX** source and **VectorBridge TX** sink in
PipeWire when it starts and removes them when it stops, so they exist only while it runs.

**Manual:** download `vectorcable-<version>-linux-x86_64.tar.gz` (or `-linux-arm64`) from the
[latest release](https://github.com/eusef/vector-cable/releases/latest), then:

```sh
tar -xzf vectorcable-*-linux-*.tar.gz
install -D -m 755 vectorcable-*-linux-*/vectorcable ~/.local/bin/vectorcable
install -D -m 644 vectorcable-*-linux-*/vectorcable.desktop ~/.local/share/applications/vectorcable.desktop
```

**Remove:**

```sh
curl -fsSL $B/install-linux.sh | VECTORCABLE_BASE=$B sh -s -- --uninstall
```

Settings in `~/.config/vectorcable` are kept; delete that folder to remove them too.

On Linux the bridge PIN is asked for on every start: VectorCable does not yet keep the app token
between runs there, as it does in the macOS Keychain.

**From source** instead: with Rust installed ([rustup.rs](https://rustup.rs)), run
`cargo install --locked --path crates/vectorcable` in a clone of this repo; it installs to
`~/.cargo/bin` (remove with `cargo uninstall vectorcable`).

## First run

1. On macOS, double-click **VectorCable** in Applications (or run `vectorcable`); Terminal opens
   the app. On Linux, open **VectorCable** from the applications menu (or run `vectorcable` in a
   terminal).
2. On macOS, allow the Microphone prompt for Terminal; TX audio needs it.
3. In WSJT-X, JS8Call or FLDIGI: audio input `VectorBridge RX`, output `VectorBridge TX`,
   rig `Hamlib NET rigctl` at the address the Setup tab shows. `vectorcable setup wsjt-x`
   prints the same steps.
4. `vectorcable doctor` checks the audio devices and the bridge, and on macOS the driver and the
   Microphone permission.

## Build from source

Rust 1.85 or newer. `cargo build --release -p vectorcable`. The macOS driver and package need
Xcode. The Linux tarballs build in Docker (`rust:1.88-bullseye`, so they link glibc 2.31). A
release runs, from one clean commit: `bash linux/build.sh`, `bash macos/driver/build.sh`, then
`bash macos/pkg/build.sh`, which writes every release file and the one `manifest.json` into
`target/macos/dist/`. Checks: `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.

| Path | What |
|---|---|
| `crates/vbcore` | Core: VectorBridge protocol, RX timing, TX gate, session rules, rig control relay |
| `crates/vectorcable` | Terminal app: TUI, `setup`, `doctor`, `version`, platform audio adapters |
| `macos/` | Driver build (a BlackHole fork), `.pkg` build, installer, launcher |
| `linux/` | Tarball build, installer, `.desktop` launcher |
| `DOCS/` | Product brief, build status, setup guides for the bridge |

## Releases and licence

Releases are tagged `vectorcable-v<version>` on this repo; each carries both installers, the
macOS package and bare binary, the two Linux tarballs, `LICENSE`, a source tarball, a `.sha256`
next to every file and a `manifest.json` for the installers and the bridge's Downloads page. Development happens in a separate repo; this
one receives one commit per release.

GPL-3.0-only. See `LICENSE` and `THIRD-PARTY-NOTICES.md`.
