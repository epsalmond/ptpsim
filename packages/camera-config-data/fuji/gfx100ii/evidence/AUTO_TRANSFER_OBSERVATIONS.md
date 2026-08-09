# GFX100 II automatic transfer observations

**Firmware:** 2.30  
**Evidence:** Wire captures of camera behavior  
**Scope:** One completed transfer, one queued-state capture, and the BLE handoff state

## Question

What observable state is sufficient to receive a camera-initiated object without exposing client implementation details?

## Observed behavior

The resting BLE state used `apState = 0x8000` and `transferState = 0x8000`. A pending transfer changed those values to `0x8003` and `0x8001`. The host then established the manifest's `app` PTP/IP connection.

The completed capture reported the queue count through member `0xDF41` of property `0xD212`. Metadata came from `GetObjectInfo (0x1008)`. Object bytes came from `GetPartialObject (0x101B)`, using property `0xD235` as the chunk limit. The completed sample drained one object to EOF.

A separate queued-state capture reported more than one pending object. It did not prove advancement after the first object.

## Uncertainty

The BLE launch write was not retained as redistributable evidence. The manifest therefore treats it as optional. Multi-object queue advancement is not established.

## Implementation consequence

The simulator may model the observed trigger values and one-object receive flow. It must not claim a general multi-object queue policy or require the unverified BLE launch write.
