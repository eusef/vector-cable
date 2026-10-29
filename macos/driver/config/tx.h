// VectorBridge TX cable: build-time constants for the BlackHole fork.
// Force-included ahead of BlackHole.c (OTHER_CFLAGS=-include), which wraps
// every constant in #ifndef. GPL-3.0, like BlackHole.
//
// Device 1 is what the desktop app sees: an output it plays TX audio to.
// Device 2 is hidden and input-only: the core reads TX audio from it and opens
// it by UID. Both share one ring buffer.
//
// Never a default device: system sounds routed here would reach the radio, and
// on a VOX rig any TX audio keys the transmitter. No volume control, so no
// hidden OS gain stage sits in the TX path.

#define kDriver_Name                "VectorBridgeTX"
#define kPlugIn_BundleID            "io.github.eusef.vectorcable.tx"
#define kHas_Driver_Name_Format     false
#define kManufacturer_Name          "VectorCable"

#define kDevice_Name                "VectorBridge TX"
#define kDevice_IsHidden            false
#define kDevice_HasInput            false
#define kDevice_HasOutput           true

#define kDevice2_Name               "VectorBridge TX (core)"
#define kDevice2_IsHidden           true
#define kDevice2_HasInput           true
#define kDevice2_HasOutput          false

#define kNumber_Of_Channels         2
#define kSampleRates                48000
#define kEnableVolumeControl        false
#define kCanBeDefaultDevice         false
#define kCanBeDefaultSystemDevice   false
