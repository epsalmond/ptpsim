# Desktop TetherApp + Wireless (infrastructure) tethering — analysis

Investigating `~/fuji/FUJIFILM_TetherApp_Win1340.exe` and the camera's "Wireless tethering"
(infrastructure) mode as a possibly-better path for a remote live feed (the model's posing
view) than the reference app BLE→AP→PTP-IP remote-control path.

## What the TetherApp is
- `FUJIFILM_TetherApp_Win1340.exe` = InstallShield installer (PE32+ x64, 16 MB). `setup.cmd`
  installs it as a **Lightroom plugin** (`%APPDATA%\Adobe\Lightroom\Modules\FUJIFILM_TetherApp.lrplugin`).
- So it is **studio tethered capture into Lightroom** (camera shoots → images auto-import to LR),
  not a standalone remote-control app. The plugin (Lua) + Fuji SDK DLLs are inside the
  InstallShield `[0]` payload (needs i6comp/isdcab or a Windows/wine install to extract — TODO).


Extraction chain (reproducible): `7z x` the outer EXE → inner `_tetherapp_embedded_ins_1340.exe`
→ `wine _tetherapp_embedded_ins_1340.exe /extract_all:C:\out` (gives InstallShield `data1.hdr`/
`data1.cab`/`data2.cab`) → `unshield -g App_Executables x data1.cab` (built unshield 1.6.2 from
source; apt blocked). Payload: **`FUJIFILM_TetherApp.exe`** (12.8 MB native C++ app),

SDK — Windows twin of the `FF0018API.so` we already RE'd), `FujiLR.dll` + `FUJIFILM_TetherApp.lrplugin`
(LR glue: a PE wrapping **compiled Lua 5.1** `LuaQ` chunks `FujiFilmTetherServiceProvider.lua` /
`FujiFilmTetherTask.lua`).

### App architecture (top→bottom)
Lightroom Lua plugin → `launchAndConnectToImageCaptureTetherServer` **spawns the local
`FUJIFILM_TetherApp.exe`** (an image-capture tether *server*) and talks to it over a local
`_serverConnection` → the app drives the **SDK (`SDK_*` exports in `FF0018API.dll`,



### How it connects over Wi-Fi by IP (answers "I gave it the IP and it got right in")

are the same logic):
- **`MngTCPIP::Connect(u16 port, u32 ip)`** builds a textbook `sockaddr_in` — `family=AF_INET(2)`,
  `htons(port)`, `htonl(ip)` — and the port is validated against a 3-entry table keyed by
  `port-55740` (`movw #0xd9bc`=55740). **i.e. the desktop tether connects plain PTP-IP TCP to the
  camera's LAN IP on the SAME `55740 / 55741 / 55742` ports as our AP path** — NOT a different port,
  NOT 15740 for the data channel.
- **`FTL_EnumDevice(dev*)`** = register-a-camera. It reads the IP from the device struct and logs
  the octets; a **`0x80000000` sentinel** in the IP field means "no IP → auto-discover". **Given an
  explicit IP it skips broadcast entirely** — exactly the predecessor's "type the camera's IP, it
  connects" path. No BLE, no AP launch, no 60 s window.
- A **UDP broadcast discovery** path also exists (`sendto`/`recvfrom` imported + a
  `255.255.255.255` literal) — used only when *no* IP is supplied (to find the camera on the LAN).
  *(Correction: earlier "port 15740" refs were a FALSE POSITIVE — `0x7c3d` is the bytes of `|=`
  inside C++ `operator|=` strings, not a port. The discovery port is **1900**; see below.)*
- `FUJIFILM_TetherApp.exe` has an explicit **Wi-Fi transport with liveview** tuned separately from
  USB: `CFFCamera.IsWifiConnected`, `GetIntervalWifiReadImageForLiveview{L,M,S}`,
  `GetIntervalWifiReadImageForPreview`, `SleepForWifiGetCommand`. So **wireless infra tether DOES
  serve liveview over the LAN IP** (polled GetObject loop, Wi-Fi-tuned intervals per LV size).

