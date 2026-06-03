# Action verbs in the manifest — schema decision

**Status:** PROPOSED (awaiting approval before authoring data)
**Authors:** ptpsim
**Created:** 2026-06-02
**Triggered by:** D3-wire's PCSS shoot-download wire trace (`docs/consults/2026-06-02-ptpsim-action-on-D3-shoot-download.md`) — capture + transfer sequences need a manifest surface that today's `entries[]` doesn't model

## Problem

ptpsim's `Connection.entries[]` model is keyed to **mode transitions**:

```yaml
entries:
  - to: image-transfer
    from: shooting/stills
    steps: [...]    # what to send on the wire to enter image-transfer
```

That fits "enter a mode," but two newly wire-confirmed PCSS sequences don't:

### Capture (3-beat virtual shutter)

```yaml
# Wire-confirmed by Hyper-Utility 14.9-min capture (wirePCSSShootDownload20260523):
- setProp 0xD039 = 0x00010000  → 0x100E(0, 0)
- setProp 0xD039 = 0x00020000  → 0x100E(0, 0)
- setProp 0xD039 = 0x00000001  → 0x100E(0, 0)
```

Camera state before and after is `shooting/stills`. This isn't a mode entry
— it's an **action within a mode** ("fire the shutter").

### Transfer (parameterized triad)

```yaml
# Per handle returned by 0x1007:
- 0x1008(handle)            # getObjectInfo
- 0x100A(handle)            # getThumb, optional
- 0x1009(handle)            # getObject
- 0x100B(handle)            # deleteObject, optional
```

The `handle` is a **runtime value** the client gets from `0x1007`'s response.
ptpsim's existing `StepParam::Runtime { runtime: "<slot>" }` covers this for
*entries* (`runtime: openCaptureTxId` is precedent for `0x1018` in the `app`
→ image-transfer entry) — but `entries` calls the sequence by mode, not by
name. There's nowhere to put a parameterized `getObject(handle)` recipe today.

The same gap exists implicitly on the `app` connection (`0x100E` shutter,
plus the `0x9054/9055/9050/9053` vendor block during image-transfer); for now
those are expressed via `entries[].steps` that depend on connection-side
state. Adding a generic action surface lets all three transports share one
recipe vocabulary.

## Options considered

### Option 1 — Pseudo-modes

Express capture as a mode-entry that re-enters the same mode:

```yaml
modes:
  shooting/stills: ...
  shutter-fired: { isTransient: true }   # not a camera state; a verb result

connections:
  wireless-tether:
    entries:
      - to: shutter-fired
        from: shooting/stills
        # After steps, camera state is implicitly back in shooting/stills.
        steps: [<3-beat D039 sequence>]
```

