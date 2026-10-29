# WSJT-X with VectorCable

Install VectorCable first ([on a Mac](vectorcable-setup-macos) or [on Linux](vectorcable-setup-linux))
and leave it running in its terminal window. WSJT-X then needs five settings. The Setup tab in VectorCable shows the same
list with the live rig-control address filled in.

| Where in WSJT-X | Set it to |
|---|---|
| Settings > Audio > Input | VectorBridge RX |
| Settings > Audio > Output | VectorBridge TX |
| Settings > Radio > Rig | Hamlib NET rigctl |
| Settings > Radio > Network Server | `127.0.0.1:4532` (VectorCable's relay; the Setup tab shows the address to use if the relay could not start) |
| Settings > Radio > PTT method | CAT, so radio control keys the radio. Choose VOX only when the bridge's Radio sheet says the rig is on VOX |

Under Settings, Radio, leave Poll Interval at 1 s and PTT port empty. Mode: Data/Pkt. Split operation: None (the bridge's proxy handles the rig). Press Test CAT: it turns green when the relay reaches the bridge.

Clock: FT8 needs the Mac's clock within about a second of UTC. Off-grid, VectorCable's Status tab shows the clock error it measures from decodes; set the clock by hand if it drifts.

## Levels

- Receive: in VectorCable's Levels tab, set RX gain so WSJT-X's input meter sits in its green band
  with the radio on a busy band. Too low and weak signals are lost; too high and strong ones clip.
- Transmit: start low. Raise TX gain until the bridge's Radio sheet shows ALC just moving on a
  test transmission, then back off a little. The bridge caps each transmission at its transmit
  time limit.

## Does it work?

1. WSJT-X decodes stations within a minute on an open band.
2. Press Tune (or Test PTT). The bridge's home page shows the radio keyed, and VectorCable's
   Status tab shows TX.
3. Unkey. If the radio stays keyed, see [Troubleshooting](vectorcable-troubleshooting).
