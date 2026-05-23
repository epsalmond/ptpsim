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
  `255.255.255.255` literal + port-`15740` refs) — used only when *no* IP is supplied (to find the
  camera on the LAN). The 15740 references are this discovery/SSDP-style probe, not the data channel.
- `FUJIFILM_TetherApp.exe` has an explicit **Wi-Fi transport with liveview** tuned separately from
  USB: `CFFCamera.IsWifiConnected`, `GetIntervalWifiReadImageForLiveview{L,M,S}`,
  `GetIntervalWifiReadImageForPreview`, `SleepForWifiGetCommand`. So **wireless infra tether DOES
  serve liveview over the LAN IP** (polled GetObject loop, Wi-Fi-tuned intervals per LV size).

### Reconciling with the earlier scan (only port 22 open at 192.168.5.192)
The full scan found 55740 **closed** while the predecessor app connects to 55740 by IP. So in plain
"Wireless Tethering / connected-to-network-idle" the camera does **not** keep 55740 listening — it
must arm the PTP-IP listener only when tethering is actively **armed/standby** in the camera menu
(or the SDK sends a UDP unicast hello to the IP first). **This is the one remaining unknown and the
decisive live test** (below): in the armed wireless-tether state, point our PTP-IP probe straight at
`camera_ip:55740` and run `InitCommandRequest` — if it answers, we've eliminated BLE/AP for ALL work.


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
