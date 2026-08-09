# ptpsim

`ptpsim` is a scriptable, open-source **camera-protocol simulator** and behavior
description engine. It loads camera behavior from manifest data. The engine
tries very hard to have no per-manufacturer code.

In theory this allows any camera-control app to be written to use any camera
ptpsim supports. Manifest (camera config) updates can be loaded at runtime. Full
e2e scripting is possible for testing and development.

It is the camera-behavior driver for https://fujikage.io. Anyone can add new
camera support, fix camera behavior bugs, etc.

[`DESIGN.md`](DESIGN.md) full architecture.

[`docs/CONTAINER.md`](docs/CONTAINER.md) `camera-sim-service` hosts the ptpsim
simulator. This lets someone connect real camera tethering apps to a
real-seeming camera for test and review purposes. Or whatever. One could
theoretically feed the sim mjpeg frames and make this a more realistic network
camera.

[`docs/REAL_CAMERA_PTPIP.md`](docs/REAL_CAMERA_PTPIP.md) Opposite of the camera
simulator. Drive a real camera using the manifest. This could let you test new
cameras, validate behavior, or reproduce failures.

## Layout

```
crates/
  ptp-core              PTP/IP packet codecs, containers, object/property encoders
  camera-config       manifest schema, validation, queries, bundle->proposal generator
  camera-media-store    camera SDcard model, object handles, thumbnails
  camera-sim            generic responder engine + scripting runtime
  protocol-primitives   concern-organized framing/quirk/establishment primitives
  camera-protocol-ffi   optional Swift/Ruby FFI boundary
services/
  camera-sim-service    tokio service: PTP listeners + control HTTP
tools/
  camera-initiator      headless real-camera PTP/IP probe over the shipping engine
  camera-simctl         CLI over the control API
  camera-sim-tui        TUI for local app testing
packages/
  camera-config-data         manifest schema, golden packets, captured camera manifests
  fixtures              small redistributable media fixtures
```

## Build and test

```sh
cargo test            # Rust workspace
```

## Local TUI

```sh
scripts/run-tui
```

This runs or attaches to an already running `camera-sim-service` on localhost.
You can see what the state of the camera is. Really handy for app development.

## License

Dual-licensed under MIT OR Apache-2.0.

## Contributing

To change existing manifest config, I need to see data, namely wire evidence. A
TSV of a pcap is fine. Part of the reason this project exists is because even
manufacturer SDKs don't get this exactly right.

Any code submissions need to have test coverage. All contributions must be under
the same license.
