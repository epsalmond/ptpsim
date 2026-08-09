# GFX100 II app PTP/IP observations

**Firmware:** 2.30  
**Evidence:** PTP/IP wire captures and direct camera observations

## Question

Which mode transitions and channel gates are observable on the camera-hosted Wi-Fi connection?

## Observed behavior

The command connection used port 55740. Live-view mode selected function mode `0x16` through property `0xDF01`. `InitiateOpenCapture (0x101C)` completed before the event and live-view listeners accepted connections on ports 55741 and 55742.

Image-import mode selected function mode `0x14`. The observed setup then read or wrote properties `0xDF28`, `0xD226`, `0xD227`, and `0xD244` before the image-import operation sequence.

A captured still cycle sent `InitiateCapture (0x100E)`, observed the postview-ready state, and then sent operation `0x9022` to retrieve the postview result.

## Uncertainty

No retained public evidence supports the former `0xDF00`, `0xDF2A`, or repeated `0x902B` startup steps. Those steps are omitted. Listener behavior after every possible disconnect is not established.

## Implementation consequence

The manifest opens auxiliary channels only after successful `0x101C`. It models only the retained mode writes and capture sequence. Unsupported startup steps are not replayed.
