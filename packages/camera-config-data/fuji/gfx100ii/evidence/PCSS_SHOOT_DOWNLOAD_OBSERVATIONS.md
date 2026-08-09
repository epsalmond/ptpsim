# GFX100 II PCSS shoot and download observations

**Firmware:** 2.30  
**Evidence:** PTP/IP wire capture on the PCSS connection  
**Duration:** 14.9 minutes

## Question

Which capture, live-view, and object-transfer operations were observed on the PCSS connection?

## Observed behavior

The capture used command port 15740 and no separate event channel. Live-view frames were polled with operation `0x9018`.

The shutter sequence wrote property `0xD039` around `InitiateCapture (0x100E)`. Object transfer used `GetObjectHandles (0x1007)`, `GetObjectInfo (0x1008)`, optional `GetThumb (0x100A)`, `GetObject (0x1009)`, and optional `DeleteObject (0x100B)`.

The capture also observed signed sentinel values for property `0x500F`. These values are represented by the checked manifest rather than inferred from a client implementation.

## Uncertainty

The capture does not establish a separate event socket, resumable reads, or support for every media format. Descriptor-only controls remain lower confidence where no successful write was observed.

## Implementation consequence

The PCSS connection uses polling, whole-object reads, and restart-from-zero transfer policy. It does not inherit the app connection's chunked transfer or auxiliary channels.
