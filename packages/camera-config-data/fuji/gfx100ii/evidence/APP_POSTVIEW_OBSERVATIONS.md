# GFX100 II app stills postview

The Wi-Fi app command and event channels of a GFX100 II on firmware 2.30
recorded [three live-view stills takes](app-postview/2026-05-18-app-take-cycles.jsonl)
on May 18, 2026. Each take used `0x100E(0, 0)`, followed by `0xC004` and
`0xC001` events on the auxiliary event channel. The client then sent
zero-parameter `0x9022`. The camera sent one compact type-2 data container
echoing the command's opcode and transaction ID, then `0x2001`. Each data
body was a complete 640×480 baseline JPEG. The `0x400D` event followed the
reply.

| Take | `0xC004` after shutter send | `0xC001` after shutter send | `0x9022` after `0xC001` | JPEG bytes | `0x400D` after `0x9022` reply |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 195 ms | 588 ms | 13 ms | 9,895 | 359 ms |
| 2 | 260 ms | 729 ms | 17 ms | 8,492 | 260 ms |
| 3 | 63 ms | 382 ms | 76 ms | 9,183 | 378 ms |

All three event types used type-4 containers with wire transaction ID 1
and four little-endian u32 parameters. The first two counters were equal.
The third parameter in `0xC001` carried the exact JPEG length. All other
trailing parameters were zero. The relation between the counters and
command transaction IDs varied across takes.

Host timestamps include transport and recording latency. One `0xC004`
arrived before the shutter acknowledgment. The simulator uses the first
take's representative 145 ms and 538 ms delays after the shutter reply, and
359 ms after the postview read reply. It serves a synthetic grayscale
fixture and reports that fixture's byte length. The synthetic counters use
the causative request transaction ID plus one. The source of the camera's
postview pixels remains unknown.