**Pros:** zero schema change. Uses existing types.
**Cons:** semantically wrong — `shutter-fired` isn't a camera mode, it's an
action outcome. Pollutes the mode graph with non-modes. `detect_mode` would
need to know which modes are transient. Transfer recipes still don't fit
(they're parameterized by `handle`, not a mode entry).

### Option 2 — `verb:` field on `Entry`

Make `Entry.to` optional, add `Entry.verb: Option<String>`. If `verb` is set,
the entry is "named recipe to run while in `mode`":

```yaml
connections:
  wireless-tether:
    entries:
      - verb: shutter
        mode: shooting/stills
        steps: [<3-beat D039 sequence>]
      - verb: getObject
        mode: image-transfer
        params: [handle]
        steps:
          - sendOp: 0x1009
            params: [{ runtime: handle }]
```

**Pros:** small schema change (two new optional fields). Reuses Step grammar.
**Cons:** conflates two concepts (mode entry vs intra-mode action) on the
same type, which complicates `mode_entry` lookup vs `action` lookup at the
query surface. Existing tests for `entries[].to` keep working unchanged, but
the client API has to pick which kind of `Entry` it's looking for.

### Option 3 — New `actions:` block (RECOMMENDED)

Sibling to `entries:`. Named, optionally parameterized step sequences gated
to a mode. Separate concept, separate query method:

```yaml
connections:
  wireless-tether:
    entries: []                     # mode-transitions: PCSS enters implicitly
    actions:
      shutter:                      # verb name (free-form id)
        mode: shooting/stills       # gating
        steps:
          - { setProp: "0xd039", value: 0x00010000 }
          - { sendOp:  "0x100e", params: [0, 0] }
          - { setProp: "0xd039", value: 0x00020000 }
          - { sendOp:  "0x100e", params: [0, 0] }
          - { setProp: "0xd039", value: 0x00000001 }
          - { sendOp:  "0x100e", params: [0, 0] }
      enumerateObjects:
        mode: image-transfer
        # response decoder hint — populates a named slot the next action's
        # params can reference. Existing 0x100A/0x100E response handling
        # already implicitly fills slots for the live-view path.
        steps:
          - { sendOp: "0x1007", params: [0xffffffff, 0], captureAs: handles }
      getObjectInfo:
        mode: image-transfer
        params: [handle]            # runtime slots the caller must bind
        steps:
          - { sendOp: "0x1008", params: [{ runtime: handle }], captureAs: info }
      getObject:
        mode: image-transfer
        params: [handle]
        steps:
          - { sendOp: "0x1009", params: [{ runtime: handle }], captureAs: bytes }
      deleteObject:
        mode: image-transfer
        params: [handle]
        steps:
          - { sendOp: "0x100b", params: [{ runtime: handle }] }
```

**Pros:**
- Honest about the distinction: entries enter modes; actions act within
  them. The query surface stays clean (`action(connection, verb, args) →
  Plan`).
- Parameterized verbs are first-class via `params:` (caller binds; engine
  emits).
- Step grammar already supports `runtime:`-bound params for `0x1018` —
  reused unchanged.
- Same pattern works retroactively for the `app` connection's `0x100E`
  shutter + the reference app vendor-prime block.

**Cons:**
- New typed field on `Connection` (one `BTreeMap<String, Action>`).
- One new `Action` struct (4 fields: `mode`, `params`, `steps`, `evidence`).
- Client-side adoption: client application gets a new method (`action(connection,
  verb, runtimeScope) → Plan`); existing `mode_entry(connection, from, to)`
  unchanged.

## Recommendation

**Option 3.** The "parameterized intra-mode action" concept is genuinely
distinct from "mode entry" and will compound — capture, transfer, vendor-
prime, live-view restart, autofocus probes are all this shape. Adding the
typed field now is one schema change; layering verbs onto `Entry` would
either pollute the type or require this same change later under a different
name.

## What lands when this is approved

1. **Schema (`crates/camera-config/src/model.rs`)**:
   - `pub struct Action { mode: String, params: Vec<String>, steps: Vec<Step>, evidence: Vec<String> }`
   - `Connection.actions: BTreeMap<String, Action>` (defaulted empty).
   - `CameraManifest::action(connection, verb)` query method returning
     `Option<&Action>`.
2. **Data (`packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml`)**:
   - `wireless-tether.actions.{shutter, enumerateObjects, getObjectInfo,
     getThumb, getObject, deleteObject}` per the wire-confirmed D3 sequences.
3. **Tests**: assert the 3-beat shutter values, the runtime-`handle` binding
   in `getObject`, and that `action(wireless-tether, "shutter")` resolves.
4. **INTEGRATION.md** §7 (Golden rules): a one-liner adding "no shutter
   sequences in app source — ask `action(connection, 'shutter')`."

## Open questions for the reviewer

1. **Action-name namespace.** Free-form strings (`shutter`, `getObject`) or
   a closed enum (`Action::Shutter | Action::GetObject | ...`)? Recommend
   free-form — different cameras may have actions ptpsim hasn't seen yet
   (manufacturer-tier extensions), and a closed enum would force a schema
   change per new verb.
2. **Response capture.** The transfer triad needs the engine to take a
   `0x1007` response (a u32 count + u32 handle list) and bind it to a
   named slot. We have `StepParam::Runtime { runtime: <slot> }` for *params*
   but no symmetric `captureAs:` on `Step` for *responses*. Probably needed
   here. Worth adding alongside Action, or defer?
3. **Composability.** Should one action be able to invoke another (e.g. an
   "enumerate then download all" macro)? Recommend NO — keeps actions as
   atomic recipes. Composition is the client's job (per the sans-io model:
   client owns control flow; manifest owns bytes).

— ptpsim
