# JS8Call with VectorCable

Install VectorCable first ([on a Mac](vectorcable-setup-macos) or [on Linux](vectorcable-setup-linux))
and leave it running in its terminal window. JS8Call then needs five settings. The Setup tab in VectorCable shows the same
list with the live rig-control address filled in.

| Where in JS8Call | Set it to |
|---|---|
| Settings > Audio > Input | VectorBridge RX |
| Settings > Audio > Output | VectorBridge TX |
| Settings > Radio > Rig | Hamlib NET rigctl |
| Settings > Radio > Network Server | `127.0.0.1:4532` (VectorCable's relay; the Setup tab shows the address to use if the relay could not start) |
| Settings > Radio > PTT method | CAT, so radio control keys the radio. Choose VOX only when the bridge's Radio sheet says the rig is on VOX |

JS8Call's Settings window mirrors WSJT-X's. Press Test CAT and Test PTT before the first call. JS8Call keys for longer than FT8, so set the bridge's transmit time limit to cover your slowest speed.

## Levels

- Receive: in VectorCable's Levels tab, set RX gain so JS8Call's input meter sits in its green band
  with the radio on a busy band. Too low and weak signals are lost; too high and strong ones clip.
- Transmit: start low. Raise TX gain until the bridge's Radio sheet shows ALC just moving on a
  test transmission, then back off a little. The bridge caps each transmission at its transmit
  time limit.

## Does it work?

1. JS8Call decodes stations within a minute on an open band.
2. Press Tune (or Test PTT). The bridge's home page shows the radio keyed, and VectorCable's
   Status tab shows TX.
3. Unkey. If the radio stays keyed, see [Troubleshooting](vectorcable-troubleshooting).
