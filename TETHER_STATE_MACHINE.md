# GFX100 II tether — modes, prerequisites, properties, valid values (state-machine reference)

WIRE-CONFIRMED 2026-05-23 over wired-gigabit + Wi-Fi infra PTP-IP tether (camera 192.168.4.169).
Tools: `scripts/connect_wireless_tether.py`, `scripts/movie_probe.py` (`--all` dumps full surface),


## 0. Network modes (how the camera is reachable)
| Mode | How to enter | Exposes | Notes |
|---|---|---|---|
| **AP / reference app** | BLE FUNCTION_LAUNCH → camera open AP | PTP-IP **55740/41/42** | flaky BLE/AP, 60s window; ISO via 0xd02a, Big-3 = relative step-ops 0x902C/D |
| **Infra tether (PCSS)** | camera joins LAN (Wi-Fi or **Ethernet**) → PCSS knock | PTP-IP **15740** | our main path; wired = gigabit, smooth; Big-3 = absolute std props |
| **XLV** | camera UI → XLV mode (infra Wi-Fi) | **HTTP / Python web server** | UNEXPLORED — next mode to map (see §5) |
| **Plain wireless tether** | camera joins LAN, idle | nothing (no listener) | dead end |

## 1. Connection state machine (entry to all PTP-IP control — infra PCSS path)
```

S1 KNOCK         host -> UDP cam:51562  "DISCOVERY * HTTP/1.1\r\nHOST:<hostIP>\r\nMX:5\r\nSERVICE:PCSS/1.0\r\n\0"
                   repeat every 10s (fresh ephemeral src port). cam not ready -> ICMP port-unreachable.
S2 NOTIFY        cam ARPs host, TCP-> host:51560, sends "NOTIFY * HTTP/1.1 ... CAMERANAME ... DSCPORT:15740"
S3 ACK           host -> "HTTP/1.1 200 OK\r\n\0" on that socket (403 = reject); cam closes :51560
S4 PTPIP         host -> TCP cam:DSCPORT(15740); Init_Command_Request (GUID + hostIP + name + zeros-tail)
                   cam may Init_Fail(0x2019 Device_Busy) or RST -> retry/reconnect until Init_Command_Ack
S5 SESSION       OpenSession(0x1002) -> 0x2001; now GetDeviceInfo/props/objects work
S6 CLOSE         CloseSession(0x1003) — REQUIRED for clean release (else camera holds / once-per-boot)
```
Prereqs: camera powered & on LAN; **power-cycle if the knock gets no callback** (spent state). Switching
the physical Still/Movie selector mid-session wedges PTP-IP → power-cycle.

## 2. Capture / live-view sub-states (after S5)
| Action | Op | Prereq | Result |
|---|---|---|---|
| Live view start | InitiateOpenCapture **0x101C** (params 0,0) | session open; works in stills AND movie | creates live-view object 0x80000001 |
| Pull frame | GetObjectInfo+GetObject+DeleteObject on **0x80000001** | live view started | JPEG; stills ~113KB, movie ~16KB @ ~16fps |
| Live view stop | TerminateOpenCapture **0x1018** | live view active | — |
| Still capture | InitiateCapture **0x100E** (0,0) | **STILLS mode** (movie acks but no photo) | new object on card → GetObject to download |
| Movie record | InitiateMovieCapture **0x9020** | — | **NOT POSSIBLE** — not in DeviceInfo SupportedOps |
| New-image download | GetObjectHandles(0xFFFFFFFF,0)→GetObjectInfo→GetObject→DeleteObject | a capture happened | Delete frees the camera's PC-transfer queue |

Camera advertises 24 ops; vendor 0x9xxx = **0x900c, 0x900d, 0x901d, 0x9018**(GetLiveViewData). No 0x9020.

