# Action verbs in the manifest — schema decision

**Status:** APPROVED 2026-06-02 (Option 3 with refinements per reviewer Q1/Q2/Q3 below)
**Authors:** ptpsim
**Created:** 2026-06-02
**Triggered by:** the PCSS shoot-download wire trace (`wirePCSSShootDownload20260523`) — capture + transfer sequences need a manifest surface that today's `entries[]` doesn't model

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

### Option 3 — New `actions:` block (APPROVED)

Sibling to `entries:`. **Closed-vocabulary** (`ActionVerb` enum) keyed map of
parameterized step sequences gated to a mode, with **declared side-effects**
(`triggers:`) so the client can plan UX without camera knowledge:

```yaml
connections:
  wireless-tether:
    entries: []                     # mode-transitions: PCSS enters implicitly
    actions:
      shutter:                      # ActionVerb::Shutter (enum-keyed)
        mode: shooting/stills       # gating
        steps:
          - { setProp: "0xd039", value: 0x00010000 }
          - { sendOp:  "0x100e", params: [0, 0] }
          - { setProp: "0xd039", value: 0x00020000 }
          - { sendOp:  "0x100e", params: [0, 0] }
          - { setProp: "0xd039", value: 0x00000001 }
          - { sendOp:  "0x100e", params: [0, 0] }
        triggers:                   # 1-3 images per press (JPEG / HEIF / RAW)
          - objectsAvailable: { min: 1, max: 3 }
      enumerateObjects:
        mode: image-transfer
        steps:
          - { sendOp: "0x1007", params: [0xffffffff, 0] }
        # Action return value is the decoded response; client iterates it
        # and calls getObjectInfo / getObject per handle. No intra-sequence
        # `captureAs:` needed (see Q2).
      getObjectInfo:
        mode: image-transfer
        params: [handle]            # runtime slots the caller must bind
        steps:
          - { sendOp: "0x1008", params: [{ runtime: handle }] }
      getThumb:
        mode: image-transfer
        params: [handle]
        steps:
          - { sendOp: "0x100a", params: [{ runtime: handle }] }
      getObject:
        mode: image-transfer
        params: [handle]
        steps:
          - { sendOp: "0x1009", params: [{ runtime: handle }] }
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
   - `pub enum ActionVerb { Shutter, EnumerateObjects, GetObjectInfo, GetThumb,
     GetObject, DeleteObject }` — **closed vocabulary**. New verbs require a
     schema PR (same fail-fast as Step verbs).
   - `pub struct ActionEffect` — flat struct mirroring the `Step` pattern
     (one optional field per variant, `deny_unknown_fields`). Fields:
     `objects_available: Option<ObjectsAvailable { min: u32, max: u32 }>` (PCSS
     1-3 / burst-mode-N), `postview_event: Option<PostviewEvent>` (reference app
     0x9022 cleanup), `live_view_stream: Option<LiveViewStream>`.
     `is_well_formed()` asserts exactly one variant per effect.
   - `pub struct Action { mode: String, params: Vec<String>, steps: Vec<Step>,
     triggers: Vec<ActionEffect>, evidence: Vec<String> }`.
   - `Connection.actions: BTreeMap<ActionVerb, Action>` (defaulted empty).
   - `CameraManifest::action(connection, verb)` query method returning
     `Option<&Action>`.
2. **Data (`packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml`)**:
   - `wireless-tether.actions.{shutter, enumerateObjects, getObjectInfo,
     getThumb, getObject, deleteObject}` per the wire-confirmed D3 sequences.
   - `shutter.triggers: [{ objectsAvailable: { min: 1, max: 3 } }]` on
     wireless-tether — the camera makes 1-3 objects available depending on
     the user's JPEG / HEIF / RAW format selection.
3. **Tests**: assert the 3-beat shutter values, the runtime-`handle` binding
   in `getObject`, `triggers: [objectsAvailable]` on the PCSS shutter, and that
   `action(wireless-tether, Shutter)` resolves.
4. **INTEGRATION.md** §7 (Golden rules): a one-liner adding "no shutter
   sequences in app source — ask `action(connection, ActionVerb::Shutter)`;
   read `.triggers` to plan UX side-effects."

## Decisions (Q1/Q2/Q3 from reviewer, 2026-06-02)

### Q1 — Action-name namespace: **enum, not free-form**

Original recommendation was free-form to allow new manufacturer-tier verbs
without schema PRs. **Conceded:** that argument conflated two things —
manifest-data growth (a new camera's bytes for an existing verb) is fine as
data; action-vocab growth (a genuinely new concept like "focus bracket sweep"
or "panorama stitch") requires app-side reasoning anyway, so gating it on a
code change is the *right* behavior. Free-form would let manifest data drift
ahead of client awareness. Enum + fail-fast on unknown variant matches the
Step-verb allowlist pattern.

### Q2 — Response capture (`captureAs:` on Step): **defer**

Original proposal included `captureAs:` on `Step` to bind decoded responses
to a named slot a later step could reference via `runtime:`. **Deferred**:
all currently-modeled actions are one observable op (plus optional setprop
scaffolding). Multi-step actions on the table don't have intra-sequence data
flow — `enumerateObjects` returns handles; the **client** iterates them and
calls `getObjectInfo(h)`. **Action return value = response slot.** Revisit
if a future action genuinely needs to plumb a response field into a later
step's param within one recipe.

### Q3 — Composability: **declared side-effects, not action-calling-action**

Original framing was "should one action invoke another"; the reviewer
sharpened it. On wireless-tether, the camera makes captured objects available
after `shutter`; the app needs to poll/download and show progress without
knowing the connection-specific transfer choreography.

**Decision**: `Action.triggers: Vec<ActionEffect>` declares what arrives
after the action completes. Engine does **not** act on it — pure declaration
for the client to plan UX:

| connection | `Shutter.triggers` | app behavior |
|---|---|---|
| `wireless-tether` | `[{ objectsAvailable: { min: 1, max: 3 } }]` | poll the object queue after `Shutter`; budget download timeout / progress UI for up to `max` arrivals; early-exit when the app's own format-selection state predicts the exact count |
| `app` | `[{ postviewEvent: {} }]` (when modeled) | wait for the event then prompt user to switch to Get |
| `ble` (remote-trigger, when modeled) | `[]` | fire and forget |

This is *not* control flow in the engine sense. It's a **Recipe** — same
sans-io model: client owns control flow; manifest declares bytes AND
post-conditions.

— ptpsim
