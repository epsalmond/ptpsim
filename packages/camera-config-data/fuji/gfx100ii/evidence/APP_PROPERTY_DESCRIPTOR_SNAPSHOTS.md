# App property descriptor snapshots

Three May 18, 2026 captures from the GFX100 II running firmware 2.30
contain 28 complete `0x902B` exchanges on the live-view app command
connection with the Android reference app, version 2.7.3.

The compact request has zero parameters. The camera sends one type-2 data
container for `0x902B` followed by `0x2001`, both with the request's
transaction ID. The data body starts with a little-endian `u32` count. Each
standard `DevicePropDesc` has a little-endian `u32` byte length prefix that
includes the prefix itself. There are no trailing bytes.

| Selector `0xD246` | Exchanges | Body bytes | Records in wire order |
| --- | ---: | ---: | --- |
| 0 (stills) | 26 | 443 | `5012 500C 5005 D001 D02A D17C D245 5007 D240 D019 D246` |
| 1 (video) | 2 | 331 | `5005 D001 D02B D245 5007 D240 D019 D246` |

Five stills requests succeeded after entering the live-view function mode
and before sending `0x101C`. Repeated requests returned complete snapshots.
The video snapshots follow an acknowledged `0xD246=1` write. The stills set
returns after an acknowledged `0xD246=0` write.

The `0x5007` current value is observed at 280 and 360, both outside its enum.
The `0x500C` default is 2, outside its enum. The `0xD246` enum has two zero
entries, even in the video snapshot. `0xD17C` carries a range and has captured current values `0x04030102`,
`0x04030504`, and `0x04030606`. `0xD240` has
current values `0x8000EA60` in stills and `0x80007530` in video, with the enum
`[0, 0xFFFFFFFF]` in both.

The reduced transactions retain the exact camera data bodies and cover only
`0x902B`. The [first capture](app-descriptor-snapshots/2026-05-18-app-descriptor-snapshots-1.jsonl)
contains 20 exchanges. The
[second](app-descriptor-snapshots/2026-05-18-app-descriptor-snapshots-2.jsonl) and
[third](app-descriptor-snapshots/2026-05-18-app-descriptor-snapshots-3.jsonl) contain four each.

The [app descriptor proposal](app-descriptor-snapshots.proposal.json) groups
byte-identical exchanges from these three bundles. The
[digest-bound review](app-descriptor-snapshots.review.json)
selects a stills and a video capture from the same command session, records
`snapshot-13` and `snapshot-17` in the first capture. The simulator serves
those two snapshots according to the current `0xD246` value. Other members
retain their captured values. Other firmware versions, camera states, and
transports remain outside the observation.