## ✅ VALIDATED LIVE 2026-05-23 — `scripts/connect_wireless_tether.py` works end to end
Ran from a host with no BLE and no AP launch: knock → camera callback → NOTIFY/`200 OK` → PTP-IP
(`Init_Fail 0x2019 Device_Busy` → retry → `Init_Command_Ack camera='GFX100 II'
guid=0870b0610a8b4593b2e79357dd36e050`) → **`OpenSession -> 0x2001 OK`** → `GetDeviceInfo` 843 B.
**This replaces the BLE→AP→PTP-IP path for all wireless work.** One full step:
`PYTHONPATH=. python3 scripts/connect_wireless_tether.py <camera_ip>` (power-cycle camera first;
once per boot). The flow below is the wire detail behind it.

## WIRE-CONFIRMED (2026-05-23, real predecessor-app capture) — supersedes the speculation below

real desktop predecessor app; camera `192.168.4.27`, hosts `192.168.4.44` / `192.168.7.49`=mbp).

**Full handshake (rendezvous/announce — the camera DIALS BACK to the PC):**
```

1. PC --> camera  UDP:51562   "knock", every 10s until accepted, fresh ephemeral src port each time:
       DISCOVERY * HTTP/1.1\r\n  HOST: <PC IP>\r\n  MX: 5\r\n  SERVICE: PCSS/1.0\r\n\x00   (69 B)
   - camera NOT ready -> ICMP type3/code3 (port-unreachable); PC re-knocks next tick.
   - camera ready -> no ICMP; it ARPs for the PC's IP, then:
2. camera --> PC  TCP connect to PC:51560, sends PCSS NOTIFY (103 B):
       NOTIFY * HTTP/1.1\r\n  DSC: <camera IP>\r\n  CAMERANAME: GFX100 II\r\n
       DSCPORT: 15740\r\n  MX: 7\r\n  SERVICE: PCSS/1.0\r\n        <-- DSCPORT = the PTP-IP port to use
3. PC --> camera  on the same :51560 socket:  "HTTP/1.1 200 OK\r\n\x00"  (18 B; 403 would reject)
   camera then closes the 51560 channel (~56 ms total).
4. PC --> camera  TCP connect to camera:<DSCPORT>  -> standard PTP-IP:
       Init_Command_Request (GUID + PC-IP + name + zeros tail) -> Init_Command_Ack -> OpenSession.
```
**Key points:** the **PTP-IP port is ANNOUNCED dynamically** in the NOTIFY's `DSCPORT` (15740 here,
parse it — don't hardcode). The PC **must** answer the NOTIFY with `HTTP/1.1 200 OK` or the camera
aborts (this is the `200 OK`/`403 Forbidden` pair seen in the binary strings). The whole thing is a
NAT-traversal-style rendezvous: the knock advertises the PC, the camera dials back and announces its
endpoint — designed to work even when the camera is behind a router (on the flat LAN it's direct).
The earlier `41641`-to-`.228` "knock" was unrelated Tailscale disco (magic `TS💬`).

**`15740`, not `55740`:** the desktop predecessor uses **standard PTP-IP `15740`**. The `55740/41/42`

port bases for the two apps; the wire capture is ground truth for the desktop tether = `15740`.

**"Works once per boot" — CONFIRMED + explains all earlier failures:** when the camera is "spent"
(already connected once since power-on) the knock to `51562` gets **ICMP type 3 / code 3
(port-unreachable)** — the PCSS daemon stops listening until reboot. The capture shows 52 knocks vs
102 ICMP-unreachables; only fresh-boot knocks were accepted. This is why every probe I ran earlier
failed: the camera was already spent **and** I was hitting port 1900. **Operational rule: power-cycle
the camera before each connection attempt.**

Tool (corrected): `scripts/pcss_discover.py <camera_ip>` — sends the `51562` knock with HOST=our IP,
then checks whether camera `15740` opened.

## (SUPERSEDED speculation) discovery protocol guess from the binary — see WIRE-CONFIRMED above
## The discovery protocol: Fuji "PCSS/1.0" (PC Shoot Service), SSDP-style over UDP:1900

It is **HTTP-over-UDP modeled on SSDP**, but with Fuji-custom verbs/service and on the **broadcast**
address `255.255.255.255:1900` (NOT the standard SSDP multicast `239.255.255.250` — that string is
absent; port 1900 LE is present once).

**Verbs / templates (exact tokens from the binary):**
- **Active search (host → LAN):**
  ```
  DISCOVERY * HTTP/1.1\r\n
  HOST: <addr>:1900\r\n
  MX: 5\r\n
  SERVICE: PCSS/1.0\r\n
  \r\n
  ```
- **Announce (camera → LAN, "here I am"):**
  ```
  NOTIFY * HTTP/1.1\r\n
  ...
  DSC: <...>\r\n
  CAMERANAME: FUJIFILM <model>\r\n
  SERVICE: PCSS\r\n
  \r\n
  ```
- **Responses:** `HTTP/1.1 200 OK\r\n` (accepted) / `HTTP/1.1 403 Forbidden\r\n` (rejected — implies
  a **host-registration / pairing gate**; an un-registered PC gets 403).

**Flow (inferred):** host broadcasts `DISCOVERY … SERVICE: PCSS/1.0` (or listens for the camera's
`NOTIFY`); camera answers `200 OK` carrying `CAMERANAME`/`DSC`; the camera then **arms its PTP-IP
listener (55740/55741/55742)** and the host connects TCP per `MngTCPIP::Connect`. The by-IP path
(`FTL_EnumDevice(ip)`) sends the `DISCOVERY` as a **unicast to the camera IP** instead of broadcast.

**LIVE PROBE RESULT (2026-05-23, camera at 192.168.5.192 in wireless-tether):** sending the
`DISCOVERY * HTTP/1.1 … SERVICE: PCSS/1.0` packet unicast to `cam:1900` AND broadcast — and passively
listening 22 s on `:1900` for the camera's `NOTIFY` — produced **no camera reply**, and 55740 stayed
closed. So the camera does NOT answer PCSS while merely connected-to-network-idle. It emits/answers
PCSS only when actively put into a **"connect to PC"** state on the body **and/or** to a
**registered** host (consistent with the `403 Forbidden` template). Tool: `~/.bin/tmp/pcss_discover.py`.
NOTE: `tcpdump` capture needs root (sudoers here only grants `nmcli`), so L2 broadcast sniffing of a
real predecessor-app session wasn't possible from this host (also not the subnet gateway → can't see
unicast). To capture the genuine handshake: run the predecessor app while sniffing on the camera's
gateway, or grant tcpdump.

