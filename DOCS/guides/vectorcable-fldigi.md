# FLDIGI with VectorCable

Install VectorCable first ([on a Mac](vectorcable-setup-macos) or [on Linux](vectorcable-setup-linux))
and leave it running in its terminal window. FLDIGI then needs five settings. The Setup tab in VectorCable shows the same
list with the live rig-control address filled in.

| Where in FLDIGI | Set it to |
|---|---|
| Configure > Soundcard > Devices > Capture | VectorBridge RX |
| Configure > Soundcard > Devices > Playback | VectorBridge TX |
| Configure > Rig Control > Hamlib > Device | Hamlib NET rigctl |
| Configure > Rig Control > Hamlib > Server | `127.0.0.1:4532` (VectorCable's relay; the Setup tab shows the address to use if the relay could not start) |
| Configure > Rig Control > PTT method | CAT, so radio control keys the radio. Choose VOX only when the bridge's Radio sheet says the rig is on VOX |

In FLDIGI the audio devices are under Configure, Soundcard, Devices, Capture and Playback (pick PortAudio). Rig control is under Configure, Rig Control, Hamlib: Device Hamlib NET rigctl, Server 127.0.0.1:4532, then Initialize. PTT via Hamlib command. FLMSG and FLAMP use FLDIGI's connection and need nothing more.

## Levels

- Receive: in VectorCable's Levels tab, set RX gain so FLDIGI's input meter sits in its green band
  with the radio on a busy band. Too low and weak signals are lost; too high and strong ones clip.
- Transmit: start low. Raise TX gain until the bridge's Radio sheet shows ALC just moving on a
  test transmission, then back off a little. The bridge caps each transmission at its transmit
  time limit.

## Does it work?

1. FLDIGI decodes stations within a minute on an open band.
2. Press Tune (or Test PTT). The bridge's home page shows the radio keyed, and VectorCable's
   Status tab shows TX.
3. Unkey. If the radio stays keyed, see [Troubleshooting](vectorcable-troubleshooting).
