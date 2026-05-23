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

## Transport taxonomy (libfuji `dev.md`)
- `USB_TETHER_SHOOT`: **Liveview + live image download + settings** over USB.
- `WIRELESS_TETHER`: *identical functionality, different transport* (Wi-Fi). This is "Wireless
  tethering" = camera joins an **infrastructure** Wi-Fi network (gets an IP), reachable directly.
- `AUTOSAVE` (PC Auto Save, 2014–2022, removed): auto-push shots to a host.

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
