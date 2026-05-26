# bluetooth-wrapper

Small macOS command-line wrapper around private CoreBluetooth APIs used by System Settings.

## Build

```sh
make
```

From the repository root, the preferred discoverable build entrypoint is:

```sh
scripts/build/build-bluetooth-wrapper.sh
```

## Usage

```sh
./bluetooth-wrapper
./bluetooth-wrapper list
./bluetooth-wrapper forget <name-or-id>
```

`list` is the default verb. It prints the best available ID, name, paired state, connection state, and raw device description for known paired/recent devices.

`forget` accepts exactly one name or ID. If more than one device matches, it refuses to forget anything. `delete` is still accepted as a compatibility alias, but `forget` is the preferred verb.

Use only the device name or the device ID/address from `list`, not the whole output row. Quote names that contain spaces.

For this `list` row:

```text
38-7c-76-74-73-21                       GFX100 II                         paired=yes connected=no  <IOBluetoothDevice: 0x125e06050 GFX100 II, 38-7c-76-74-73-21>
```

Either of these is the exact command line to forget it:

```sh
./bluetooth-wrapper forget 'GFX100 II'
./bluetooth-wrapper forget 38-7c-76-74-73-21
```

There is also a local helper for runs that need to execute outside a sandboxed automation context:

```sh
tools/bt-local list
tools/bt-local forget 38-7c-76-74-73-21
```

## Notes

This uses private CoreBluetooth selectors:

- `CBDiscovery` with discovery flags `0x80000a00000`
- `CBDiscovery +devicesWithDiscoveryFlags:error:`
- `CBController -deleteDevice:completion:`
- `IOBluetoothDevice -remove`
- `IOBluetoothDevice -forceRemove`

These APIs are not stable, are not App Store safe, and may require permissions or entitlements on some macOS versions.

## How These APIs Were Found

This project came from inspecting the macOS System Settings Bluetooth pane on this machine.

The main System Settings app is only a host. Its sidebar plist points the Bluetooth row at the extension bundle identifier `com.apple.BluetoothSettings`:

```sh
plutil -p "/System/Applications/System Settings.app/Contents/Resources/Sidebar.plist"
```

That bundle is implemented here:

```text
/System/Library/ExtensionKit/Extensions/Bluetooth.appex
```

The extension links against public `CoreBluetooth.framework`, but it uses private ObjC classes and selectors that are not in the public CoreBluetooth headers:

```sh
otool -L "/System/Library/ExtensionKit/Extensions/Bluetooth.appex/Contents/MacOS/Bluetooth"
strings -a "/System/Library/ExtensionKit/Extensions/Bluetooth.appex/Contents/MacOS/Bluetooth" | rg -i "deleteDevice|CBDiscovery|paired|unpair|device"
nm -m "/System/Library/ExtensionKit/Extensions/Bluetooth.appex/Contents/MacOS/Bluetooth" | rg -i "BluetoothManager|Device|CB"
```

The useful symbols and log strings in the extension included:

```text
Bluetooth.BluetoothManager.pairedDevices
Bluetooth.BluetoothManager.nearbyDevices
Bluetooth.Device.isPaired
CBDiscovery
CBController
CBDeviceRequest
CBDeviceSettings
setDeviceFoundHandler:
setDeviceLostHandler:
setDiscoveryFlags:
activateWithCompletion:
deleteDevice:completion:
unpair(from device: %s (%s))
SUCCESS: unpair(from device: %s (%s))
Number of Paired Devices: %ld
Number of Nearby Devices: %ld
```



```text
0x80000a00000
```

as the discovery flags. There is also an Apple-internal branch that widens the flags to:

```text
0x6082000a00000
```

The normal branch is what this tool uses.

The forget-device path was visible around the `unpair(from device:)` log string. The extension eventually sends:

```objc
[controller deleteDevice:device completion:...]
```

where `controller` is a private `CBController` instance and `device` is the private `CBDevice` object from the discovery list.

Classic Bluetooth devices may only appear as `IOBluetoothDevice` objects. For those, this tool tries private `IOBluetoothDevice` and HCI link-key removal selectors instead of passing the object to `CBController`.

On macOS 15.7.5, `CBController -deleteDevice:completion:` returns `CBErrorDomain` code `-71168` with `Missing entitlement: com.apple.bluetooth.system` for this unsigned command-line tool. Ad-hoc signing with that private entitlement causes macOS to kill the binary at launch, so this entitlement appears to be Apple-restricted. For devices that only expose an `IOBluetoothDevice` paired record, `forget` may report failure even though the same device is listed.

The private CoreBluetooth class and selector inventory was also visible in the dyld shared cache strings:

```sh
strings -a /System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld/dyld_shared_cache_* | rg "CBController|CBDiscovery|deleteDevice:completion:|devicesWithDiscoveryFlags"
```

That showed methods such as:

```text
+[CBDiscovery devicesWithDiscoveryFlags:error:]
-[CBDiscovery discoveredDevices]
-[CBDiscovery setDeviceFoundHandler:]
-[CBDiscovery setDeviceLostHandler:]
-[CBDiscovery setDiscoveryFlags:]
-[CBDiscovery activateWithCompletion:]
-[CBController deleteDevice:completion:]
-[CBController performDeviceRequest:device:completion:]
-[CBController modifyDevice:settings:completion:]
```

Because these are private SPI, the implementation avoids compile-time private headers and uses Objective-C runtime calls through `NSClassFromString`, `NSSelectorFromString`, and typed `objc_msgSend` casts. This keeps the code buildable with normal Command Line Tools, but it does not make the API stable or supported.
