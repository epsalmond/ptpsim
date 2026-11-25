# GFX100 II app video selector operations

One May 18, 2026 session between a GFX100 II on firmware 2.30 and the Android
reference app, version 2.7.3, recorded the app's in-live-view switch from
stills to video and back on the Wi-Fi app connection. The
[reduced bundle](app-video-selector/2026-05-18-app-video-selector-ops.jsonl)
keeps the two selector writes, the two vendor transactions, and the two
events between them. All records come from one command connection and its
event connection in the same session.

| Record | Wire behavior | Time |
| --- | --- | ---: |
| `0x1016(0xD246)` | One-byte data phase `01`, then `0x2001`. | 0 ms |
| `0x9020` | Transaction 58, parameters `(0, 0)`. Bare `0x2001`. | 2,346 ms |
| `0xC004` event | Parameters `(58, 58, 0, 0)`. | 2,672 ms |
| `0x9021` | Transaction 64, parameter `58`. Bare `0x2001`. | 3,670 ms |
| `0x400D` event | Parameters `(67, 67, 0, 0)`. | 4,359 ms |
| `0x1016(0xD246)` | One-byte data phase `00`, then `0x2001`. | 7,455 ms |

Times are host timestamps from the `0xD246=1` command send. Transactions use
the command send time. Events use the time the event container was fully
received. Host timestamps include transport and recording latency. Both events
used type-4 containers with wire transaction ID 1.

Earlier in the session the client had written function mode `0x16` and sent
`0x101C(0, 0)`. It wrote no function mode and sent no `0x1018` in this
interval. The client sent neither `0x100E` nor
`0x9022` in this interval, and no `0xC001` event appeared. The `0x902B`
descriptor snapshots taken in the same session after each selector write are
`snapshot-17` through `snapshot-20` of the
[first descriptor capture](app-descriptor-snapshots/2026-05-18-app-descriptor-snapshots-1.jsonl)
([description](APP_PROPERTY_DESCRIPTOR_SNAPSHOTS.md)).

What the session establishes: with `0xD246=1`, the camera acknowledged one
`0x9020(0, 0)` and one `0x9021` whose parameter equaled the `0x9020`
transaction ID.

What it does not establish:

- whether any other operation is refused with `0xD246=1`;
- whether the `0x9021` parameter must equal the `0x9020` transaction ID, and
  what a mismatch returns;
- whether the pair causes the `0xC004` and `0x400D` events, and whether their
  timing is stable;
- whether the pair works with `0xD246=0`;
- whether `0xD246` survives `CloseSession`, a reconnect, or an image-transfer
  round trip;
- the operation names. The session contains no observed recording.

The simulator detects `shooting/video` on the app connection from function
mode `0x16` with `0xD246=1` and acknowledges both operations without data. It
does not emit the two events and does not check the `0x9021` parameter.
