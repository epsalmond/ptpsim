# RESUME — BLE-MVP CI workstream

> Snapshot at 2026-06-02 evening, before context compaction. The detail any
> post-compact session needs to pick up cleanly.

## Where we are

**The BLE-MVP itself (schema + FFI) is merged.** PR #2 landed all of P0+P1
plus the `transform: { bitOr: … }` schema addition and the uniffi 0.28→0.31
bump. Current `main` ships:

* `packages/camera-config-data/fuji/index.yaml` — manufacturer index +
  family BLE block + GFX100 II signatures, authored from

* `crates/camera-config/src/index/` — typed schema + loader (inheritance
  merge §11.9, GATT-name resolution §11.3, static template substitution
  §11.1, signature precedence §11.7, encoding allowlist §11.2,
  transform allowlist).
* `crates/camera-protocol-ffi/src/mfg_index.rs` — uniffi-exposed
  `from_manufacturer_index` / `recognize` / `establishment` /
  `refine_establishment` (stub).
* `docs/handoff-ios-ble-mvp.md` — handoff for the iOS rewrite agent.

Workspace test count on `main`: **151 passing**, fmt + clippy `-D warnings`
clean.

## What's on `ble-mvp` (currently 9 commits ahead of `main`, not yet PR'd)

The CI side of §11.11 — the xcframework build and publish.

```
a91cb9b ci(ansible): bypass unarchive's BSD-tar check by calling tar directly
ffaa44b ci(ansible): discover brew prefix instead of relying on PATH
4a56e93 ci(python-wheel): platform-tagged Python wheel + Woodpecker release step  ← user landed this in parallel; not part of the xcframework chain
d170f18 ci(ansible): port macOS Woodpecker agent provisioning to Ansible

196aa8b ci: build-xcframework.sh — codify the verified §11.11 recipe
1e5d22d docs(INTEGRATION): pull-model surface + uniffi 0.31 CLI + xcframework recipe link
d1f7aca plan: BLE-MVP §11.11 — recipe verified + updated for uniffi 0.31
f56a7f3 feat(camera-protocol-ffi): uniffi.toml — pin Swift module name to CameraProtocolFFI
```

Pieces these commits drop in place:

* `crates/camera-protocol-ffi/uniffi.toml` — pins the Swift module name
  to `CameraProtocolFFI` (uniffi 0.31 moved this out of CLI flags).
* `ci/build-xcframework.sh` — the verified recipe: 4 `cargo build` slices,
  `lipo` the macOS arches, `uniffi-bindgen generate -l swift`, `xcodebuild
  -create-xcframework`, tarball to `out/dist/`. Spike-verified end-to-end


  Woodpecker macOS agent (service user, rustup + Apple targets, gh CLI
  via Homebrew, agent binary download, LaunchDaemon plist). This was
  ported from a one-shot bash script; the user wanted to adopt Ansible
  now because Docker→VM migration is imminent.
* `.woodpecker.yml` — new `xcframework` step gated on `main` + `push`,
  routes to `platform: darwin` agents, runs the build script, attaches
  the tarball to a GitHub release tagged `sha-<8>` on the merge commit.

Updates to `docs/INTEGRATION.md` and the plan §11.11 reflect the verified
uniffi 0.31 recipe + the new pull-model surface.

## Where we left off (this is the live state when picked up)


server at `192.168.5.228:9000` (the internal LAN IP — not the public

gRPC port 9000). `launchctl print` shows `state = running`, `runs = 1`,
`last exit code = (never exited)`.

Bumps the playbook required to get here:

* `woodpecker_agent_version: "3.10.0"` → `"3.14.1"` — the agent and
  server must match minor versions because Woodpecker bumps the gRPC
  wire version between minor releases (3.10→3.14 was v14→v16). The
  server is 3.14.1; agent now matches.
* Tarball unpack moved into a version-tagged temp dir
  (`/tmp/woodpecker-agent-<version>/`) so future version bumps actually
  replace the binary — the prior non-versioned `creates:` guard would
  skip unpack on re-runs.

Run that worked:

```
ansible-playbook woodpecker-agent.yml \
    -e woodpecker_server=192.168.5.228:9000 \
    -e woodpecker_agent_secret=$WP_SECRET
```

Labels in the live plist: `platform=darwin,arch=amd64,xcode=true`. The
`.woodpecker.yml` `xcframework` step gates on `labels: { platform:
darwin }` only — no `arch` constraint — so this matches.

## The blocking question for the PR

Before the `ble-mvp` follow-up PR can merge:

1. ~~Get the agent talking to the server~~ — DONE (2026-06-02).
2. **Add the `github_token` secret** to the ptpsim repo in Woodpecker
   (Settings → Secrets). Fine-grained PAT on `epsalmond/ptpsim` with
   `contents: write`. The `xcframework` step calls `gh release create` /
   `upload` which needs this.
3. **Open the PR** for the follow-up commits. Merging triggers the first
   xcframework build.

## Next steps in priority order


   `platform=darwin,arch=amd64,xcode=true`.
2. Add `github_token` to Woodpecker repo secrets.
3. Open PR for the `ble-mvp` follow-ups.
4. After merge: monitor the first `xcframework` Woodpecker run; confirm a
   GitHub release with the tarball lands at `sha-<8>`.
5. Tell the iOS team they can pull the artifact.

## Out of scope (queued elsewhere)

* **Terraform for Woodpecker server state** (agent registration token + the
  `github_token` secret declared in HCL) — explicitly deferred.
* **Android (Kotlin) + Python wheel for protocol-mapper** — Task #24.
  Python wheel side partially landed by the user in `4a56e93`; Android side
  is still pending.
* **serde_yaml fork decision** (current `0.9.34+deprecated`) — separate
  session.
* **§11.5 `refine_establishment` real implementation** — currently a stub
  returning `None`. Plan §5 explicitly calls this the graceful-degrade
  contract for the MVP.

## Where the authoritative docs live

* Plan: `docs/plans/ios-rewrite-p0-p1-ble-mvp.md` (§11 is the contract
  tiebreaker; §11.11 has the verified xcframework recipe).
* iOS handoff: `docs/handoff-ios-ble-mvp.md` (already shipped to the iOS
  planning agent).
* Operator analysis the schema was authored from:


* CI design: `.woodpecker.yml` + `ci/build-xcframework.sh` + `ci/ansible/`.
