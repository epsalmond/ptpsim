# docs/ index

Index of the ptpsim documentation tree. Update this file in the same change
that adds, moves, or removes a doc. Each doc's own YAML frontmatter
(`description` / `status` / `read-when`) is the per-doc source of truth; this
index is a navigational summary of it.

## Reference

| Doc | Purpose | Read when |
| --- | --- | --- |
| [`../DESIGN.md`](../DESIGN.md) | Baseline architecture — purpose, the central observation loop, the generic engine, the transport/mode matrix | Architecture-level decisions; needing the system-wide picture |
| [`MANIFEST_SCHEMA.md`](MANIFEST_SCHEMA.md) | The manifest schema contract — template grammar, step/observation vocabulary, predicates, wire conventions; wins over code and other docs on conflict | Authoring or reviewing manifests, schema changes, or anything that parses/emits manifest data |
| [`INTEGRATION.md`](INTEGRATION.md) | How a client app consumes `camera-protocol-ffi` — the platform-neutral seam, binding generation, both query surfaces, the action catalog | Wiring an app to the FFI, or changing the FFI surface consumers depend on |
| [`REAL_CAMERA_PTPIP.md`](REAL_CAMERA_PTPIP.md) | Operating `camera-initiator`, the headless real-camera PTP/IP probe lane | Driving a real camera, capturing traces, or debugging manifests against hardware |
| [`NIKON_LSS.md`](NIKON_LSS.md) | Spec of the sans-I/O Nikon LSS authentication primitive in `protocol-primitives`, with clean-room provenance | Nikon BLE authentication / LSS crypto work; verifying the clean-room boundary |
| [`ANDROID_INTEGRATION.md`](ANDROID_INTEGRATION.md) | Consuming the published `CameraProtocolFFI` `.aar` from a Kotlin app — release contents, ABIs, Gradle wiring | Integrating or updating the Android bindings; Android packaging/ABI issues |
| [`SPM_INTEGRATION.md`](SPM_INTEGRATION.md) | Consuming a pinned, promoted `CameraProtocolFFI.xcframework` via SPM or direct vendoring | Adding or updating an iOS/macOS consumer's dependency |
| [`APPLE_FFI_RELEASES.md`](APPLE_FFI_RELEASES.md) | How Apple FFI artifacts are promoted — supported slices, tag-driven promotion procedure | Cutting or consuming an Apple FFI release; changing the xcframework pipeline |
| [`CONTAINER.md`](CONTAINER.md) | The `camera-sim-service` container image — platforms, runtime port contract, publication guidance | Building, publishing, or running the simulator container |
| [`CI.md`](CI.md) | Reading ptpsim's Woodpecker CI — workflow layout, status meanings, triage rules of thumb | A pipeline is red or errored; changing `.woodpecker/` workflows |
| [`CODE_REVIEW_WORKFLOW.md`](CODE_REVIEW_WORKFLOW.md) | The durable PR review-thread workflow — SHA-pinned reviews, thread triage, delta reviews, readiness gates | Running or remediating a review pass; resuming a PR after context loss |

## Plans (docs/plans/)

`TRANSPORTS.md` lives at the docs/ root but is a plan, so it is listed here.

| Doc | Status | One-line |
| --- | --- | --- |
| [`TRANSPORTS.md`](TRANSPORTS.md) | plan | PTP/USB, XLV, and wireless-tether transports as manifest-driven adapters; the initiator-side USB model is specified in `MANIFEST_SCHEMA.md` §11.29, the rest remains paper design |
| [`plans/canonical-observation-loop.md`](plans/canonical-observation-loop.md) | shipped | Implementation contract (#305) for the one supported fail-closed evidence loop, observation JSONL through atomic manifest apply |
| [`plans/manifest-system.md`](plans/manifest-system.md) | plan | Cross-cutting direction for the manifest system — data repo / standalone engine / ptpsim as co-consumer |
| [`plans/ios-rewrite-p0-p1-ble-mvp.md`](plans/ios-rewrite-p0-p1-ble-mvp.md) | shipped | BLE-only MVP slice of the iOS rewrite; its schema contract (§11) moved to `MANIFEST_SCHEMA.md` |
| [`plans/ios-rewrite-service-architecture.md`](plans/ios-rewrite-service-architecture.md) | shipped | Greenfield manifest-driven iOS service architecture; supersedes `ios-adoption.md` |
| [`plans/camera-config.md`](plans/camera-config.md) | shipped | Implementation plan for `camera-config`, the manifest/config system with modes as a first-class axis |
| [`plans/action-verbs.md`](plans/action-verbs.md) | shipped | Approved schema decision adding action verbs (capture/transfer) alongside `entries[]` mode transitions |
| [`plans/ffi-surface.md`](plans/ffi-surface.md) | historical | Original FFI surface design sketch; superseded by `INTEGRATION.md` |
| [`plans/ios-adoption.md`](plans/ios-adoption.md) | historical | Pre-rewrite plan for the existing client application iOS app to adopt ptpsim; superseded by the rewrite |



[`consults/`](consults/README.md) has its own README and frontmatter schema (cross-project request/response records); it is not covered by the schema above.

Historical root-level docs — [`handoff-ios-agent.md`](handoff-ios-agent.md), [`handoff-ios-ble-mvp.md`](handoff-ios-ble-mvp.md), and [`internal-async-notes.md`](internal-async-notes.md) — are kept for provenance only.
