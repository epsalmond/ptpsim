---
description: Reduced static client identities, model-scoped candidate ledgers, digest-bound catalog intake and initiator validation boundaries.
status: reference
read-when: Adding unverified client catalog identities or consuming static candidates in an initiator.
---

# Static client catalog candidates

`camera-static-candidates/v1` records reduced protocol identities extracted
from a shipped client. It does not represent a camera observation. Every row
has `status: unverified`. No camera firmware scope is inferred from an
application version.

Each document identifies the application, version, plugin and SHA-256 in
`source`, the `manufacturer`, and the `model`. A null model means that the
plugin's camera identity is unknown. Its rows cannot establish support on a
named camera. `candidates` contains one disposition per full property or
operation code, plus coverage rows for compound behavior. Codes retain four
hexadecimal digits.

Rows retain all `nativeNames`, `accessorRoles`, and `typeHints`. A unique client
datatype hint may also appear as `expectedType`. These are client expectations,
not descriptor observations. `catalogName` is a deterministic candidate name;
the code suffix distinguishes multiple properties with the same accessor
family. `reason` records conflicts and missing facts. `evidence` contains
public reduced-fact references. A `request` contains an opcode, numeric
parameter list, and `dataPhase` (`none`, `in`, or `out`) only when that complete
shape is established. Optional `payloadHex` records known data-out bytes.
Dynamic object handles, unknown payloads and unsafe recipes remain explicit
gaps. A request does not authorize its execution.

The dispositions are:

- `addCatalog`: a missing property in a named model's catalog.
- `nameCandidate`: an existing raw property or operation with a candidate client identity.
- `preserveReviewed`: an existing semantic identity remains authoritative.
- `unknownModel`: the property or operation belongs to an unidentified plugin.
- `coverageOnly`: compound behavior or an operation retained for validation.

Run `camera-config-generate propose --static-candidates FILE --output FILE`
to create a proposal. Every row becomes an independent digest-bound assertion.
Review uses the existing `camera-config-review/v1` decision format, with one
decision per assertion. `camera-config-generate apply` checks the digest and
model before applying accepted assertions.

Accepted `addCatalog` properties enter as `catalogOnly`, with no access,
descriptor, current/default value, legal values, controls or simulator effects.
The catalog stores no static datatype as an observed property type.
Accepted `nameCandidate` rows replace only generated raw property or operation names. Existing
reviewed names, types and behavior are preserved. Generated YAML comments begin
with `unverified:` and identify static additions or candidate names. The
source ledger and reviewed proposal retain all alternate identities and client
type hints. Operation names retain their existing execution classification. Other static
operation and coverage assertions remain in those artifacts; they do not create
executable simulator behavior.

Initiator validation consumes the candidate document directly. Descriptor and
value reads can compare actual camera results with the client hints. Operations
and writes require an explicit bounded verification plan and the initiator's
control opt-in. Unknown-model rows require an explicit scope override and
retain their unverified status. Only a later camera observation may establish
support, access, legal values or effects through normal observation intake.
