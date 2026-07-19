# ptpsim

A scriptable, open-source **camera-protocol simulator** (PTP/IP, responder role).
ptpsim runs a believable camera from manifest **data** — one generic engine, no
per-manufacturer code — and records real-camera and simulator traffic through
one fail-closed `camera-observation/v1` evidence contract.

See [`DESIGN.md`](DESIGN.md) for the full design.

The deployable `camera-sim-service` container contract is documented in
[`docs/CONTAINER.md`](docs/CONTAINER.md).

The headless real-camera workflow is documented in
[`docs/REAL_CAMERA_PTPIP.md`](docs/REAL_CAMERA_PTPIP.md).

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

## Build & test

```sh
cargo test            # Rust workspace
```

## Local TUI

```sh
scripts/run-tui
```

This attaches to an existing local `camera-sim-service`, or starts `./run` with
the default GFX100 II fixture service before launching the operator console.

## CI

Continuous integration runs via the workflows under [`.woodpecker/`](.woodpecker/).
The Linux and OCI workflows request a Docker `linux/amd64` agent for the
current repository and pipeline event rather than a named host. Deployments
must supply the trusted clone image. An optional workstation accepts push
events only and reuses an isolated durable checkout; public pull requests remain
on the platform's ephemeral NAS workspace.

Linux steps keep build caches inside their disposable containers rather than
requesting host volumes. This lets the public workflow use an unprivileged
runner while Docker image layers and the durable Git object database provide
cross-run reuse. The privileged multi-architecture OCI workflow remains on NAS;
Apple XCFramework promotion remains a separate Darwin workflow.

## License

Dual-licensed under MIT OR Apache-2.0.
