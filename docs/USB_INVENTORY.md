---
description: Native raw USB DeviceInfo inventory through camera-initiator.
status: reference
read-when: Selecting a raw USB camera or validating USB inventory behavior.
---

# Native USB inventory

`camera-initiator usb-inventory` reads the selected camera's advertised
DeviceInfo inventory through raw USB. The connection must declare USB command
framing and `session.ownership: initiatorOwned`. Apple daemon-attached
pass-through is a separate transport and is rejected by this command.

Native USB inventory supports Linux and macOS. Select a device by its USB bus
identifier and address. Linux bus identifiers are zero-padded, such as `001`. The adapter checks the selected
manifest establishment, interface descriptors, and output paths before claiming
the interface. Interface class, subclass, and protocol come from the manifest.
Bulk IN, bulk OUT, and interrupt IN endpoint addresses come from descriptors.
The adapter does not detach kernel drivers, seize daemon ownership, change USB
configuration, or select a non-default alternate setting.

```sh
cargo run -p camera-initiator -- \
  --manifest packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml \
  --manufacturer packages/camera-config-data/fuji/fuji.yaml \
  --manufacturer-index packages/camera-config-data/fuji/index.yaml --model gfx100ii \
  --connection usb --usb-bus 001 --usb-address 5 \
  --trace inventory-trace.jsonl --observation inventory-observation.jsonl \
  usb-inventory --output-dir inventory
```

The command executes the manifest USB establishment with
`run_usb_establishment`, then the manifest `readDeviceInfo` action through the
shared action executor. It permits only OpenSession and parameter-free
GetDeviceInfo during establishment. It validates responses, retains transaction
IDs, and uses CloseSession before releasing its interface. CloseSession is
attempted only after a successful OpenSession response and while the command
stream remains synchronized. A session reported as already open is released
without sending CloseSession. A failed run retains its primary error and reports
any cleanup failure. Each transfer and complete container is bounded by the
configured timeout and byte limit.

Every complete DeviceInfo data container is retained, including one followed
by a failed action or non-OK response. The new output directory contains the
action DeviceInfo payload, any establishment DeviceInfo payload, and a JSON
report with every advertised operation, event, property, capture format, and
image format. Advertised values do not establish that an operation is callable.
The canonical observation bundle identifies USB transport and framing. Its
actual-camera firmware is `unknown`, regardless of the selected manifest or
overlay version. The raw DeviceInfo `deviceVersion` string is retained in the
payload and report without interpreting it as an observed firmware version.

This command does not execute selector or property writes, capture, object
deletion, hidden operations, or live view. Scripted acceptance tests establish
host-side behavior. A physical-device smoke run remains a separate acceptance
gate under #176, #540, and #541.
