# GFX100 II app PTP/IP observations

**Firmware:** 2.30  
**Evidence:** PTP/IP wire captures and direct camera observations

## Question

Which mode transitions and channel gates are observable on the camera-hosted Wi-Fi connection?

## Observed behavior

The command connection used port 55740. Live-view mode selected function mode
`0x16` through property `0xDF01`. `InitiateOpenCapture (0x101C)` completed
before the event and live-view listeners accepted connections on ports 55741
and 55742.

Image-import mode selected function mode `0x14`. The observed setup then read or wrote properties `0xDF28`, `0xD226`, `0xD227`, and `0xD244` before the image-import operation sequence.

A captured still cycle sent `InitiateCapture (0x100E)`, observed the postview-ready state, and then sent operation `0x9022` to retrieve the postview result.

## Reviewed manifest declaration

The reviewed live-view declaration restores an advisory write of 6 to
`0xDF00`, followed by the `0xDF01` mode write, a `0xDF2A` read and echo, and
four `0x902B` operations. The transfer-to-live-view declaration writes 2 to
`0xDF2A` instead of echoing its current value. Both sequences send `0x101C`
before opening the event and live-view channels.

## Uncertainty

Retained public captures do not independently reproduce the restored `0xDF00`,
`0xDF2A`, and `0x902B` steps. Hardware observations may refine this declaration.
Listener behavior after every possible disconnect is not established.

## Implementation consequence

The manifest replays the reviewed live-view preamble and opens auxiliary
channels only after successful `0x101C`.
