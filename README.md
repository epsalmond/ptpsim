# ptpsim

A scriptable, open-source **camera-protocol simulator** (PTP/IP, responder role)
and probe toolkit. ptpsim runs a believable camera from manifest **data** — one
generic engine, no per-manufacturer code — and pairs with `camera-probe` to learn
new cameras from observed behavior.

See [`DESIGN.md`](DESIGN.md) for the full design.

## Layout

```
crates/
  ptp-core              PTP/IP packet codecs, containers, object/property encoders
  camera-manifest       manifest schema, validation, queries, bundle->proposal generator
  camera-media-store    filesystem card model, object handles, thumbnails
  camera-sim            generic responder engine + scripting runtime
  protocol-primitives   concern-organized framing/quirk/establishment primitives
  camera-protocol-ffi   optional Swift/Ruby FFI boundary
services/
  camera-sim-service    tokio service: PTP listeners + control HTTP
tools/
  camera-simctl         CLI over the control API
  camera-probe          Python probe/exploration tool (emits JSONL observation bundles)
packages/
  protocol-spec         manifest schema, golden packets, captured camera manifests
  fixtures              small redistributable media fixtures
```

## Build & test

```sh
cargo test            # Rust workspace
( cd tools/camera-probe && python -m pytest -q )   # probe
```

## License

Dual-licensed under MIT OR Apache-2.0.