### Reconciling with the earlier scan (only port 22 open at 192.168.5.192) — RESOLVED
The full scan found 55740 **closed**, and the live PCSS probe above confirms why: the camera keeps
**no** PTP-IP listener (and does not answer PCSS) until the **PCSS `DISCOVERY`/`NOTIFY` handshake
completes from a registered host with the body actively in "connect to PC" state**. The "UDP unicast
hello" hypothesis is now concrete = the **`DISCOVERY * HTTP/1.1 … SERVICE: PCSS/1.0` packet on
UDP:1900**. Only after the camera returns `200 OK` (not `403`) does it arm 55740 for the TCP connect.
**Open item / decisive test:** capture a real predecessor-app PCSS exchange (needs gateway-side
tcpdump or root here) to learn (a) the exact `DISCOVERY`/`NOTIFY` header set incl. how a host
registers/authenticates past the `403` gate, and (b) confirm 55740 opens immediately post-`200 OK`.
Once we can pass the PCSS gate, point the PTP-IP probe at `camera_ip:55740` — eliminating BLE/AP for
ALL future work.


## Transport taxonomy (libfuji `dev.md`)
- `USB_TETHER_SHOOT`: **Liveview + live image download + settings** over USB.
- `WIRELESS_TETHER`: *identical functionality, different transport* (Wi-Fi). This is "Wireless
  tethering" = camera joins an **infrastructure** Wi-Fi network (gets an IP), reachable directly.
