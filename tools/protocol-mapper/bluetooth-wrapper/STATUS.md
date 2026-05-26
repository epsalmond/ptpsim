# bluetooth-wrapper status

Date: 2026-05-02

## Current state

- `list` is fast now. The original long delay came from active `IOBluetoothDeviceInquiry` plus CoreBluetooth run-loop waiting. Normal listing now uses known paired/recent devices and returns quickly.
- The CLI verb is now `forget`; `delete` remains accepted as a compatibility alias.
- README usage now includes exact command examples:
  - `./bluetooth-wrapper forget 'GFX100 II'`
  - `./bluetooth-wrapper forget 38-7c-76-74-73-21`
- `tools/bt-local` was added as a narrow helper for runs that need to execute outside the automation sandbox:
  - `tools/bt-local list`
  - `tools/bt-local forget <name-or-id>`
  - `tools/bt-local probe <fixed-probe-mode> <name-or-id>`
- The binary currently compiles with `make` or `scripts/build/build-bluetooth-wrapper.sh`.

## What was fixed

- Avoided a segfault from treating private scalar selectors as object returns:
  - `CBDevice -deviceFlags` returns `uint64_t`, not `id`.
  - Runtime method return types are now checked before object/integer `objc_msgSend` calls.
- Avoided the previous `IOBluetoothDeviceInquiry` dealloc crash by removing active inquiry from the normal list path.
- `CBController` completion waits were changed to run-loop based waiting in probe code, revealing the actual CoreBluetooth entitlement error instead of timing out on the main thread.

## GFX100 II forget attempt

Target device:

```text
38-7c-76-74-73-21  GFX100 II  paired=yes connected=no
```

Observed state from probes:

- It appears as `IOBluetoothDevice`.
- `isPaired=yes`
- `isBRPaired=yes`
- `isLEPaired=no`
- `isMCPaired=no`
- `isiCloudPaired=no`
- `isLowEnergyDevice=yes`
- Attached `CBClassicPeer` reports `Unpaired`.
- Attached `CBPeripheral` has the camera name and a UUID, but deleting a constructed `CBDevice` still fails without entitlement.

Tried without success:

- `IOBluetoothDevice -remove`
- `IOBluetoothDevice -forceRemove`
- `IOBluetoothDevice -removeLinkKey`
- `IOBluetoothHostController -BluetoothHCIDeleteStoredLinkKey:inDeleteAllFlag:outNumKeysDeleted:`
- `BluetoothManager -unpairDevice:`
- `BluetoothDevice -unpair`
- `BluetoothManager -_removeDevice:`
- `/opt/homebrew/bin/blueutil --unpair 38-7c-76-74-73-21`
- `/opt/homebrew/bin/blueutil --unpair 387c76747321`

CoreBluetooth result:

- `CBController -deleteDevice:completion:` returns `CBErrorDomain -71168`.
- The underlying message is `Missing entitlement: com.apple.bluetooth.system`.
- Ad-hoc signing with `com.apple.bluetooth.system` caused macOS to kill the binary at launch, so the entitlement appears Apple-restricted.

## Open issue

`forget` does not currently forget the GFX100 II. It fails quickly and reports that the paired record still exists.

One small cleanup was made just before pausing: explanatory messages in `main.m` were adjusted so BluetoothManager and IOBluetooth failures report the correct backend. That final cleanup should be rebuilt/verified with:

```sh
scripts/build/build-bluetooth-wrapper.sh
tools/bt-local forget 38-7c-76-74-73-21
```

## Possible next directions

1. Use System Settings automation.
   - Most practical option.
   - Apple’s UI has the entitlement needed to forget devices.
   - Could drive the Bluetooth pane with Accessibility/AppleScript/Computer Use.


   - Investigate whether there is an unentitled route to the same privileged service.
   - Risk: entitlement check likely blocks this.

3. Root/bluetoothd persistence investigation.
   - Pairing record may live in root-owned bluetoothd storage.
   - Would require sudo/password, careful backups, and probably restarting bluetoothd.
   - More invasive and higher risk.

4. Keep wrapper as a fast lister.
   - Leave `forget` as best-effort with clear failure output for devices blocked by macOS entitlements.

Recommended next step if resumed: try System Settings automation first.
