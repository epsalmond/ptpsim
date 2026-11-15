# TETHER APP static candidates

These reduced facts describe FUJIFILM TETHER APP 1.34.1.0 arm64 accessor and
operation call sites. They contain function identities, full PTP codes and
client type hints. The source binary hashes
are in each document. Static candidates remain unverified on a camera.

| Plugin | Model scope | Property codes | Vendor property codes | Missing vendor properties at base | Raw vendor names at base |
|---|---|---:|---:|---:|---:|
| FF0018 | GFX100 II | 246 | 236 | 64 | 163 |
| FF0021 | Unknown | 288 | 282 | 101 | 172 |

The comparison base is `a2cea30`. It has 330 GFX100 II properties. FF0018 also
has three absent standard-range property codes: `0x5014`, `0x5018`, `0x5019`.
Its catalog proposal therefore adds 67 property identities and replaces 163
generated raw property names. Two advertised-only firmware-upload operations
also receive candidate names without changing their classification. Existing reviewed names and camera types remain intact.
FF0021 rows do not establish GFX100 II support.

All rows use the [static candidate contract](../../../../docs/STATIC_CATALOG.md).
The JSON documents are the initiator's machine-readable validation surface.
Property rows retain getter, setter and capability-accessor hints without
declaring camera writability. Operation rows retain known request shapes or
state the missing dynamic arguments. Compound coverage rows account for
capture, focus/zoom writes, video properties, D212 polling, internal wait
signals, live view, transfer, backup/restore and firmware upload. No firmware
upload or formatting probe is authorized by these facts.

Corrections retained in the catalog:

- `0xd208` remains `pcssCaptureFunction` with its existing AF actions.
- `0xd20c` remains `simultaneousCardRecording`. `SDK_SetMediaRecord` does not
  establish a movie start/stop command.
- `0xd145` binds PasswordInput; `0xd222/0xd223` bind SensitivityFineTune;
  `0xd232/0xd233` bind AFCCustom; `0xd273/0xd274` bind MovieMonochromaticColor.
- Capture request `0x100e` carries two parameters, `[0, 0]`.
- `0xc006` and `0x4002` poller wakeups remain internal SDK signals. They do not
  add camera-emitted events.
- Six zero-datatype entries per plugin came from a truncated extraction
  expression in white-balance call sites. They are excluded from type hints.

`tether-app-ff0018.proposal.json` and `tether-app-ff0018.review.json` bind the
catalog additions to their reviewed facts. The review accepts static identities
and their stated limitations. It does not promote them to wire observations.
The generated consolidated manifest contains `unverified:` comments on every
added or statically named property and operation.

Regenerate a proposal with:

```sh
cargo run -p camera-config --bin camera-config-generate -- propose \
  --static-candidates packages/camera-config-data/fuji/catalog/tether-app-ff0018.candidates.json \
  --output packages/camera-config-data/fuji/catalog/tether-app-ff0018.proposal.json
```

Apply the digest-bound review to the consolidated manifest from the comparison
base. Apply rejects an already-added property or a curated-name conflict.
This prevents silently treating a stale static proposal as a new observation.

Issue #549 owns camera validation of property identities, datatype conflicts
and static write/action candidates. Selector-specific operation support is
#540; live-view response contracts are #541; D212 record maintenance is #329.
Hardware verification belongs to a later initiator run.