- `AUTOSAVE` (PC Auto Save, 2014–2022, removed): auto-push shots to a host.

## CORRECTION (2026-05-23, live scan): plain "Wireless Tethering" exposes NO network service
Camera in Wireless Tethering on the LAN (`192.168.5.192`): full 1–65535 scan → **only port 22**
open (a user-added SSH backdoor; dead end). **No PTP-IP (55740 closed), no XLV.** So plain
wireless-tether does NOT expose PTP-IP-by-IP. PTP-IP (55740) is only opened in **AP mode** after
the BLE `FUNCTION_LAUNCH`. **XLV is a *separate* infra-Wi-Fi mode** that starts a Python web
server (the browser live-view; "works but bad" per user). So the camera has ≥3 distinct network
faces — AP-mode PTP-IP, XLV web, and plain wireless-tether (nothing) — selected by camera UI mode.

## Two robustness/architecture wins from wireless tethering
1. **Infrastructure mode eliminates the BLE/AP juggling.** Today we BLE-launch the camera AP
   (flaky, ~60 s window, re-pair churn). In wireless-tether the camera joins our LAN and is a
   normal IP host — the SDK supports it (`XSDK_Detect` flags `XSDK_DSC_IF_WIFI_LOCAL` /
   `XSDK_DSC_IF_WIFI_IP`). **We could connect our PTP-IP probe straight to the camera's IP — no
   BLE, no AP, no timeout.** Big stability win for *all* our work.
2. The tether path bundles the **full SDK** (more API surface / options) than the phone reference app.

## The hard constraint (live feed vs camera usable)
- **Live view = sensor→stream = takes over the camera.** Confirmed on our path: both live-view
  (`DF01=22`) AND import/auto-transfer (`DF01=20`) **blank the LCD and disable on-body controls**
  (user-observed). The desktop-tether live view uses the **same `0x101C InitiateOpenCapture` +
  `GetObject(0x80000001)` ~13 fps loop** (tether-walk FINDINGS) — so its live view is the same
  sensor-takeover mechanism; **[unconfirmed whether the desktop tether blanks the LCD or is gentler]**.
- **HDMI works for the model because it MIRRORS the display** (hardware passthrough) without a
  control-takeover — the photographer keeps composing. The PTP-IP remote-shooting/live-view mode
  has no equivalent "mirror without takeover"; XLV (camera web live-view) is the closest network
  mirror but the user reports it's bad/camera-specific.

## Implications for the model's posing-feed goal
- A network feed that mirrors *without* taking over the camera likely only exists via **XLV**
  (bad) — the SDK/PTP-IP live-view always takes the camera.
- **Pragmatic path: shoot remotely.** Run our 60 fps live feed for the model and trigger shots
  from the computer (remote shutter release). The blank camera LCD is moot if nobody uses the
  body. (Needs remote shutter — `XSDK_Release`/the capture op.)
- Whether the **desktop tether keeps the camera usable while feeding live view** is the key open
  question the TetherApp could answer — needs the plugin/SDK extracted or run.

## Recommended next steps
1. **Set the camera to Wireless Tethering** (join the lab Wi-Fi) → get its IP → point the PTP-IP
   probe at that IP directly. Validates the no-BLE/no-AP path (huge robustness win) and lets us
   test the tether-shoot function mode vs the reference app's `DF01=22`.
2. **Extract the `.lrplugin`** (InstallShield `[0]` via i6comp / wine install) → read the Lua to
   see its connection model, whether it offers live view, and which function mode / SDK calls it
   uses (does it blank the camera?).
3. If a non-takeover network feed isn't available, implement **remote shutter release** so the
   "live feed for the model + shoot from the computer" workflow is complete.
