# VectorCable troubleshooting

Start with `vectorcable doctor` in a terminal: each row prints a fix. Then this table. The Linux
rows are at the end.

| Symptom | Likely cause | Fix |
|---|---|---|
| No VectorBridge RX or TX in the app's audio list | The plug-ins are not loaded | Restart the Mac, then run the installer again. `doctor` shows the Driver row |
| The app decodes nothing | Audio input is not VectorBridge RX, or RX gain is zero, or the bridge has no radio audio | Check the app's Input, the Levels tab, and the bridge's Radio sheet |
| TX is silent, the radio keys but sends nothing | Terminal has no Microphone permission | System Settings, Privacy and Security, Microphone: allow Terminal, restart Terminal |
| "Link lost" on the Status tab | Wi-Fi dropped, the bridge rebooted, or its address moved | Rejoin the bridge's Wi-Fi. VectorCable looks the bridge up again every 2 s |
| "Fix: Start Tailscale on this computer" | The bridge is reached over a tailnet and Tailscale stopped on the Mac | Start Tailscale, or join the bridge's Wi-Fi |
| Test CAT fails in the app | Nothing listens on 127.0.0.1:4532, the bridge refused VectorCable's log-in, or the bridge refuses rig control from this network | Keep VectorCable running: it is the relay and logs in for the app. If Status asks for the PIN ("press p"), type it, then Test CAT again. On the bridge, Rig access `wifi` answers on its Wi-Fi; `all` is needed over a tailnet |
| The app says port 4532 is in use | Another rigctld runs on the Mac | Quit it, or start VectorCable with `--rig-port 4533` (or put `rig_port = 4533` in `~/.config/vectorcable/config`) and point the app at `127.0.0.1:4533`. The bridge takes rig control only through VectorCable, so pointing the app straight at it does not work |
| Status asks for a PIN ("press p") | The bridge has an admin PIN and has not issued this computer a token | Press p and type the bridge's PIN once; VectorCable keeps a token, never the PIN |
| Choppy audio, Underruns climbing | A busy Wi-Fi channel or weak signal | Move closer to the bridge, or move the bridge off a crowded 5 GHz channel; the Check tab shows packet loss |
| The radio stays keyed | The transmit time cap or the app's PTT did not release | The bridge unkeys at its transmit time limit; set the app's PTT to CAT, not VOX, when the rig has CAT |
| Status says "an app keyed the radio while VectorCable is Listen Only" | WSJT-X (or another app) transmitted over rig control while VectorCable was Listen Only, so the radio keyed with no audio | Press t for TX mode before transmitting, or stop the app transmitting (Halt Tx in WSJT-X) |
| Decodes are late or missing with FT8 | The Mac's clock is off | Status shows the clock error; set the clock by hand off-grid |
| Pressing m says "... is a VectorBridge device" or "... includes a VectorBridge device" | The system sound output is VectorBridge TX or RX, or a combined output that includes one; monitoring there would send radio audio back to the radio | Set the sound output to speakers or headphones, then press m again |
| Pressing m plays nothing | The sound output changed after m was pressed, or the volume is down | Press m twice to pick up the current output; check the system volume |
| Installer says "checksum mismatch" | The download was cut short or the bridge serves a different release | Run the installer again; if it repeats, tell whoever manages the bridge |
| Linux: "pactl not found: install pulseaudio-utils" | The PipeWire helpers `pactl`, `pacat` and `parec` are missing | `sudo apt install pulseaudio-utils` (Fedora: `dnf`, the same name; Arch: `pacman -S libpulse`), then start VectorCable again |
| Linux: pressing m says "pw-dump not found" | The PipeWire tools that let VectorCable check the sound output are missing | `sudo apt install pipewire-bin` on Debian or Ubuntu, then press m again |
| Linux: no VectorBridge RX or TX in the app's audio list | The devices exist only while VectorCable runs | Start VectorCable first, then open (or reopen) the app |
| Linux: `vectorcable: command not found` after installing | `~/.local/bin` is not on this shell's PATH yet | Log out and in again, or run `~/.local/bin/vectorcable`; the menu launcher that `install-linux.sh` writes names the full path, so it works either way |
| Linux: Status asks for the PIN at every start | VectorCable could not write `~/.local/state/vectorcable/tokens` (Status said "The token file did not keep the bridge token"), or the home folder is not set | Fix the folder's ownership or permissions, then press p and type the PIN once more |

If none of this helps, keep the output of `vectorcable doctor` and the Check tab when asking for help.
