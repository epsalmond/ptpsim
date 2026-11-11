# GFX100 II firmware 2.30 audit reconciliation

This reconciles [audit issue #539](https://github.com/epsalmond/ptpsim/issues/539)
with the committed public evidence and manifest contract. The camera scope is
FUJIFILM GFX100 II firmware 2.30. Static firmware findings and measured elapsed
times do not extend runtime behavior beyond their established connection and
state.

## Operation inventories

`../gfx100ii.yaml` contains 33 authored executable operations.
`../gfx100ii.consolidated.yaml` contains those operations plus 14
`advertisedOnly` rows from the reviewed observation corpus. Consumers needing
the inventory must load the consolidated manifest. The additional codes are
`0x1001`, `0x1004` through `0x1006`, `0x100C`, `0x100D`, `0x100F`, `0x1017`,
`0x900C`, `0x900D`, `0x901D`, `0x9801`, `0x9802`, and `0x9805`.

The eight `probe/2026-05-27-ptp-evidence-*.jsonl` bundles establish these
positive inventory scopes:

| Connection | Persona | Advertised operations |
|---|---|---|
| USB | shooting/stills | 24 |
| USB | shooting/stills/pc-priority | 24 |
| USB | shooting/video | 24 |
| USB | raw-conversion/backup | 20 |
| USB | card-reader | 25 |
| USB | tether-auto | 25 |
| wireless-tether | shooting/stills | 24 |
| wireless-tether | shooting/video | 24 |

The stills/video operation sets match within both USB and wireless-tether.
Both connections have 104 property descriptors observed only in video and 98
observed only in stills. These descriptor observations do not establish a
callable movie recording or transfer workflow.

The consolidated `observedScopes` for `0x9018` already include USB stills,
PC-priority, and video, plus wireless-tether stills and video. Its authored
callable scope remains wireless-tether shooting/stills. `0x101B` and `0x9803`
already record USB card-reader and tether-auto advertisement while retaining
their app image-transfer behavior. `0x101B` also retains app reserved-transfer
support. Advertisement does not prove the app chunking or object-size contract
on USB.

The 25-operation storage snapshots cannot be replaced by the audit's reported
23-operation session-default list without explaining the selector/session
scope. The reported hidden 22-operation list and `0x9008`/`0x901B` dispatch
arms have no matching committed wire observation. The separate app-mode
identity reported for `0x9008` also needs a connection-qualified contract.
[Issue #540](https://github.com/epsalmond/ptpsim/issues/540) owns that evidence
question. Static dispatch arms do not define safe calls or prove universal
rejection of other codes.

App command codes such as `0x9022`, `0x9026`/`0x9027`, `0x902B` through
`0x902E`, and the authored `0x9050` family retain their existing app-only
behavior. They are absent from these USB/PCSS advertisements. Their internal
menu-command or dispatch-table classification does not remove that separate
app surface. The unresolved app `0x902B` bulk-descriptor behavior remains
owned by [issue #537](https://github.com/epsalmond/ptpsim/issues/537).

The absence of `0x9804` and `0x9806` through `0x980B` from these observations
does not prove a response on every connection. The reported `0x2005` standard
stubs (`0x1010` through `0x1013`, `0x1019`, `0x101A`) remain unmodeled rather
than receiving new executable rows.

## Live view and descriptors

The wireless-tether `startLiveView` action already writes `0xD1BC=2` and sends
`0x101C(0,0)`. Its `pollLiveView` action requests one `0x9018` payload; callers
own repetition. The app connection already uses a separate push socket after
successful `0x101C`. These are distinct delivery contracts.

The consolidated catalog already contains camera-sourced `0xD173` enum
`[1,3]` and `0xD174` enum `[1,2,3]`, both `catalogOnly`. Numeric descriptors do
not establish SDK quality labels or a stream effect from a successful write.
The reported `0xD173` default/current values 1/3 and pull-stream `0xD174=1`
writes remain session observations; they do not seed global simulator values.

The audit reports standalone 1024x768 baseline JPEG pull frames, setup-specific
wired/Wi-Fi polling rates, a separate 640x480 app push stream, and no app push
effect from writes to `0xD173`, `0xD174`, `0xD1BC`, or `0xD23C`. It also
reports unarmed USB `0x9018` returning `0x2002`, app polling timing out with
session loss, and an object-based USB live-view path at handle `0x80000001`.
These new reports need public reduced transactions and frame evidence before
they change simulator responses, USB workflows, frame geometry, or property
effects. [Issue #541](https://github.com/epsalmond/ptpsim/issues/541) owns the
transport/state reconciliation and remaining stream-property observations.
PCSS click modes remain under [issue #297](https://github.com/epsalmond/ptpsim/issues/297),
and the control-effect matrix remains under [issue #193](https://github.com/epsalmond/ptpsim/issues/193).

## Timeouts

The audit reports event-wait values 10000/180000 with units only presumed to be
milliseconds, a 7000 ms PCSS watchdog, phase-specific receives of
200/10/infinite, loop/error delays of 1000/500 ms, five 300 ms DHCP retries,
and a 100 ms accept poll. These values need qualified units, phase, reset, and
expiry semantics before becoming camera-side deadlines.

Existing host discovery retries, connection budgets, and activity duration
estimates serve separate purposes. They are not replaced by internal firmware
scheduling values. The reported approximately 26-second elapsed receive
timeout is not an established camera constant.
[Issue #542](https://github.com/epsalmond/ptpsim/issues/542) owns the missing
timeout semantics.

## BLE and network implementation

The audit reports a legacy-only BLE firmware build, BLE execution on the
camera's RTOS, and an open camera AP implemented by its network processor.
These are scoped implementation observations. They do not define new PTP
operations, justify removing other models' BLE families, or establish a new
consumer recovery policy. Existing recognition and app endpoint data remain
applicable; camera-side shutdown still requires an independent readiness
observation.

## Reproduction

The existing proposal and review in this directory remain the digest-bound
authority for the consolidated inventory. No new manifest data is required by
the reconciled advertisement or descriptor claims. The corpus tests regenerate
the proposal from committed observations, verify reviewed apply, and check the
captured inventory scopes. FFI seam coverage verifies that consumers can
enumerate the additional rows while their execution remains unavailable.
