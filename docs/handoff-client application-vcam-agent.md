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


WHAT PTPSIM PROVIDES:



  docs/woodpecker.md for the registry contract). Multi-arch (linux/amd64 +
  linux/arm64). Generic, manifest-driven, lease-AGNOSTIC — no pool/lease logic
  in the image, that's yours.
- Baked into the image: the GFX100 II rich manifest at
    /etc/ptpsim/gfx100ii.consolidated.yaml
  (324 props, real descriptors; GENERATED — to consume a different manifest
  per lease, mount one over /etc/ptpsim/ and override --manifest).
- Default CMD already covers --manifest + --media-root + --profile, so
  ad-hoc 'docker run' brings up a healthy instance against an empty card.
- Per-lease run shape (override --instance-id + bind to the lease's IPv6):
    docker run -d --rm \
      --name ptpsim-<lease-uuid> \
      -v <per-lease card directory>:/var/lib/ptpsim/media-root:ro \
      -p [<lease-v6>]:55740:55740 \
      -p [<lease-v6>]:55741:55741 \
      -p [<lease-v6>]:55742:55742 \

        --manifest    /etc/ptpsim/gfx100ii.consolidated.yaml \
        --media-root  /var/lib/ptpsim/media-root \
        --profile     fuji/gfx100ii \
        --instance-id <lease-uuid> \
        --connection  app \
        --command-bind  '[::]:55740' \
        --event-bind    '[::]:55741' \
        --liveview-bind '[::]:55742' \
        --control-bind  '0.0.0.0:8080'   # required if you poll /healthz from
                                         # outside the container's netns
  Ports: 55740 command, 55741 event, 55742 live-view (through-picture).
  IPv6 binds for App Review (NAT64). Media root holds DCIM/<NNN>_FUJI/….

CONTROL PLANE (the contract the sidecar polls):
- GET /healthz → 200 with:
    {"ok":true,"instance_id":…,"profile":…,"connection":…,"bind":"<command_addr>",
     "sessions":N,"media_root":…}
  `up` for the pool snapshot = HTTP 200 AND "ok":true.
- GET /state → current simulator state snapshot for operator/debug inspection.
- PATCH /state → apply a manifest-validated JSON state overlay.
- POST /shutdown → graceful drain. SIGTERM also drains.
- `--state-callback` remains push-first and sends an initial snapshot after
  startup-state application before debounced mutation snapshots.
- DESIGN.md lists future endpoints (`/scenario/load`, `/faults`, …) under
  Scriptability — NOT implemented yet; don't depend on them. Use
  `--startup-state` for boot-time camera state and `PATCH /state` for explicit
  local control mutations.

VALIDATED ON THE PTPSIM SIDE:
- Gates #3 ImageImport, #4 LiveView, #5 black-box smoke: green. The sim runs
  the GFX100 II choreography (including the wire-discovered vendor-prime
  "chord" 0x9054/0x9055/0x9050/0x9053) and enumerates believably (~31 ops,
  ~324 props, real DevicePropDesc). Unmodeled ops → PTP NOT_SUPPORTED.

TASK (client application side):
1. Delete the placeholder `crates/camera-protocol-{core,fuji,ffi}` directories
   if any remain, and any C `vcam` build/runtime no longer used.

   :sha-<8> once you want lease determinism). 'docker pull' on host startup.
3. Spawn one container per lease from the management sidecar (touch the files
   that currently spawn/track the C vcam):
   backend/api/lib/client application/runtime/{service,instance_registry,pool,vcam_nats}.rb
   - one per-lease media root (stage DCIM fixtures or mount per-lease storage,
     bind-mounted RO at /var/lib/ptpsim/media-root)
   - per-lease IPv6 command bind, per-lease control port
   - pass --instance-id = the lease uuid
4. Build the `vcam_pool` snapshot {host,capacity,count,instances:[{ipv6,up}],ts}
   by polling each instance's /healthz. Mirror as today into
   `review_camera_instances`. NATS contract (vcam.cmd.{up,down,restart},
   vcam_pool KV) is unchanged — it's still YOUR contract.
   - Two ways to reach /healthz: (a) override --control-bind 0.0.0.0:8080 and
     publish that port on the lease's IPv6, or (b) keep the loopback default
     and 'docker exec' curl from the sidecar. Either is fine; pick one.
5. Reconcile `PROFILES`: ptpsim ids look like `fuji/gfx100ii`. Refactor the
   client application profile key (e.g. `fuji_gfx100ii`) and the vcam_pool `profile`
   field to the ptpsim id. Keep `model: "GFX100 II"` so the app's model-based
   gating still fires; only `display_name` is the test marker.
6. Graceful lifecycle: drain via POST /shutdown on lease release; SIGTERM on
   timeout. Reap on health-check failure (Docker's own HEALTHCHECK already
   curls /healthz internally — `docker inspect --format '{{.State.Health.Status}}'`
   reads it, or watch the SIGTERM path).

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
