# ptpsim

`ptpsim` is a scriptable, open-source **camera-protocol simulator** for the
PTP/IP responder role. It loads camera behavior from manifest **data** through
one generic engine. The engine contains no per-manufacturer code. Real-camera
and simulator traffic use the same fail-closed `camera-observation/v1` evidence
contract.

[`DESIGN.md`](DESIGN.md) documents the full architecture.

[`docs/CONTAINER.md`](docs/CONTAINER.md) documents the deployable
`camera-sim-service` container contract.

[`docs/REAL_CAMERA_PTPIP.md`](docs/REAL_CAMERA_PTPIP.md) documents the headless
real-camera workflow.

## Layout

```
crates/
  ptp-core              PTP/IP packet codecs, containers, object/property encoders
  camera-config       manifest schema, validation, queries, bundle->proposal generator
  camera-media-store    filesystem card model, object handles, thumbnails
  camera-sim            generic responder engine + scripting runtime
  protocol-primitives   concern-organized framing/quirk/establishment primitives
  camera-protocol-ffi   optional Swift/Ruby FFI boundary
services/
  camera-sim-service    tokio service: PTP listeners + control HTTP
tools/
  camera-initiator      headless real-camera PTP/IP probe over the shipping engine
  camera-simctl         CLI over the control API
  camera-sim-tui        colorful terminal operator console over the control API
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

This attaches to an existing local `camera-sim-service`. If none is running, it
starts `./run` with the default GFX100 II fixture service, then launches the
operator console.

## CI

Continuous integration uses the workflows under [`.woodpecker/`](.woodpecker/).
The Linux and OCI workflows select Docker `linux/amd64` lanes through repository
and trust requirements. They do not target named hosts. Deployments must supply
the trusted clone image.

An optional workstation can reuse an isolated durable checkout for
default-branch pushes on the unprivileged Linux lane. Other compatible agents
may use ephemeral workspaces.

Linux build caches stay inside disposable containers. The steps do not request
host volumes. Docker image layers and the durable Git object database provide
cross-run reuse. The privileged multi-architecture OCI workflow requires the
deployment's `host-root` lane. A separate Darwin workflow promotes the Apple
XCFramework.

## License

Dual-licensed under MIT OR Apache-2.0.
