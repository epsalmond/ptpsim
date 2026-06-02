# `camera-protocol-ffi` — Python bindings

uniffi-generated Python surface for the ptpsim manufacturer-index FFI.
Same types and methods as the Swift / Kotlin sides; consume from any
Python ≥ 3.9.

Primary consumer: client application's test-injection TUI, which uses these
bindings to construct `Observation` values and feed them through
`recognize` / `establishment` for in-process test fixtures (no real
BLE radio).

## Install

Wheels are published as GitHub release assets on each push to `main`,
attached to a `sha-<8>` release on the merge commit. Pull the wheel for
your platform from:

```
https://github.com/epsalmond/ptpsim/releases
```

Then:

```bash
pip install camera_protocol_ffi-<version>-cp3-...-<platform>.whl
```

## Local build (host platform only)

```bash
./ci/build-python-wheel.sh
# Wheel lands in packages/camera-protocol-python/dist/
pip install packages/camera-protocol-python/dist/*.whl
```

The wheel is platform-specific (it bundles `libcamera_protocol_ffi.so`
on Linux / `.dylib` on macOS); a wheel built on Linux/x86_64 won't
install on macOS/arm64.

## Usage

The Python surface mirrors the Swift one in plan §3.3 +
`docs/INTEGRATION.md` §9. Quick recognize → establishment round-trip:

```python
from camera_protocol_ffi import (
    ConfigStore, Observation, Recognition, KeyValue,
)

# Load. modelBodies takes (model_id, yaml_text) pairs.
store = ConfigStore.from_manufacturer_index(
    index_yaml=open("fuji/index.yaml").read(),
    model_bodies=[KeyValue(key="gfx100ii",
                           value=open("fuji/gfx100ii/gfx100ii.yaml").read())],
)

# Inject a synthetic LEGACY advert.
obs = Observation.BLE_ADVERT(
    service_uuids=["AF854C2E-B214-458E-97E2-912C4ECF2CB8"],
    manufacturer_data=bytes.fromhex("02 44 73 2a 80".replace(" ", "")),
    local_name="GFX100 II",
)
result = store.recognize(obs)

if isinstance(result, Recognition.CANDIDATE):
    plan = store.establishment(
        model=result.model,
        connection=result.connection,
        initial_scope=result.runtime_scope,
    )
    # plan.steps is the walkable Step sequence — the same shape the iOS
    # dispatcher consumes.
```

## What's available

Every type from plan §3.3 + §11: `Observation`, `Recognition`,
`Candidate` / `Disambiguate` / `NoMatch`, `EstablishmentPlan`, the
7-verb `Step` grammar, `StepOptions`, `StepValue` (with optional
`transform`), `AcquireSource`, `BleNotifyUntil`, `Predicate`,
`PredicateOp`, `ValueTransform`, plus the existing single-body surface
(`connections`, `mode_entry`, `operation_available`, …).

Naming is Python-conventional: snake_case methods, `Observation.BLE_ADVERT`
for enum variants, `bytes` for byte vectors. The uniffi Python codegen
handles the conversions.
