// VectorBridge RX cable: build-time constants for the BlackHole fork.
// Force-included ahead of BlackHole.c (OTHER_CFLAGS=-include), which wraps
// every constant in #ifndef. GPL-3.0, like BlackHole.
//
// Device 1 is what the desktop app sees: an input it records the radio from.
// Device 2 is hidden and output-only: the core writes assembled RX audio to it
// and opens it by UID. Both share one ring buffer.

#define kDriver_Name                "VectorBridgeRX"
#define kPlugIn_BundleID            "io.github.eusef.vectorcable.rx"
#define kHas_Driver_Name_Format     false
#define kManufacturer_Name          "VectorCable"

#define kDevice_Name                "VectorBridge RX"
#define kDevice_IsHidden            false
#define kDevice_HasInput            true
#define kDevice_HasOutput           false

#define kDevice2_Name               "VectorBridge RX (core)"
#define kDevice2_IsHidden           true
#define kDevice2_HasInput           false
#define kDevice2_HasOutput          true

#define kNumber_Of_Channels         2
#define kSampleRates                48000
#define kEnableVolumeControl        false
#define kCanBeDefaultDevice         false
#define kCanBeDefaultSystemDevice   false
