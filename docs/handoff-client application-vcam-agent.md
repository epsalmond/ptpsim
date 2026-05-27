# Handoff prompt — client application: replace the C vcam with ptpsim (camera-sim-service)

> Paste the block below to the client application agent. ptpsim provides a believable GFX100 II


```
You're replacing client application's C `vcam` (the App-Review / dev camera) with ptpsim's
simulator, in ~/git/client application. ptpsim already provides a believable GFX100 II PTP/IP
responder driven by a manifest; your job is the client application-side cutover (Ruby management
plane + deployment), NOT camera-protocol code.

WHAT PTPSIM PROVIDES (~/git/ptpsim):
- Binary: `camera-sim-service` (cargo build -p camera-sim-service --release). Generic,

- Manifest: packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml
  (the rich GFX100 II — 324 props, real descriptors; regenerable, do not hand-edit).
- Run:
    camera-sim-service \
      --manifest <…>/gfx100ii.consolidated.yaml --media-root <per-lease card dir> \
      --profile fuji/gfx100ii \
      --command-bind '[::]:55740' --event-bind '[::]:55741' --liveview-bind '[::]:55742' \
      --control-bind '127.0.0.1:<ctl>'
  IPv6 binds are required (Apple Review is NAT64/IPv6-only). Ports: 55740 command,
  55741 event, 55742 live-view (through-picture). Media root holds DCIM/<NNN>_FUJI/….
- Control plane: `GET /healthz` →
    {"ok":true,"instance_id":…,"profile":…,"bind":…,"sessions":N,"media_root":…}
  `POST /shutdown` drains gracefully; SIGTERM does too.
- Validated: the sim runs the live-view + image-import choreography and enumerates
  believably (ptpsim gates #3/#4/#5). Behavior is the manifest's; unmodeled ops return
  PTP NOT_SUPPORTED (0x2005).

TASK (client application side):
1. Delete the placeholder `crates/camera-protocol-{core,fuji,ffi}` if any remain, and
   any C vcam build/runtime no longer used.
2. Point the management sidecar at the binary — spawn one camera-sim-service per lease
   (per-lease media root + IPv6 command bind), in:
   backend/api/lib/client application/runtime/{service,instance_registry,pool,vcam_nats}.rb
   (whatever currently spawns/tracks the C vcam).
3. Build the `vcam_pool` snapshot {host,capacity,count,instances:[{ipv6,up}],ts} by
   polling each instance's `/healthz` (use the shape above; `up` = HTTP 200 + "ok":true).
4. Reconcile the `PROFILES` key (e.g. fuji_gfx100ii → profile id `fuji/gfx100ii`); keep
   `model: "GFX100 II"` for app-side gating.
5. Graceful lifecycle: drain via `/shutdown` + SIGTERM on lease release.

VALIDATE (the remaining ptpsim DESIGN gates, client application-side):
- #6 review-mode e2e: a review build leases ONE IPv6 instance; the app connects, does
  live-view, browses, downloads.
- #7 soak: 5 instances on the 1 GB VM profile, no unbounded memory.

HARD CONSTRAINTS:
- ptpsim is lease-agnostic — all leasing/pooling/orchestration stays in your management
  plane; do not push that into the binary.
- The sim is the RESPONDER; the app is the initiator (they share manifests, not state).
- Need richer/teardown-free behavior or an unmodeled op? That's a ptpsim manifest/engine
  change to request upstream — don't reimplement protocol behavior in the sidecar.


```

## Note
This is the SIMULATOR (responder) cutover — distinct from the iOS app's FFI adoption
(`handoff-ios-agent.md`), which consumes the same manifests as an initiator.
