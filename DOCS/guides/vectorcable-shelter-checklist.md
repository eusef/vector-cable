# First 10 minutes at a shelter

For an operator arriving with a laptop at a site that already has a VectorBridge and a radio.

1. **Power and radio.** The bridge is on and its status light is steady or blinking short. The
   radio is on, on the agreed band, and connected to the bridge's audio interface.
2. **Join the Wi-Fi.** On the Mac, join the bridge's Wi-Fi network. Its name and passphrase are on
   the bridge's label or the site sheet.
3. **Open the bridge page.** In a browser, go to `http://10.42.0.1/`. The home page shows the
   radio and the Downloads link in the footer.
4. **Install VectorCable if this Mac has none.** Terminal:
   `curl -fsSL http://10.42.0.1/downloads/install-mac.sh | sh`. No internet needed.
5. **Start VectorCable** from Applications. Allow the Microphone prompt. Wait for the Status tab
   to show the bridge and a steady buffer.
6. **Start the digital-mode app** (WSJT-X, JS8Call or FLDIGI). Check its audio and rig settings
   against VectorCable's Setup tab. Press Test CAT.
7. **Listen first.** Decodes within a minute mean audio and the clock are fine. No decodes: run
   `vectorcable doctor`.
8. **Clock.** Off-grid, compare the Mac's clock with VectorCable's clock-error readout and a
   known time source; FT8 needs it within a second.
9. **Switch to Transmit** in VectorCable only when the site lead says the radio is clear.
   Tune once at low power; watch the bridge's Radio sheet for ALC.
10. **Log the hand-over.** Note the band, the radio's power setting and who is on the key.

Second laptop? The Downloads page can be copied to a USB stick: it holds the installer, the
package, the licence and the source.