## 3. Big-3 + key settings — STILLS mode (absolute, SetDevicePropValue 0x1016)
| Setting | Prop | Type | Valid values |
|---|---|---|---|
| ISO | **0x500f** | UINT32 | literal (200,320,2000,3200…) or 0xFFFFFFFF/0xFFFFFFFE = Auto |
| Shutter | **0x500d** | UINT32 | **microseconds** (20000=1/50s, 100000=1/10s, 200000=1/5s) |
| Aperture | **0x5007** | UINT16 | FNumber ×100 (280 = f/2.8) |
| Exposure mode | 0x500e | UINT16 | PASM (enum locked to current dial position) |
| Exp bias | 0x5010 | — | EV comp |
| Image size | 0x5003 | STR | "4000x3000" |
| White balance | 0x5005 | — | enum |
| DateTime | 0x5011 | STR | "20260523T091637" |
Full stills surface = ~80 props (set/desc/get) — see `WIRED_TETHER_CONTROL.md` + `wired_ptp_cmds_*.txt`.

## 4. MOVIE mode — available properties (24; the other ~63 stills props return 0x2002 = N/A)
Set Still/Movie selector to **Movie**, power-cycle, reconnect. Big-3 (0x5007/500d/500f) are **N/A in movie**.
| Prop | Type | W | Valid values (enum/range) | likely |
|---|---|---|---|---|
| 0x500a | UINT16 | y | [1] | FocusMode (locked) |
| 0x500e | UINT16 | y | [1] | exposure mode (locked) |
| 0xd01b | UINT16 | y | [1,2,3] | |
| 0xd037 | UINT16 | y | [0x101] | |
| 0xd039 | UINT32 | y | [0x10000,1,0x20000,2,0x80000…] | |
| 0xd136 | UINT16 | y | [1,2,3] | |
| 0xd170 | UINT16 | y | range 1..1 | |
| 0xd171 | INT16 | y | enum (complex) | |
| **0xd174** | UINT16 | y | **[1,2,3]** | LiveView ImageSize (restream lever) |
| 0xd1bc | UINT16 | y | [1] | LiveView Mode |
| 0xd201 | UINT16 | y | [0xffff] | |
| 0xd208 | UINT16 | y | [0x200,4,0x304,0x8000,0xa0…] | |
| 0xd209 | UINT16 | n | — | (read-only status) |
| 0xd211 | UINT16 | n | range 0..0 | |
| 0xd228 | UINT16 | y | [1,2,3,4] | |
| 0xd230 | UINT16 | y | [1,2] | |
| 0xd23c | UINT16 | y | [1] | LiveView aspect |
| 0xd23f | UINT32 | n | — | |
| **0xd247** | UINT16 | y | **[1,2,3,4,5,6]** | movie format/rate (6 options) |
| 0xd24c | UINT16 | y | (empty enum) | movie setting |
| 0xd253 | UINT16 | y | (empty enum) | movie setting |
| 0xd304 | UINT16 | y | [1,3,4,5,6,…] | film simulation? |
| 0xd36a | UINT32 | n | — | |
| 0xd38c | UINT16 | y | range 1..1 | |
Note: `0xd1bc`/`0xd23c` gain a 2nd enum value in some movie states (seen [1,2]); enums are dynamic per
camera state, so read DevicePropDesc live before relying on them. (Stills-mode `--all` dump TODO to
complete the both-modes matrix.)

## 5. XLV — the unexplored mode (TODO)
Separate infra-Wi-Fi mode that runs a **Python web server** on the camera (per earlier notes, "works but
bad"). Not PTP-IP. Next pass: enter XLV on the camera, find its IP, map the HTTP API (live-view endpoint,
controls, JWT/auth). Prior XLV RE exists in the cohort (Flask routes / property_code_def.py).

## State-machine summary
NETWORK(AP | infra-PCSS | XLV | idle) → [PCSS rendezvous] → SESSION → {LIVEVIEW(0x101C/0x80000001),
STILL-CAPTURE(0x100E, stills-only), DOWNLOAD(GetObjectHandles…), PROPS(get/set, surface depends on
stills-vs-movie)}. Movie-record is unreachable over tether. Live view is the cross-mode constant.
