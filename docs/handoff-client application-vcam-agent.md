# Handoff prompt — client application backend: replace the C vcam with ptpsim

> Paste the block below to a backend agent in `~/git/client application`. ptpsim ships a
> believable GFX100 II PTP/IP responder as a generic, manifest-driven binary

> is client application-side. The companion iOS-side docs are
> [`handoff-ios-agent.md`](handoff-ios-agent.md) (production app adopting the
> manifest/FFI) and
> [`handoff-client application-ios-integration-tests.md`](handoff-client application-ios-integration-tests.md)
> (iOS integration tests against ptpsim).

```
You're replacing client application's C `vcam` (the App-Review / dev "review camera") with
ptpsim's simulator, in ~/git/client application. ptpsim ALREADY provides a believable
GFX100 II PTP/IP responder driven by a manifest; your scope is client application's Ruby


WHAT PTPSIM PROVIDES (~/git/ptpsim):
- Binary: `camera-sim-service`
    cargo build -p camera-sim-service --release
  Generic, manifest-driven, lease-AGNOSTIC. No pool/lease logic — that's yours.
- Manifest: packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml
  (the rich GFX100 II — 324 props, real descriptors; GENERATED, do not hand-edit).
- Run shape:
    camera-sim-service \
      --manifest <path>/gfx100ii.consolidated.yaml \
      --media-root <per-lease card directory> \
      --profile fuji/gfx100ii --instance-id <lease-uuid> \
      --command-bind  '[::]:55740' \
      --event-bind    '[::]:55741' \
      --liveview-bind '[::]:55742' \
      --control-bind  '127.0.0.1:<ephemeral>'
  IPv6 binds for App Review (NAT64). Ports: 55740 command, 55741 event,
  55742 live-view (through-picture). Media root holds DCIM/<NNN>_FUJI/….

CONTROL PLANE (the contract the sidecar polls):
- GET /healthz → 200 with:
    {"ok":true,"instance_id":…,"profile":…,"bind":"<command_addr>",
     "sessions":N,"media_root":…}
  `up` for the pool snapshot = HTTP 200 AND "ok":true.
- POST /shutdown → graceful drain. SIGTERM also drains.
- That is the WHOLE current contract. DESIGN.md lists future endpoints
  (/scenario/load, /faults, …) under Scriptability — NOT implemented yet;
  don't depend on them. If you need richer control, request it upstream.

VALIDATED ON THE PTPSIM SIDE:
- Gates #3 ImageImport, #4 LiveView, #5 black-box smoke: green. The sim runs
  the GFX100 II choreography (including the wire-discovered vendor-prime
  "chord" 0x9054/0x9055/0x9050/0x9053) and enumerates believably (~31 ops,
  ~324 props, real DevicePropDesc). Unmodeled ops → PTP NOT_SUPPORTED.

TASK (client application side):
1. Delete the placeholder `crates/camera-protocol-{core,fuji,ffi}` directories
   if any remain, and any C `vcam` build/runtime no longer used.
2. Spawn one ptpsim instance per lease from the management sidecar (touch the
   files that currently spawn/track the C vcam):
   backend/api/lib/client application/runtime/{service,instance_registry,pool,vcam_nats}.rb
   - one per-lease media root (stage DCIM fixtures or mount per-lease storage)
   - per-lease IPv6 command bind, per-lease control port
   - pass --instance-id = the lease uuid
3. Build the `vcam_pool` snapshot {host,capacity,count,instances:[{ipv6,up}],ts}
   by polling each instance's /healthz. Mirror as today into
   `review_camera_instances`. NATS contract (vcam.cmd.{up,down,restart},
   vcam_pool KV) is unchanged — it's still YOUR contract.
4. Reconcile `PROFILES`: ptpsim ids look like `fuji/gfx100ii`. Refactor the
   client application profile key (e.g. `fuji_gfx100ii`) and the vcam_pool `profile`
   field to the ptpsim id. Keep `model: "GFX100 II"` so the app's model-based
   gating still fires; only `display_name` is the test marker.
5. Graceful lifecycle: drain via POST /shutdown on lease release; SIGTERM on
   timeout. Reap on health-check failure (no /healthz for N seconds → kill +
   re-spawn).
6. Package: build the deployable image (amd64 + arm64) embedding the binary
   and a default consolidated manifest.

VALIDATE (the remaining DESIGN gates — client application-owned):
- #6 review-mode e2e: a review build leases ONE IPv6 instance; the app
  connects, does live-view, browses, downloads a JPEG.
- #7 soak: 5 instances on the 1 GB VM profile, no unbounded memory.

HARD CONSTRAINTS:
- ptpsim stays lease-agnostic. All leasing / pooling / orchestration / NATS

- The sim is the RESPONDER; the app is the initiator. They share MANIFESTS,
  not state.
- Need richer behavior or an unmodeled op? That's a ptpsim manifest/engine
  change to request upstream — don't reimplement protocol behavior in the
  sidecar.

BACKGROUND:

  split and the /healthz contract.

  reconciliation in context.
```
